//! Incoming webhooks and their bots (llm-docs/DECISIONS.md, D16).
//!
//! An admin or owner makes a webhook for a conversation; it has an address,
//! `{public-url}/hooks/{id}/{secret}`, shown once, and a **bot** user that authors what is posted
//! to it. Anything that can `POST {"text": "..."}` to the address posts into that one
//! conversation. A delivery is an ordinary send ([`crate::messages::send_message`]) by the bot,
//! which is a member of the conversation, so ordering, events, unread counts, replay and receipts
//! are the ones every message has.
//!
//! # The secret
//!
//! 256 random bits, kept only as a SHA-256 hash and compared in constant time. A wrong secret, an
//! id that does not exist (or is not an id at all), and a revoked webhook all answer `not_found`
//! with the same body, and all do the same work first (one lookup, one hash, one comparison), so
//! neither the answer nor its timing says which. The address never reaches a log: requests are logged by their matched route pattern
//! (`/hooks/{webhook_id}/{secret}`, see [`crate::logging`]), never by their path.
//!
//! # The bot
//!
//! `users.is_bot`, username `bot-` and eight hex digits taken from the *end* of the webhook id (the
//! random tail; the start of a v7 id is a timestamp and would repeat within a minute), the webhook's
//! name as the display name, role `member`. It has no password (the marker `'!'`, which the table
//! constrains bots to and people cannot have) and no session can reference it, so it can never sign
//! in. Revoking a webhook removes the bot from the conversation and keeps the user, so past messages
//! keep their author.
//!
//! Locks: creating and revoking change the conversation's membership, so both hold its row
//! exclusively, like adding or removing any member; a delivery holds it shared, like any send, so a
//! delivery and a revocation take turns and a delivery never lands after the bot has left.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lazy_domain::emoji::replace_shortcodes;
use lazy_domain::{
    ChannelName, ConversationId, ConversationKind, DisplayName, MAX_WEBHOOK_DELIVERIES_KEPT,
    MAX_WEBHOOKS_PER_CONVERSATION, MessageDocument, MessageId, OperationId, Timestamp, UserId,
    WebhookId, WorkspaceId,
};
use lazy_protocol::{
    CreateWebhookRequest, CreatedWebhook, ErrorCode, Event, SendMessageRequest, User, Webhook,
    WebhookDelivery, WebhookDeliveryOutcome, WorkspaceWebhook, api::paths,
};
use sqlx::PgPool;
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::access;
use crate::app::AppState;
use crate::config::WebhookRate;
use crate::conversations::{self, Lock};
use crate::crypto;
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::groups;
use crate::logging::Describe;
use crate::messages;

/// How many times creating a webhook tries a new id when the bot's generated username is taken.
const USERNAME_ATTEMPTS: usize = 5;

/// What every failed authentication says.
fn webhook_not_found() -> AppError {
    AppError::not_found("webhook not found")
}

/// Whether `username` has the shape of the ones generated for bots (`bot-` and eight lowercase
/// hex digits). People cannot register such a name, so a bot's can never already be taken by one.
pub fn is_bot_username(username: &str) -> bool {
    username.strip_prefix("bot-").is_some_and(|rest| {
        rest.len() == 8
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

/// The username for the bot of webhook `id`: the last eight hex digits of the id.
fn bot_username(id: WebhookId) -> String {
    let simple = id.as_uuid().simple().to_string();
    format!("bot-{}", &simple[simple.len() - 8..])
}

// --- Rate limit ------------------------------------------------------------------------------

struct Bucket {
    tokens: f64,
    updated: Instant,
    /// The last request was refused: the next refusal continues the same run.
    refusing: bool,
}

/// A request the limiter refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    /// How long until there is a token.
    pub wait: Duration,
    /// Whether this is the first refusal since the webhook's last accepted request, so that a
    /// flood of refusals makes one entry in the delivery log, not one each.
    pub first: bool,
}

/// A token bucket per webhook, in memory: `burst` deliveries at once, then one more per
/// `refill_every`. Only a webhook whose secret was right ever gets a bucket, so guesses cannot
/// grow the map or use up anyone's allowance.
pub struct WebhookLimiter {
    rate: WebhookRate,
    buckets: Mutex<HashMap<WebhookId, Bucket>>,
}

impl WebhookLimiter {
    pub fn new(rate: WebhookRate) -> Self {
        Self {
            rate,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `id`, or says how long until there is one.
    pub fn take(&self, id: WebhookId) -> Result<(), Refused> {
        self.take_at(id, Instant::now())
    }

    fn refilled(&self, bucket: &Bucket, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(bucket.updated);
        let gained = elapsed.as_secs_f64() / self.rate.refill_every.as_secs_f64().max(1e-9);
        (bucket.tokens + gained).min(f64::from(self.rate.burst))
    }

    fn take_at(&self, id: WebhookId, now: Instant) -> Result<(), Refused> {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buckets.len() > 4096 {
            // Forget webhooks that have been quiet long enough to be full again.
            let full = f64::from(self.rate.burst);
            let rate = self.rate;
            buckets.retain(|_, bucket| {
                let gained = now.saturating_duration_since(bucket.updated).as_secs_f64()
                    / rate.refill_every.as_secs_f64().max(1e-9);
                bucket.tokens + gained < full
            });
        }
        let bucket = buckets.entry(id).or_insert(Bucket {
            tokens: f64::from(self.rate.burst),
            updated: now,
            refusing: false,
        });
        let tokens = self.refilled(bucket, now);
        bucket.updated = now;
        if tokens >= 1.0 {
            bucket.tokens = tokens - 1.0;
            bucket.refusing = false;
            Ok(())
        } else {
            bucket.tokens = tokens;
            let first = !bucket.refusing;
            bucket.refusing = true;
            let missing = 1.0 - tokens;
            Err(Refused {
                wait: self.rate.refill_every.mul_f64(missing),
                first,
            })
        }
    }
}

// --- Managing webhooks -----------------------------------------------------------------------

fn manage_error() -> AppError {
    AppError::forbidden("only admins and owners manage integrations")
}

#[derive(sqlx::FromRow)]
struct WebhookRow {
    id: Uuid,
    conversation_id: Uuid,
    name: String,
    bot_user_id: Uuid,
    created_by: Uuid,
    created_at: OffsetDateTime,
}

impl WebhookRow {
    fn into_webhook(self) -> Result<Webhook, AppError> {
        Ok(Webhook {
            id: WebhookId::from_uuid(self.id),
            conversation_id: ConversationId::from_uuid(self.conversation_id),
            name: DisplayName::parse(&self.name)
                .map_err(|_| AppError::internal("stored webhook name is invalid"))?,
            bot_user_id: UserId::from_uuid(self.bot_user_id),
            created_by: UserId::from_uuid(self.created_by),
            created_at: Timestamp::from(self.created_at),
        })
    }
}

/// Creates a webhook for a conversation, its bot, and the bot's place in the conversation, and
/// returns the address (the only time it can be shown).
///
/// The caller must be able to read the conversation (`not_found` otherwise) and be an admin or
/// owner (`forbidden`); only channels take webhooks (`invalid_request`), an archived one is
/// `conflict`, and a channel has at most [`MAX_WEBHOOKS_PER_CONVERSATION`] (`limit_reached`; exact,
/// because the conversation row is held exclusively). Appends `member_joined` (the bot, to the
/// workspace) and `member_added` (to the conversation's members) in the same transaction.
pub async fn create(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
    request: CreateWebhookRequest,
) -> Result<CreatedWebhook, AppError> {
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
        return Err(AppError::invalid(
            "webhooks post into channels, not into direct or group conversations",
        ));
    }
    if loaded.facts.archived {
        return Err(AppError::conflict(
            "this channel is archived; unarchive it before adding an integration",
        ));
    }
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM webhooks WHERE conversation_id = $1 AND revoked_at IS NULL",
    )
    .bind(conversation.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if usize::try_from(active).unwrap_or(usize::MAX) >= MAX_WEBHOOKS_PER_CONVERSATION {
        return Err(AppError::new(
            ErrorCode::LimitReached,
            format!("a channel can have at most {MAX_WEBHOOKS_PER_CONVERSATION} webhooks"),
        ));
    }

    // The bot's name is generated from the webhook id; if it is taken (a one in four billion
    // chance, or a group handle), try another id.
    let mut chosen = None;
    for _ in 0..USERNAME_ATTEMPTS {
        let id = WebhookId::generate();
        let username = bot_username(id);
        groups::lock_handle(&mut tx, &username).await?;
        let taken: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE username = $1)")
                .bind(&username)
                .fetch_one(&mut *tx)
                .await?;
        if !taken && !groups::handle_names_a_group(&mut tx, &username).await? {
            chosen = Some((id, username));
            break;
        }
    }
    let (id, username) =
        chosen.ok_or_else(|| AppError::internal("no free username for the webhook's bot"))?;

    let bot = UserId::generate();
    sqlx::query(
        "INSERT INTO users (id, username, display_name, password_hash, is_bot) \
         VALUES ($1, $2, $3, '!', true)",
    )
    .bind(bot.as_uuid())
    .bind(&username)
    .bind(request.name.as_str())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO workspace_members (workspace_id, user_id, role) VALUES ($1, $2, 'member')",
    )
    .bind(loaded.workspace_id.as_uuid())
    .bind(bot.as_uuid())
    .execute(&mut *tx)
    .await?;
    conversations::add_members(&mut tx, conversation, &[bot]).await?;

    let (secret, hash) = crypto::new_webhook_secret()?;
    let row: WebhookRow = sqlx::query_as(
        "INSERT INTO webhooks (id, workspace_id, conversation_id, name, bot_user_id, secret_hash, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         RETURNING id, conversation_id, name, bot_user_id, created_by, created_at",
    )
    .bind(id.as_uuid())
    .bind(loaded.workspace_id.as_uuid())
    .bind(conversation.as_uuid())
    .bind(request.name.as_str())
    .bind(bot.as_uuid())
    .bind(&hash[..])
    .bind(actor.as_uuid())
    .fetch_one(&mut *tx)
    .await?;

    let user = User {
        id: bot,
        username: lazy_domain::Username::parse(&username)
            .map_err(|_| AppError::internal("generated username is invalid"))?,
        display_name: request.name,
        is_bot: true,
        status: None,
        deactivated: false,
        avatar_version: 0,
    };
    let conversation_dto = conversations::conversation_dto(&mut tx, conversation).await?;
    events::append_all(
        &mut tx,
        loaded.workspace_id,
        vec![
            PendingEvent {
                kind: "member_joined",
                conversation_id: None,
                operation_id: None,
                audience: Audience::Workspace,
                event: Event::MemberJoined { user },
            },
            PendingEvent {
                kind: "member_added",
                conversation_id: Some(conversation),
                operation_id: None,
                audience: Audience::Members,
                event: Event::MemberAdded {
                    conversation: conversation_dto,
                    user_id: bot,
                },
            },
        ],
    )
    .await?;
    tx.commit().await?;

    let webhook = row.into_webhook()?;
    let url = format!(
        "{}{}",
        state.integrations.public_url,
        paths::hook(id, &secret)
    );
    Ok(CreatedWebhook { webhook, url })
}

/// The conversation's webhooks that are in use, oldest first, without secrets. Admins and owners
/// who can read the conversation only.
pub async fn list(
    pool: &PgPool,
    actor: UserId,
    conversation: ConversationId,
) -> Result<Vec<Webhook>, AppError> {
    let mut conn = pool.acquire().await?;
    let loaded = conversations::load(&mut conn, actor, conversation)
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(|| AppError::not_found("conversation not found"))?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    let rows: Vec<WebhookRow> = sqlx::query_as(
        "SELECT id, conversation_id, name, bot_user_id, created_by, created_at FROM webhooks \
         WHERE conversation_id = $1 AND revoked_at IS NULL ORDER BY created_at, id",
    )
    .bind(conversation.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(WebhookRow::into_webhook).collect()
}

/// Revokes a webhook: its address stops working, and its bot leaves the conversation
/// (`member_removed`, then `conversation_updated` with the new count). The bot user stays, so past
/// messages keep their author. Idempotent: a webhook that is gone, or already revoked, is a quiet
/// success (as is one that never existed, for any signed-in caller).
pub async fn delete(pool: &PgPool, actor: UserId, id: WebhookId) -> Result<(), AppError> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM webhooks WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(pool)
            .await?;
    let Some(conversation) = found.map(ConversationId::from_uuid) else {
        return Ok(());
    };
    let (mut tx, loaded) = conversations::begin_visible(pool, actor, conversation, Lock::Exclusive)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == ErrorCode::NotFound => {
                AppError::not_found("webhook not found")
            }
            other => other,
        })?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    let row: Option<(Uuid, bool)> = sqlx::query_as(
        "SELECT bot_user_id, revoked_at IS NOT NULL FROM webhooks WHERE id = $1 FOR UPDATE",
    )
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((bot, revoked)) = row else {
        return Ok(());
    };
    if revoked {
        return Ok(());
    }
    sqlx::query("UPDATE webhooks SET revoked_at = now(), secret_hash = NULL WHERE id = $1")
        .bind(id.as_uuid())
        .execute(&mut *tx)
        .await?;
    conversations::remove_and_announce(&mut tx, &loaded, UserId::from_uuid(bot)).await?;
    tx.commit().await?;
    Ok(())
}

// --- Delivering ------------------------------------------------------------------------------

/// A webhook whose address was right and that is still in use.
#[derive(Debug, Clone, Copy)]
pub struct ActiveWebhook {
    pub id: WebhookId,
    pub conversation_id: ConversationId,
    pub bot_user_id: UserId,
}

/// Checks an address. Every failure is the same `not_found`, and every failure does the same work:
/// one lookup (of the nil id, if `id` is not an id), one hash of `secret`, one constant-time
/// comparison (against zeros, if there is nothing to compare with).
pub async fn authenticate(
    pool: &PgPool,
    id: &str,
    secret: &str,
) -> Result<ActiveWebhook, AppError> {
    let parsed = id.parse::<WebhookId>().ok();
    let row: Option<(Uuid, Uuid, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT conversation_id, bot_user_id, secret_hash FROM webhooks WHERE id = $1",
    )
    .bind(parsed.map_or(Uuid::nil(), |id| id.as_uuid()))
    .fetch_optional(pool)
    .await?;
    let candidate = crypto::webhook_secret_hash(secret);
    let expected: [u8; 32] = row
        .as_ref()
        .and_then(|(_, _, hash)| hash.as_deref())
        .and_then(|hash| <[u8; 32]>::try_from(hash).ok())
        .unwrap_or([0u8; 32]);
    let matches: bool = candidate.ct_eq(&expected).into();
    match (parsed, row) {
        (Some(id), Some((conversation, bot, Some(_)))) if matches => Ok(ActiveWebhook {
            id,
            conversation_id: ConversationId::from_uuid(conversation),
            bot_user_id: UserId::from_uuid(bot),
        }),
        _ => Err(webhook_not_found()),
    }
}

/// An `Idempotency-Key`: 1 to 64 printable ASCII characters.
pub fn check_idempotency_key(key: &[u8]) -> Result<&str, AppError> {
    if key.is_empty() || key.len() > 64 || !key.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return Err(AppError::invalid(
            "Idempotency-Key must be 1 to 64 printable ASCII characters",
        ));
    }
    std::str::from_utf8(key).map_err(|_| AppError::invalid("Idempotency-Key must be ASCII"))
}

/// The operation id of a delivery: the same for the same webhook and key, so a retry is recognized
/// by the ordinary send receipt; a new one each time without a key.
fn operation_id(webhook: WebhookId, key: Option<&str>) -> OperationId {
    match key {
        Some(key) => OperationId::from_uuid(Uuid::new_v5(&webhook.as_uuid(), key.as_bytes())),
        None => OperationId::generate(),
    }
}

/// A delivery that posted, or answered a message posted before under the same `Idempotency-Key`.
#[derive(Debug, Clone, Copy)]
pub struct Delivered {
    pub message_id: MessageId,
    pub duplicate: bool,
}

/// A delivery that did not post: the refusal to answer with, and what the delivery log records
/// (`None` when there is nothing to record it under: the webhook was revoked meanwhile).
#[derive(Debug)]
pub struct Undelivered {
    pub error: AppError,
    pub outcome: Option<WebhookDeliveryOutcome>,
}

impl From<AppError> for Undelivered {
    /// The log's word for a refusal, by its code. An archived channel is classified where it is
    /// found ([`explain_refusal`]); any other `conflict` is an `Idempotency-Key` used again with
    /// different text, which is the sender's mistake, like any unreadable request.
    fn from(error: AppError) -> Self {
        let outcome = match &error {
            AppError::Api(api) => match api.code {
                ErrorCode::InvalidRequest | ErrorCode::Conflict => {
                    Some(WebhookDeliveryOutcome::Invalid)
                }
                ErrorCode::PayloadTooLarge => Some(WebhookDeliveryOutcome::TooLarge),
                ErrorCode::RateLimited => Some(WebhookDeliveryOutcome::RateLimited),
                ErrorCode::NotFound => None,
                _ => Some(WebhookDeliveryOutcome::Failed),
            },
            AppError::Database(_) | AppError::Internal(_) => Some(WebhookDeliveryOutcome::Failed),
        };
        Self { error, outcome }
    }
}

/// Posts `text` into the webhook's conversation as its bot, through the ordinary send.
///
/// The text is composer source: `:shortcode:` emoji are replaced and the rest is parsed as
/// markdown, with no mention directory, so `@names` stay text and notify nobody. Nothing visible
/// is `invalid_request`. The refusals of the send (the bot is no longer a member because the
/// webhook was revoked a moment ago; the conversation was archived) are reported as what they
/// mean to the caller of the address: `not_found` and `conflict`.
///
/// A key that already has a receipt is a duplicate: the send answers the earlier message and
/// posts nothing. (Two requests with a new key at the same moment both count as posted; one of
/// them waits for the other's receipt and answers its message.)
pub async fn deliver(
    pool: &PgPool,
    webhook: ActiveWebhook,
    key: Option<&str>,
    text: &str,
    rich: Option<lazy_domain::RichContent>,
) -> Result<Delivered, Undelivered> {
    if text.trim().is_empty() {
        return Err(AppError::invalid("text must not be empty").into());
    }
    let source = replace_shortcodes(text);
    let document = MessageDocument::from_markdown(&source)
        .map_err(|e| AppError::invalid(format!("the text cannot be sent: {e}")))?;
    let operation_id = operation_id(webhook.id, key);
    let duplicate = match key {
        Some(_) => sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM operation_receipts \
             WHERE user_id = $1 AND operation_id = $2)",
        )
        .bind(webhook.bot_user_id.as_uuid())
        .bind(operation_id.as_uuid())
        .fetch_one(pool)
        .await
        .map_err(AppError::from)?,
        None => false,
    };
    let request = SendMessageRequest {
        operation_id,
        document,
        thread_root: None,
        also_in_channel: false,
        // A webhook posts words only.
        attachment_ids: Vec::new(),
    };
    match messages::send_message_with(
        pool,
        webhook.bot_user_id,
        webhook.conversation_id,
        request,
        rich,
    )
    .await
    {
        Ok(message) => Ok(Delivered {
            message_id: message.id,
            duplicate,
        }),
        Err(AppError::Api(api))
            if matches!(api.code, ErrorCode::Forbidden | ErrorCode::NotFound) =>
        {
            Err(explain_refusal(pool, webhook, AppError::Api(api)).await?)
        }
        Err(other) => Err(other.into()),
    }
}

/// What a refused delivery means for the address: revoked while it was being delivered is
/// `not_found`; an archived conversation is `conflict`; anything else is the refusal itself.
async fn explain_refusal(
    pool: &PgPool,
    webhook: ActiveWebhook,
    refusal: AppError,
) -> Result<Undelivered, AppError> {
    let state: Option<(bool, bool)> = sqlx::query_as(
        "SELECT w.revoked_at IS NOT NULL, c.archived FROM webhooks w \
         JOIN conversations c ON c.id = w.conversation_id WHERE w.id = $1",
    )
    .bind(webhook.id.as_uuid())
    .fetch_optional(pool)
    .await?;
    Ok(match state {
        None | Some((true, _)) => Undelivered {
            error: webhook_not_found(),
            outcome: None,
        },
        Some((false, true)) => Undelivered {
            error: AppError::conflict("this channel is archived"),
            outcome: Some(WebhookDeliveryOutcome::Archived),
        },
        Some((false, false)) => Undelivered {
            error: refusal,
            outcome: Some(WebhookDeliveryOutcome::Failed),
        },
    })
}

// --- The delivery log ------------------------------------------------------------------------

/// The stored word for an outcome. `Unknown` is never recorded (it is only what a client reads
/// from a newer server).
fn outcome_to_db(outcome: WebhookDeliveryOutcome) -> &'static str {
    match outcome {
        WebhookDeliveryOutcome::Posted => "posted",
        WebhookDeliveryOutcome::Duplicate => "duplicate",
        WebhookDeliveryOutcome::RateLimited => "rate_limited",
        WebhookDeliveryOutcome::TooLarge => "too_large",
        WebhookDeliveryOutcome::Invalid => "invalid",
        WebhookDeliveryOutcome::Archived => "archived",
        WebhookDeliveryOutcome::Failed | WebhookDeliveryOutcome::Unknown => "failed",
    }
}

fn outcome_from_db(text: &str) -> WebhookDeliveryOutcome {
    match text {
        "posted" => WebhookDeliveryOutcome::Posted,
        "duplicate" => WebhookDeliveryOutcome::Duplicate,
        "rate_limited" => WebhookDeliveryOutcome::RateLimited,
        "too_large" => WebhookDeliveryOutcome::TooLarge,
        "invalid" => WebhookDeliveryOutcome::Invalid,
        "archived" => WebhookDeliveryOutcome::Archived,
        "failed" => WebhookDeliveryOutcome::Failed,
        _ => WebhookDeliveryOutcome::Unknown,
    }
}

/// One request to record in a webhook's log.
#[derive(Debug, Clone, Copy)]
pub struct DeliveryRecord {
    pub outcome: WebhookDeliveryOutcome,
    pub message_id: Option<MessageId>,
    pub body_bytes: usize,
    pub idempotency_key: bool,
}

/// Adds an entry to the webhook's delivery log and trims the log to its newest
/// [`MAX_WEBHOOK_DELIVERIES_KEPT`]. Best effort: the request has already been answered for, so a
/// failure here is logged (without anything from the request) and not reported to the sender.
pub async fn record(pool: &PgPool, webhook: WebhookId, entry: DeliveryRecord) {
    if let Err(error) = try_record(pool, webhook, entry).await {
        tracing::warn!(
            webhook_id = %webhook,
            error = %error.describe(),
            "could not record a webhook delivery"
        );
    }
}

async fn try_record(
    pool: &PgPool,
    webhook: WebhookId,
    entry: DeliveryRecord,
) -> Result<(), AppError> {
    let posted = entry.outcome.posted();
    sqlx::query(
        "INSERT INTO webhook_deliveries (webhook_id, outcome, message_id, body_bytes, idempotency_key) \
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(webhook.as_uuid())
    .bind(outcome_to_db(entry.outcome))
    .bind(entry.message_id.filter(|_| posted).map(|id| id.as_uuid()))
    .bind(i32::try_from(entry.body_bytes).unwrap_or(i32::MAX))
    .bind(entry.idempotency_key)
    .execute(pool)
    .await?;
    let keep = i64::try_from(MAX_WEBHOOK_DELIVERIES_KEPT).unwrap_or(i64::MAX);
    sqlx::query(
        "DELETE FROM webhook_deliveries WHERE webhook_id = $1 AND id < \
         (SELECT id FROM webhook_deliveries WHERE webhook_id = $1 \
          ORDER BY id DESC OFFSET $2 - 1 LIMIT 1)",
    )
    .bind(webhook.as_uuid())
    .bind(keep)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    received_at: OffsetDateTime,
    outcome: String,
    message_id: Option<Uuid>,
    body_bytes: i32,
    idempotency_key: bool,
}

impl DeliveryRow {
    fn into_delivery(self) -> WebhookDelivery {
        WebhookDelivery {
            received_at: Timestamp::from(self.received_at),
            outcome: outcome_from_db(&self.outcome),
            message_id: self.message_id.map(MessageId::from_uuid),
            body_bytes: u32::try_from(self.body_bytes).unwrap_or(0),
            idempotency_key: self.idempotency_key,
        }
    }
}

/// A webhook's delivery log, newest first. Admins and owners who can read its channel; a revoked
/// webhook keeps its log. A webhook that does not exist, or is in a channel the caller cannot
/// read, is `not_found`.
pub async fn deliveries(
    pool: &PgPool,
    actor: UserId,
    id: WebhookId,
) -> Result<Vec<WebhookDelivery>, AppError> {
    let mut conn = pool.acquire().await?;
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM webhooks WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    let conversation = found
        .map(ConversationId::from_uuid)
        .ok_or_else(webhook_not_found)?;
    let loaded = conversations::load(&mut conn, actor, conversation)
        .await?
        .filter(conversations::Loaded::can_read)
        .ok_or_else(webhook_not_found)?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    let keep = i64::try_from(MAX_WEBHOOK_DELIVERIES_KEPT).unwrap_or(i64::MAX);
    let rows: Vec<DeliveryRow> = sqlx::query_as(
        "SELECT received_at, outcome, message_id, body_bytes, idempotency_key \
         FROM webhook_deliveries WHERE webhook_id = $1 ORDER BY id DESC LIMIT $2",
    )
    .bind(id.as_uuid())
    .bind(keep)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(DeliveryRow::into_delivery).collect())
}

#[derive(sqlx::FromRow)]
struct WorkspaceWebhookRow {
    #[sqlx(flatten)]
    webhook: WebhookRow,
    channel_name: Option<String>,
    received_at: Option<OffsetDateTime>,
    outcome: Option<String>,
    message_id: Option<Uuid>,
    body_bytes: Option<i32>,
    idempotency_key: Option<bool>,
    recent_failures: i64,
}

/// The webhooks in use in every channel of the workspace that the caller can read, oldest first,
/// each with its latest delivery and how many of its requests in the last day did not post.
/// Admins and owners only: `forbidden` for other members, `not_found` for anyone else.
pub async fn list_workspace(
    pool: &PgPool,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<WorkspaceWebhook>, AppError> {
    let mut conn = pool.acquire().await?;
    let role = access::require_member(&mut conn, workspace, actor).await?;
    if !role.can_manage_integrations() {
        return Err(manage_error());
    }
    let rows: Vec<WorkspaceWebhookRow> = sqlx::query_as(
        "SELECT w.id, w.conversation_id, w.name, w.bot_user_id, w.created_by, w.created_at, \
                c.name AS channel_name, d.received_at, d.outcome, d.message_id, d.body_bytes, d.idempotency_key, \
                (SELECT count(*) FROM webhook_deliveries f WHERE f.webhook_id = w.id \
                   AND f.received_at > now() - interval '24 hours' \
                   AND f.outcome NOT IN ('posted', 'duplicate')) AS recent_failures \
         FROM webhooks w \
         JOIN conversations c ON c.id = w.conversation_id \
         LEFT JOIN LATERAL (SELECT received_at, outcome, message_id, body_bytes, idempotency_key \
                            FROM webhook_deliveries WHERE webhook_id = w.id \
                            ORDER BY id DESC LIMIT 1) d ON true \
         WHERE w.workspace_id = $1 AND w.revoked_at IS NULL \
           AND ((c.kind = 'channel' AND c.visibility = 'public') OR EXISTS \
                (SELECT 1 FROM conversation_members cm \
                 WHERE cm.conversation_id = c.id AND cm.user_id = $2)) \
         ORDER BY w.created_at, w.id",
    )
    .bind(workspace.as_uuid())
    .bind(actor.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|row| {
            let last_delivery = match (row.received_at, row.outcome) {
                (Some(received_at), Some(outcome)) => Some(
                    DeliveryRow {
                        received_at,
                        outcome,
                        message_id: row.message_id,
                        body_bytes: row.body_bytes.unwrap_or(0),
                        idempotency_key: row.idempotency_key.unwrap_or(false),
                    }
                    .into_delivery(),
                ),
                _ => None,
            };
            let channel_name = row
                .channel_name
                .as_deref()
                .and_then(|name| ChannelName::parse(name).ok())
                .ok_or_else(|| AppError::internal("a webhook's channel has no valid name"))?;
            Ok(WorkspaceWebhook {
                webhook: row.webhook.into_webhook()?,
                channel_name,
                last_delivery,
                recent_failures: u32::try_from(row.recent_failures).unwrap_or(u32::MAX),
            })
        })
        .collect()
}

/// Gives a webhook a new secret and returns its new address (the only time it is shown). The old
/// address is `not_found` from the moment this commits; the bot, the channel, and the log stay.
/// Admins and owners who can read the channel; a revoked or unknown webhook is `not_found`. An
/// archived channel may still get a new address (deliveries stay refused until it is unarchived).
pub async fn new_address(
    state: &AppState,
    actor: UserId,
    id: WebhookId,
) -> Result<CreatedWebhook, AppError> {
    let found: Option<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM webhooks WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_optional(&state.pool)
            .await?;
    let conversation = found
        .map(ConversationId::from_uuid)
        .ok_or_else(webhook_not_found)?;
    let (mut tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Shared)
            .await
            .map_err(|error| match error {
                AppError::Api(api) if api.code == ErrorCode::NotFound => webhook_not_found(),
                other => other,
            })?;
    if !loaded
        .facts
        .workspace_role
        .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    let (secret, hash) = crypto::new_webhook_secret()?;
    let row: Option<WebhookRow> = sqlx::query_as(
        "UPDATE webhooks SET secret_hash = $2 WHERE id = $1 AND revoked_at IS NULL \
         RETURNING id, conversation_id, name, bot_user_id, created_by, created_at",
    )
    .bind(id.as_uuid())
    .bind(&hash[..])
    .fetch_optional(&mut *tx)
    .await?;
    let webhook = row.ok_or_else(webhook_not_found)?.into_webhook()?;
    tx.commit().await?;
    let url = format!(
        "{}{}",
        state.integrations.public_url,
        paths::hook(id, &secret)
    );
    Ok(CreatedWebhook { webhook, url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bot_usernames_are_recognized_and_a_persons_are_not() {
        let id = WebhookId::generate();
        assert!(is_bot_username(&bot_username(id)));
        assert!(is_bot_username("bot-0123abcd"));
        for not in [
            "bot-0123abc",
            "bot-0123abcde",
            "bot-0123ABCD",
            "bot-0123abcg",
            "robot-0123abcd",
            "bot0123abcd",
            "bot-",
        ] {
            assert!(!is_bot_username(not), "{not}");
        }
    }

    #[test]
    fn a_bots_name_comes_from_the_random_end_of_the_id() {
        let first = bot_username(WebhookId::generate());
        let second = bot_username(WebhookId::generate());
        assert_ne!(first, second, "ids made together must not share a name");
        assert_eq!(first.len(), 12);
    }

    #[test]
    fn idempotency_keys_are_short_printable_ascii() {
        assert_eq!(check_idempotency_key(b"order-1234").unwrap(), "order-1234");
        assert_eq!(check_idempotency_key(&[b'a'; 64]).unwrap().len(), 64);
        for bad in [
            &b""[..],
            &[b'a'; 65][..],
            b"tab\there",
            "é".as_bytes(),
            b"nul\0",
        ] {
            assert!(check_idempotency_key(bad).is_err());
        }
    }

    #[test]
    fn the_same_key_makes_the_same_operation_and_another_webhook_a_different_one() {
        let (a, b) = (WebhookId::generate(), WebhookId::generate());
        assert_eq!(operation_id(a, Some("k")), operation_id(a, Some("k")));
        assert_ne!(operation_id(a, Some("k")), operation_id(a, Some("j")));
        assert_ne!(operation_id(a, Some("k")), operation_id(b, Some("k")));
        assert_ne!(operation_id(a, None), operation_id(a, None));
    }

    #[test]
    fn the_limiter_allows_a_burst_then_one_per_interval() {
        let rate = WebhookRate {
            burst: 3,
            refill_every: Duration::from_secs(2),
        };
        let limiter = WebhookLimiter::new(rate);
        let id = WebhookId::generate();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(limiter.take_at(id, start).is_ok());
        }
        let refused = limiter.take_at(id, start).unwrap_err();
        assert_eq!(refused.wait, Duration::from_secs(2));
        assert!(refused.first, "the first refusal of a run is marked");
        // Half an interval later there is half a token: still refused, and the wait is shorter.
        let refused = limiter
            .take_at(id, start + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(refused.wait, Duration::from_secs(1));
        assert!(!refused.first, "the run of refusals continues");
        assert!(limiter.take_at(id, start + Duration::from_secs(2)).is_ok());
        // An accepted request ends the run, so the next refusal starts a new one.
        assert!(
            limiter
                .take_at(id, start + Duration::from_secs(2))
                .unwrap_err()
                .first
        );
        // Another webhook has its own allowance; a long quiet spell refills only up to the burst.
        assert!(limiter.take_at(WebhookId::generate(), start).is_ok());
        let later = start + Duration::from_secs(600);
        for _ in 0..3 {
            assert!(limiter.take_at(id, later).is_ok());
        }
        assert!(limiter.take_at(id, later).is_err());
    }
}
