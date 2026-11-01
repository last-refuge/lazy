//! Webhook connectors (llm-docs/DECISIONS.md, D34): Linear and Jira post their activity into the
//! channels that follow it, from a webhook an admin makes in the provider's own settings.
//!
//! # Connecting
//!
//! An admin opens Integrations, chooses the provider, and connects: Lazy makes the connection's
//! address (`{public-url}/connectors/{provider}/{id}`) and a bot named after the provider. The
//! admin pastes the address into the provider's webhook form and joins up the signing secret:
//! Linear shows one, which is pasted into Lazy; Jira takes one, which Lazy makes and shows once.
//! The secret is a credential (D31), so a server without a credentials key cannot connect.
//!
//! # Receiving
//!
//! A delivery to the address is checked before anything else: the connection must be live and
//! have a secret, the signature must match (constant time), and a Linear delivery must be fresh.
//! Then it is reduced to a [`Summary`] (the provider's module decides the kind, the team or
//! project, and the message) and queued as a `connector.event` job deduplicated by the
//! provider's delivery id, so a redelivery is not posted twice. Both providers count anything
//! other than `200` as a failure, so queued and already-received both answer `200`.
//!
//! # Posting
//!
//! The job posts the message, as the bot, into every channel whose subscription follows that
//! kind for that team or project (or for everything). The operation id is derived from the
//! subscription and the job, so a retried job never posts twice in one channel.

mod jira;
mod linear;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lazy_domain::connector::ConnectorProvider;
use lazy_domain::{
    ChannelName, ConnectorSubscriptionId, ConversationId, ConversationKind, MessageDocument,
    OperationId, Timestamp, UserId, WorkspaceId,
};
use lazy_protocol::{
    ConnectorConnection, ConnectorDelivery, ConnectorStatusResponse, ConnectorSubscription,
    CreateConnectorSubscriptionRequest, ErrorCode, Event, GithubDeliveryState, SendMessageRequest,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::credentials::{self, CredentialId, Secret};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::github::{create_bot, manage_error, require_admin};
use crate::jobs::{self, JobContext, NewJob, Outcome};
use crate::logging::Describe;
use crate::messages;

/// The job that posts one delivery.
pub const EVENT_JOB: &str = "connector.event";
/// The largest delivery read.
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
/// The credential kind of a signing secret.
const SECRET_KIND: &str = "connector.signing_secret";
/// How many recent deliveries the status lists.
const DELIVERIES_LISTED: i64 = 50;
/// The shortest and longest signing secret accepted from a provider.
const SECRET_CHARS: std::ops::RangeInclusive<usize> = 8..=256;

/// What a provider's delivery comes to: decided by the provider's module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summarized {
    /// The kind of activity, one of the provider's [`ConnectorProvider::kinds`].
    pub kind: &'static str,
    /// The teams or projects it belongs to (upper-case keys); empty belongs to none in
    /// particular, and reaches only subscriptions that follow everything.
    pub scopes: Vec<String>,
    /// A few words for the delivery list ("ENG-12 status change").
    pub label: String,
    /// The message, Markdown with everything from the provider escaped.
    pub text: String,
}

/// What is kept of a delivery: where it applies and the message. Nothing else of the payload is
/// stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub provider: String,
    pub connector_id: Uuid,
    pub kind: String,
    pub scopes: Vec<String>,
    pub label: String,
    pub text: String,
}

/// What a provider's module must say about a delivery.
struct Received {
    /// The provider's id for the delivery, the same across its retries.
    delivery: String,
    /// `None` posts nothing (an event no channel can follow).
    summary: Option<Summarized>,
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn plain(status: StatusCode, text: &'static str) -> Response {
    (status, text).into_response()
}

/// Whether `hex` (lower or upper case) is the HMAC-SHA256 of `body` under `secret`, in constant
/// time.
pub fn hmac_hex_ok(secret: &[u8], body: &[u8], hex: Option<&str>) -> bool {
    crate::github::signature_ok(
        secret,
        body,
        hex.map(|hex| format!("sha256={hex}")).as_deref(),
    )
}

/// A string at `path` in `value`, or "".
fn s<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .fold(value, |v, key| &v[*key])
        .as_str()
        .unwrap_or_default()
}

fn provider_of(name: &str) -> Result<ConnectorProvider, AppError> {
    ConnectorProvider::from_name(name).ok_or_else(|| AppError::not_found("no such connector"))
}

fn webhook_url(state: &AppState, provider: ConnectorProvider, id: Uuid) -> String {
    format!(
        "{}/connectors/{}/{id}",
        state.integrations.public_url,
        provider.as_str()
    )
}

// ----- Receiving ----------------------------------------------------------------------------

/// The live connection a delivery is addressed to.
#[derive(sqlx::FromRow)]
struct LiveRow {
    workspace_id: Uuid,
    secret_id: Option<Uuid>,
}

/// `POST /connectors/{provider}/{id}`: one delivery from a provider.
pub async fn receive(
    state: &AppState,
    provider: &str,
    connector: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Response {
    match receive_inner(state, provider, connector, headers, body).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn receive_inner(
    state: &AppState,
    provider: &str,
    connector: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Response, AppError> {
    let (Some(provider), Ok(connector)) = (
        ConnectorProvider::from_name(provider),
        Uuid::parse_str(connector),
    ) else {
        return Ok(plain(StatusCode::NOT_FOUND, "unknown connector"));
    };
    let mut conn = state.pool.acquire().await?;
    let live: Option<LiveRow> = sqlx::query_as(
        "SELECT workspace_id, secret_id FROM connectors \
         WHERE id = $1 AND provider = $2 AND disconnected_at IS NULL",
    )
    .bind(connector)
    .bind(provider.as_str())
    .fetch_optional(&mut *conn)
    .await?;
    let Some(live) = live else {
        return Ok(plain(StatusCode::NOT_FOUND, "unknown connector"));
    };
    let Some(secret_id) = live.secret_id else {
        return Ok(plain(StatusCode::UNAUTHORIZED, "no signing secret yet"));
    };
    let secret = credentials::load(
        &mut conn,
        &state.integrations.credentials,
        CredentialId(secret_id),
    )
    .await?;
    let signed = match provider {
        ConnectorProvider::Linear => linear::signature_ok(secret.expose(), headers, body),
        ConnectorProvider::Jira => jira::signature_ok(secret.expose(), headers, body),
        ConnectorProvider::Unknown => false,
    };
    drop(secret);
    if !signed {
        tracing::warn!(
            connector_id = %connector,
            provider = provider.as_str(),
            "a connector delivery's signature did not match"
        );
        return Ok(plain(StatusCode::UNAUTHORIZED, "signature does not match"));
    }
    let payload: Value =
        serde_json::from_slice(body).map_err(|_| AppError::invalid("the body must be JSON"))?;
    let received = match provider {
        ConnectorProvider::Linear => {
            if !linear::fresh(&payload, OffsetDateTime::now_utc()) {
                return Ok(plain(StatusCode::UNAUTHORIZED, "the delivery is too old"));
            }
            linear::receive(headers, &payload)
        }
        ConnectorProvider::Jira => jira::receive(headers, &payload),
        ConnectorProvider::Unknown => return Ok(plain(StatusCode::NOT_FOUND, "unknown connector")),
    };
    let delivery = if received.delivery.is_empty() || received.delivery.len() > 100 {
        // No usable id: the body itself identifies a redelivery.
        hex(&Sha256::digest(body))
    } else {
        received.delivery
    };
    let Some(summarized) = received.summary else {
        return Ok(plain(StatusCode::OK, "ignored"));
    };
    let summary = Summary {
        provider: provider.as_str().to_owned(),
        connector_id: connector,
        kind: summarized.kind.to_owned(),
        scopes: summarized.scopes,
        label: summarized.label,
        text: summarized.text,
    };
    let job = NewJob::new(
        EVENT_JOB,
        serde_json::to_value(&summary).unwrap_or(Value::Null),
    )
    .workspace(WorkspaceId::from_uuid(live.workspace_id))
    .dedupe(format!("{connector}:{delivery}"))
    .max_attempts(6);
    let enqueued = jobs::enqueue(&mut conn, job).await?;
    if enqueued.created {
        state.integrations.jobs.wake();
        Ok(plain(StatusCode::OK, "queued"))
    } else {
        Ok(plain(StatusCode::OK, "already received"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ----- Posting ------------------------------------------------------------------------------

/// Posts one delivery into every channel that follows it. The job's handler.
pub async fn handle_event(ctx: JobContext) -> Outcome {
    let summary: Summary = match serde_json::from_value(ctx.payload.clone()) {
        Ok(summary) => summary,
        Err(_) => return Outcome::fail("the stored delivery could not be read"),
    };
    match post(&ctx.pool, ctx.id, &summary).await {
        Ok(()) => Outcome::Done,
        Err(PostError::Disconnected) => Outcome::fail("the connector is no longer connected"),
        Err(PostError::Retry(error)) => Outcome::retry(error),
    }
}

enum PostError {
    Disconnected,
    Retry(String),
}

impl From<AppError> for PostError {
    fn from(error: AppError) -> Self {
        Self::Retry(format!("the database failed ({})", error.describe()))
    }
}

async fn post(pool: &PgPool, delivery: i64, summary: &Summary) -> Result<(), PostError> {
    let bot: Option<Uuid> = sqlx::query_scalar(
        "SELECT bot_user_id FROM connectors WHERE id = $1 AND disconnected_at IS NULL",
    )
    .bind(summary.connector_id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::from)?;
    let Some(bot) = bot.map(UserId::from_uuid) else {
        return Err(PostError::Disconnected);
    };
    let targets: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT id, conversation_id FROM connector_subscriptions \
         WHERE connector_id = $1 AND (scope = '' OR scope = ANY($2)) AND $3 = ANY(events) \
         ORDER BY id",
    )
    .bind(summary.connector_id)
    .bind(&summary.scopes)
    .bind(&summary.kind)
    .fetch_all(pool)
    .await
    .map_err(AppError::from)?;
    let document = MessageDocument::from_markdown(&summary.text)
        .map_err(|_| PostError::Retry("the message could not be made".to_owned()))?;
    let mut failure = None;
    for (subscription, conversation) in targets {
        let request = SendMessageRequest {
            // The same delivery to the same subscription is the same message, however often the
            // job runs.
            operation_id: OperationId::from_uuid(Uuid::new_v5(
                &subscription,
                delivery.to_string().as_bytes(),
            )),
            document: document.clone(),
            thread_root: None,
            also_in_channel: false,
            attachment_ids: Vec::new(),
        };
        match messages::send_message(pool, bot, ConversationId::from_uuid(conversation), request)
            .await
        {
            Ok(_) => {}
            // The channel can no longer take it (archived, the bot removed, gone): nothing to
            // retry.
            Err(AppError::Api(api))
                if matches!(
                    api.code,
                    ErrorCode::Forbidden | ErrorCode::NotFound | ErrorCode::Conflict
                ) => {}
            Err(other) => failure = Some(other),
        }
    }
    match failure {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

// ----- Managing -----------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct ConnectionRow {
    id: Uuid,
    bot_user_id: Uuid,
    secret_id: Option<Uuid>,
    created_by: Uuid,
    created_at: OffsetDateTime,
}

async fn live_connection(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    provider: ConnectorProvider,
) -> Result<Option<ConnectionRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, bot_user_id, secret_id, created_by, created_at FROM connectors \
         WHERE workspace_id = $1 AND provider = $2 AND disconnected_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(provider.as_str())
    .fetch_optional(conn)
    .await?)
}

/// The connector's connection, the subscriptions the caller can see, and recent deliveries.
/// Admins and owners only.
pub async fn status(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: &str,
) -> Result<ConnectorStatusResponse, AppError> {
    let provider = provider_of(provider)?;
    let mut conn = state.pool.acquire().await?;
    require_admin(&mut conn, workspace, actor).await?;
    let connection = live_connection(&mut conn, workspace, provider).await?;
    let subscriptions = match &connection {
        Some(row) => subscriptions_visible(&mut conn, row.id, actor).await?,
        None => Vec::new(),
    };
    let deliveries: Vec<(i64, Value, String, i32, Option<String>, OffsetDateTime)> =
        sqlx::query_as(
            "SELECT id, payload, state, attempts, last_error, created_at FROM jobs \
             WHERE workspace_id = $1 AND kind = $2 AND payload->>'provider' = $3 \
             ORDER BY id DESC LIMIT $4",
        )
        .bind(workspace.as_uuid())
        .bind(EVENT_JOB)
        .bind(provider.as_str())
        .bind(DELIVERIES_LISTED)
        .fetch_all(&mut *conn)
        .await?;
    Ok(ConnectorStatusResponse {
        provider,
        available: state.integrations.credentials.enabled(),
        connection: connection.map(|row| ConnectorConnection {
            webhook_url: webhook_url(state, provider, row.id),
            secret_set: row.secret_id.is_some(),
            bot_user_id: UserId::from_uuid(row.bot_user_id),
            created_by: UserId::from_uuid(row.created_by),
            created_at: Timestamp::from(row.created_at),
        }),
        subscriptions,
        deliveries: deliveries
            .into_iter()
            .map(
                |(id, payload, state, attempts, last_error, created_at)| ConnectorDelivery {
                    id,
                    summary: s(&payload, &["label"]).to_owned(),
                    scope: payload["scopes"]
                        .as_array()
                        .and_then(|scopes| scopes.first())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    state: match state.as_str() {
                        "queued" => GithubDeliveryState::Queued,
                        "running" => GithubDeliveryState::Running,
                        "succeeded" => GithubDeliveryState::Posted,
                        "failed" => GithubDeliveryState::Failed,
                        "unknown" => GithubDeliveryState::Unknown,
                        _ => GithubDeliveryState::Other,
                    },
                    attempts: u32::try_from(attempts).unwrap_or(0),
                    last_error,
                    received_at: Timestamp::from(created_at),
                },
            )
            .collect(),
    })
}

/// Connects the provider: makes the address and the bot. Connected already answers the existing
/// connection. `unavailable` without a credentials key, since the signing secret is a credential.
pub async fn connect(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: &str,
) -> Result<ConnectorStatusResponse, AppError> {
    let provider_kind = provider_of(provider)?;
    {
        let mut conn = state.pool.acquire().await?;
        require_admin(&mut conn, workspace, actor).await?;
    }
    if !state.integrations.credentials.enabled() {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "connectors need this server's credentials key (LAZY_CREDENTIALS_KEY); ask whoever \
             runs it to set one",
        ));
    }
    let mut tx = state.pool.begin().await?;
    // Two admins connecting at once: the second waits, then finds the first's connection.
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.as_uuid())
        .execute(&mut *tx)
        .await?;
    if live_connection(&mut tx, workspace, provider_kind)
        .await?
        .is_none()
    {
        let bot = create_bot(&mut tx, workspace, provider_kind.label()).await?;
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO connectors (id, workspace_id, provider, bot_user_id, created_by) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(workspace.as_uuid())
        .bind(provider_kind.as_str())
        .bind(bot.id.as_uuid())
        .bind(actor.as_uuid())
        .execute(&mut *tx)
        .await?;
        tracing::info!(
            workspace_id = %workspace,
            connector_id = %id,
            provider = provider_kind.as_str(),
            "connector connected"
        );
    }
    tx.commit().await?;
    status(state, actor, workspace, provider).await
}

/// Seals `secret` as the connection's signing secret, replacing any before it.
async fn store_secret(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: ConnectorProvider,
    secret: &Secret,
) -> Result<(), AppError> {
    let mut tx = state.pool.begin().await?;
    require_admin(&mut tx, workspace, actor).await?;
    let row: Option<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, secret_id FROM connectors \
         WHERE workspace_id = $1 AND provider = $2 AND disconnected_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(provider.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, current)) = row else {
        return Err(AppError::conflict("connect it before setting its secret"));
    };
    let keys = &state.integrations.credentials;
    match current {
        Some(existing) => {
            credentials::replace(&mut tx, keys, CredentialId(existing), secret).await?
        }
        None => {
            let stored = credentials::store(&mut tx, keys, workspace, SECRET_KIND, secret).await?;
            sqlx::query("UPDATE connectors SET secret_id = $2 WHERE id = $1")
                .bind(id)
                .bind(stored.0)
                .execute(&mut *tx)
                .await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Sets the signing secret the provider made (Linear shows one on the webhook's page).
pub async fn set_secret(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: &str,
    secret: &str,
) -> Result<(), AppError> {
    let provider = provider_of(provider)?;
    let secret = secret.trim();
    if !SECRET_CHARS.contains(&secret.chars().count())
        || secret.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(AppError::invalid(
            "paste the whole signing secret: 8 to 256 characters, no spaces",
        ));
    }
    store_secret(
        state,
        actor,
        workspace,
        provider,
        &Secret::new(secret.as_bytes().to_vec()),
    )
    .await
}

/// Makes a new signing secret for a provider whose form takes one (Jira), stores it, and answers
/// it once.
pub async fn generate_secret(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: &str,
) -> Result<String, AppError> {
    let provider = provider_of(provider)?;
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(format!("no randomness: {e}")))?;
    let text = hex(&bytes);
    store_secret(
        state,
        actor,
        workspace,
        provider,
        &Secret::new(text.as_bytes().to_vec()),
    )
    .await?;
    Ok(text)
}

#[derive(sqlx::FromRow)]
struct SubscriptionRow {
    id: Uuid,
    conversation_id: Uuid,
    channel_name: Option<String>,
    scope: String,
    events: Vec<String>,
    created_by: Uuid,
    created_at: OffsetDateTime,
}

impl SubscriptionRow {
    fn into_dto(self) -> Result<ConnectorSubscription, AppError> {
        Ok(ConnectorSubscription {
            id: ConnectorSubscriptionId::from_uuid(self.id),
            conversation_id: ConversationId::from_uuid(self.conversation_id),
            channel_name: self
                .channel_name
                .as_deref()
                .and_then(|name| ChannelName::parse(name).ok())
                .ok_or_else(|| AppError::internal("a subscription's channel has no name"))?,
            scope: self.scope,
            events: self.events,
            created_by: UserId::from_uuid(self.created_by),
            created_at: Timestamp::from(self.created_at),
        })
    }
}

const SUBSCRIPTION_COLUMNS: &str = "s.id, s.conversation_id, c.name AS channel_name, s.scope, \
     s.events, s.created_by, s.created_at";

async fn subscriptions_visible(
    conn: &mut PgConnection,
    connector: Uuid,
    actor: UserId,
) -> Result<Vec<ConnectorSubscription>, AppError> {
    let rows: Vec<SubscriptionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Safe: SUBSCRIPTION_COLUMNS is a constant.
        "SELECT {SUBSCRIPTION_COLUMNS} \
         FROM connector_subscriptions s JOIN conversations c ON c.id = s.conversation_id \
         WHERE s.connector_id = $1 \
           AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
                (SELECT 1 FROM conversation_members cm \
                 WHERE cm.conversation_id = c.id AND cm.user_id = $2)) \
         ORDER BY c.name, s.scope"
    )))
    .bind(connector)
    .bind(actor.as_uuid())
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(SubscriptionRow::into_dto).collect()
}

/// Subscribes a channel to a team's or project's activity, or changes what it follows. The bot
/// joins the channel.
pub async fn subscribe(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
    provider: &str,
    request: CreateConnectorSubscriptionRequest,
) -> Result<ConnectorSubscription, AppError> {
    let provider = provider_of(provider)?;
    let scope = provider.normalize_scope(&request.scope).ok_or_else(|| {
        AppError::invalid(format!(
            "that is not a {} key (like {}); leave it empty to follow everything",
            provider.scope_label().to_lowercase(),
            provider.scope_example()
        ))
    })?;
    let mut events: Vec<String> = request
        .events
        .into_iter()
        .filter(|event| provider.kind(event).is_some())
        .collect();
    events.sort_unstable();
    events.dedup();
    if events.is_empty() {
        return Err(AppError::invalid(
            "choose at least one kind of activity to follow",
        ));
    }
    let (mut tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Exclusive).await?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    if loaded.facts.kind != ConversationKind::Channel {
        return Err(AppError::invalid(format!(
            "{} posts into channels only",
            provider.label()
        )));
    }
    if loaded.facts.archived {
        return Err(AppError::conflict("this channel is archived"));
    }
    let Some(connection) = live_connection(&mut tx, loaded.workspace_id, provider).await? else {
        return Err(AppError::conflict(format!(
            "connect {} before choosing channels",
            provider.label()
        )));
    };
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO connector_subscriptions \
             (id, connector_id, workspace_id, conversation_id, scope, events, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (connector_id, conversation_id, scope) DO UPDATE SET events = EXCLUDED.events \
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(connection.id)
    .bind(loaded.workspace_id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(&scope)
    .bind(&events)
    .bind(actor.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    let row: SubscriptionRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Safe: SUBSCRIPTION_COLUMNS is a constant.
        "SELECT {SUBSCRIPTION_COLUMNS} \
         FROM connector_subscriptions s JOIN conversations c ON c.id = s.conversation_id \
         WHERE s.id = $1"
    )))
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    let bot = UserId::from_uuid(connection.bot_user_id);
    let added = conversations::add_members(&mut tx, conversation, &[bot]).await?;
    if !added.is_empty() {
        let dto = conversations::conversation_dto(&mut tx, conversation).await?;
        events::append_all(
            &mut tx,
            loaded.workspace_id,
            vec![PendingEvent {
                kind: "member_added",
                conversation_id: Some(conversation),
                operation_id: None,
                audience: Audience::Members,
                event: Event::MemberAdded {
                    conversation: dto,
                    user_id: bot,
                },
            }],
        )
        .await?;
    }
    tx.commit().await?;
    row.into_dto()
}

/// Removes a subscription; the bot leaves the channel when nothing there follows the provider any
/// more. Gone already is a quiet success; one in a channel the caller cannot read is
/// `not_found`.
pub async fn unsubscribe(
    state: &AppState,
    actor: UserId,
    id: ConnectorSubscriptionId,
) -> Result<(), AppError> {
    let found: Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT conversation_id, connector_id FROM connector_subscriptions WHERE id = $1",
    )
    .bind(id.as_uuid())
    .fetch_optional(&state.pool)
    .await?;
    let Some((conversation, connector)) = found else {
        return Ok(());
    };
    let conversation = ConversationId::from_uuid(conversation);
    let (mut tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Exclusive).await?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    sqlx::query("DELETE FROM connector_subscriptions WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    let remaining: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM connector_subscriptions \
                        WHERE conversation_id = $1 AND connector_id = $2)",
    )
    .bind(conversation.as_uuid())
    .bind(connector)
    .fetch_one(&mut *tx)
    .await?;
    if !remaining {
        let bot: Option<Uuid> =
            sqlx::query_scalar("SELECT bot_user_id FROM connectors WHERE id = $1")
                .bind(connector)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(bot) = bot {
            conversations::remove_and_announce(&mut tx, &loaded, UserId::from_uuid(bot)).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Disconnects the provider: the secret is revoked, deliveries are refused from now on, the
/// subscriptions go, and the bot leaves every channel. The webhook stays in the provider's
/// settings until an admin deletes it there. Not connected is a quiet success.
pub async fn disconnect(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    provider: &str,
) -> Result<(), AppError> {
    let provider = provider_of(provider)?;
    {
        let mut conn = state.pool.acquire().await?;
        require_admin(&mut conn, workspace, actor).await?;
    }
    let mut tx = state.pool.begin().await?;
    let live: Option<(Uuid, Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id, bot_user_id, secret_id FROM connectors \
         WHERE workspace_id = $1 AND provider = $2 AND disconnected_at IS NULL FOR UPDATE",
    )
    .bind(workspace.as_uuid())
    .bind(provider.as_str())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, bot, secret)) = live else {
        return Ok(());
    };
    if let Some(secret) = secret {
        credentials::revoke(&mut tx, CredentialId(secret)).await?;
    }
    sqlx::query("UPDATE connectors SET disconnected_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let mut channels: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM connector_subscriptions WHERE connector_id = $1 RETURNING conversation_id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    // The bot leaves each channel it was in, loaded as the bot (a member of each).
    let bot = UserId::from_uuid(bot);
    channels.sort_unstable();
    channels.dedup();
    for channel in channels {
        let conversation = ConversationId::from_uuid(channel);
        let Ok((mut tx, loaded)) =
            conversations::begin_visible(&state.pool, bot, conversation, Lock::Exclusive).await
        else {
            continue;
        };
        conversations::remove_and_announce(&mut tx, &loaded, bot).await?;
        tx.commit().await?;
    }
    tracing::info!(
        workspace_id = %workspace,
        connector_id = %id,
        provider = provider.as_str(),
        "connector disconnected"
    );
    Ok(())
}

/// Runs a failed or unknown delivery again. Admins and owners of its workspace.
pub async fn retry_delivery(
    state: &AppState,
    actor: UserId,
    delivery: i64,
) -> Result<(), AppError> {
    let workspace: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT workspace_id FROM jobs WHERE id = $1 AND kind = $2")
            .bind(delivery)
            .bind(EVENT_JOB)
            .fetch_optional(&state.pool)
            .await?;
    let Some(Some(workspace)) = workspace else {
        return Err(AppError::not_found("delivery not found"));
    };
    {
        let mut conn = state.pool.acquire().await?;
        require_admin(&mut conn, WorkspaceId::from_uuid(workspace), actor)
            .await
            .map_err(|error| match error {
                AppError::Api(api) if api.code == ErrorCode::NotFound => {
                    AppError::not_found("delivery not found")
                }
                other => other,
            })?;
    }
    if jobs::retry(&state.pool, delivery).await? {
        state.integrations.jobs.wake();
        Ok(())
    } else {
        Err(AppError::conflict(
            "only a delivery that failed, or whose outcome is unknown, can be tried again",
        ))
    }
}
