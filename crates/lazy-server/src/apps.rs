//! Apps with bot tokens (llm-docs/DECISIONS.md, D35).
//!
//! An admin or owner makes an app in Integrations: a bot (a workspace member that cannot sign in,
//! like a webhook's, D16) and a token for Lazy's bot API, `lzb_` and 64 hex digits, answered once
//! and kept only as its SHA-256. The token acts as the bot within the app's scopes:
//!
//! - `chat:write`: post (and reply in threads) in channels the bot is in;
//! - `channels:read`: list those channels;
//! - `channels:join`: join a public channel by itself.
//!
//! The admin chooses the bot's channels in the app's screen (or the bot joins public ones itself
//! with `channels:join`). A message is read like an incoming webhook's body (D33): Lazy's own
//! `text`, or Slack's. Posting is rate-limited per app, like a webhook, and an `Idempotency-Key`
//! makes a retry post once. Deleting an app stops its token at once and takes its bot out of every
//! channel; the bot stays so what it posted keeps its author.
//!
//! A bot token is not a session: the ordinary API refuses it, and the bot API refuses sessions.

use axum::http::HeaderMap;
use lazy_domain::app::{AppScope, BOT_TOKEN_PREFIX};
use lazy_domain::{
    AppId, ChannelName, ConversationId, ConversationKind, MessageId, OperationId, Timestamp,
    UserId, WebhookId, WorkspaceId,
};
use lazy_protocol::{
    App, AppChannel, BotConversationsResponse, BotMeResponse, BotMessageResponse, CreateAppRequest,
    CreatedAppResponse, ErrorCode, Event, SendMessageRequest,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::error::AppError;
use crate::events::{self, Audience, PendingEvent};
use crate::github::{create_bot, manage_error, require_admin};
use crate::messages;

/// The most apps a workspace can have.
pub const MAX_APPS_PER_WORKSPACE: i64 = 50;

/// Who a bot token speaks for.
#[derive(Debug, Clone)]
pub struct BotPrincipal {
    pub app_id: AppId,
    pub workspace_id: WorkspaceId,
    pub bot_user_id: UserId,
    pub name: String,
    pub scopes: Vec<AppScope>,
}

impl BotPrincipal {
    fn require(&self, scope: AppScope) -> Result<(), AppError> {
        if self.scopes.contains(&scope) {
            Ok(())
        } else {
            Err(AppError::forbidden(format!(
                "this app's token does not have the {} scope",
                scope.as_str()
            )))
        }
    }
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn new_token() -> Result<String, AppError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(format!("no randomness: {e}")))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{BOT_TOKEN_PREFIX}{hex}"))
}

fn hint(token: &str) -> String {
    token[token.len().saturating_sub(4)..].to_owned()
}

fn scopes_of(names: &[String]) -> Vec<AppScope> {
    names
        .iter()
        .filter_map(|name| AppScope::from_name(name))
        .collect()
}

fn scope_names(scopes: &[AppScope]) -> Result<Vec<String>, AppError> {
    let mut names: Vec<String> = scopes
        .iter()
        .filter(|scope| **scope != AppScope::Unknown)
        .map(|scope| scope.as_str().to_owned())
        .collect();
    names.sort_unstable();
    names.dedup();
    if names.is_empty() {
        return Err(AppError::invalid(
            "choose at least one thing the app may do",
        ));
    }
    Ok(names)
}

fn app_name(name: &str) -> Result<String, AppError> {
    let name = name.trim();
    lazy_domain::DisplayName::parse(name)
        .map(|_| name.to_owned())
        .map_err(|_| AppError::invalid("an app's name is 1 to 64 characters"))
}

// ----- Authenticating --------------------------------------------------------------------------

/// An app found by its token's hash: id, workspace, bot, name, scopes, stored hash, last use.
type TokenRow = (
    Uuid,
    Uuid,
    Uuid,
    String,
    Vec<String>,
    Vec<u8>,
    Option<OffsetDateTime>,
);

/// The app a bot token belongs to. Anything else (no such token, a deleted app, a session token)
/// is `unauthenticated`, all alike.
pub async fn authenticate(pool: &PgPool, token: &str) -> Result<BotPrincipal, AppError> {
    if !token.starts_with(BOT_TOKEN_PREFIX) || token.len() > 200 {
        return Err(AppError::unauthenticated());
    }
    let hash = token_hash(token);
    let row: Option<TokenRow> = sqlx::query_as(
        "SELECT id, workspace_id, bot_user_id, name, scopes, token_hash, last_used_at \
             FROM apps WHERE token_hash = $1 AND deleted_at IS NULL",
    )
    .bind(&hash[..])
    .fetch_optional(pool)
    .await?;
    let Some((id, workspace, bot, name, scopes, stored, last_used)) = row else {
        return Err(AppError::unauthenticated());
    };
    // The lookup was by hash; compare again in constant time all the same.
    if !bool::from(stored.as_slice().ct_eq(&hash[..])) {
        return Err(AppError::unauthenticated());
    }
    // When it was last used, to the minute: enough to tell an idle token, without a write each
    // request.
    let stale =
        last_used.is_none_or(|at| OffsetDateTime::now_utc() - at > time::Duration::minutes(1));
    if stale {
        sqlx::query("UPDATE apps SET last_used_at = now() WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(BotPrincipal {
        app_id: AppId::from_uuid(id),
        workspace_id: WorkspaceId::from_uuid(workspace),
        bot_user_id: UserId::from_uuid(bot),
        name,
        scopes: scopes_of(&scopes),
    })
}

// ----- Managing --------------------------------------------------------------------------------

#[derive(sqlx::FromRow)]
struct AppRow {
    id: Uuid,
    workspace_id: Uuid,
    name: String,
    bot_user_id: Uuid,
    token_hint: String,
    scopes: Vec<String>,
    created_by: Uuid,
    created_at: OffsetDateTime,
    last_used_at: Option<OffsetDateTime>,
}

const APP_COLUMNS: &str = "id, workspace_id, name, bot_user_id, token_hint, scopes, created_by, \
     created_at, last_used_at";

/// The channels `bot` is in that `viewer` can read (public ones, and private ones they are in).
async fn channels_of(
    conn: &mut PgConnection,
    bot: Uuid,
    viewer: Option<UserId>,
) -> Result<Vec<AppChannel>, AppError> {
    let rows: Vec<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT c.id, c.name FROM conversations c \
         JOIN conversation_members m ON m.conversation_id = c.id AND m.user_id = $1 \
         WHERE c.kind = 'channel' AND NOT c.archived \
           AND ($2::uuid IS NULL OR c.visibility = 'public' OR EXISTS \
                (SELECT 1 FROM conversation_members v \
                 WHERE v.conversation_id = c.id AND v.user_id = $2)) \
         ORDER BY c.name",
    )
    .bind(bot)
    .bind(viewer.map(|v| v.as_uuid()))
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, name)| {
            Some(AppChannel {
                conversation_id: ConversationId::from_uuid(id),
                name: ChannelName::parse(&name?).ok()?,
            })
        })
        .collect())
}

async fn app_dto(conn: &mut PgConnection, row: AppRow, viewer: UserId) -> Result<App, AppError> {
    let channels = channels_of(conn, row.bot_user_id, Some(viewer)).await?;
    Ok(App {
        id: AppId::from_uuid(row.id),
        name: row.name,
        bot_user_id: UserId::from_uuid(row.bot_user_id),
        scopes: scopes_of(&row.scopes),
        token_hint: row.token_hint,
        channels,
        created_by: UserId::from_uuid(row.created_by),
        created_at: Timestamp::from(row.created_at),
        last_used_at: row.last_used_at.map(Timestamp::from),
        commands: crate::commands::commands_of(conn, row.id).await?,
    })
}

/// A live app, with its workspace checked against the caller: `not_found` for an app the caller
/// cannot manage or see (a stranger, another workspace), `forbidden` for a member.
async fn managed_app(
    conn: &mut PgConnection,
    actor: UserId,
    app: AppId,
    lock: bool,
) -> Result<AppRow, AppError> {
    let row: Option<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Safe: APP_COLUMNS is a constant and the lock clause one of two literals.
        "SELECT {APP_COLUMNS} FROM apps WHERE id = $1 AND deleted_at IS NULL{}",
        if lock { " FOR UPDATE" } else { "" }
    )))
    .bind(app.as_uuid())
    .fetch_optional(&mut *conn)
    .await?;
    let row = row.ok_or_else(|| AppError::not_found("app not found"))?;
    require_admin(conn, WorkspaceId::from_uuid(row.workspace_id), actor)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == ErrorCode::NotFound => {
                AppError::not_found("app not found")
            }
            other => other,
        })?;
    Ok(row)
}

/// One app, as its admin sees it.
pub async fn get(state: &AppState, actor: UserId, app: AppId) -> Result<App, AppError> {
    let mut conn = state.pool.acquire().await?;
    let row = managed_app(&mut conn, actor, app, false).await?;
    app_dto(&mut conn, row, actor).await
}

/// Adds an app's bot to a conversation `actor` is in (a command's answer posted there).
pub async fn add_bot_member(
    pool: &PgPool,
    actor: UserId,
    bot: UserId,
    conversation: ConversationId,
) -> Result<(), AppError> {
    let (mut tx, loaded) =
        conversations::begin_visible(pool, actor, conversation, Lock::Exclusive).await?;
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
    Ok(())
}

/// The workspace's apps, oldest first. Admins and owners.
pub async fn list(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<App>, AppError> {
    let mut conn = state.pool.acquire().await?;
    require_admin(&mut conn, workspace, actor).await?;
    let rows: Vec<AppRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Safe: APP_COLUMNS is a constant.
        "SELECT {APP_COLUMNS} FROM apps WHERE workspace_id = $1 AND deleted_at IS NULL \
         ORDER BY created_at, id"
    )))
    .bind(workspace.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    let mut apps = Vec::with_capacity(rows.len());
    for row in rows {
        apps.push(app_dto(&mut conn, row, actor).await?);
    }
    Ok(apps)
}

/// Makes an app: its bot and its first token, answered once.
pub async fn create(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
    request: CreateAppRequest,
) -> Result<CreatedAppResponse, AppError> {
    let name = app_name(&request.name)?;
    let scopes = scope_names(&request.scopes)?;
    let mut tx = state.pool.begin().await?;
    require_admin(&mut tx, workspace, actor).await?;
    sqlx::query("SELECT id FROM workspaces WHERE id = $1 FOR UPDATE")
        .bind(workspace.as_uuid())
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM apps WHERE workspace_id = $1 AND deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if count >= MAX_APPS_PER_WORKSPACE {
        return Err(AppError::conflict(format!(
            "a workspace can have at most {MAX_APPS_PER_WORKSPACE} apps"
        )));
    }
    let bot = create_bot(&mut tx, workspace, &name).await?;
    let token = new_token()?;
    let id = AppId::generate();
    sqlx::query(
        "INSERT INTO apps (id, workspace_id, name, bot_user_id, token_hash, token_hint, scopes, \
                           created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(id.as_uuid())
    .bind(workspace.as_uuid())
    .bind(&name)
    .bind(bot.id.as_uuid())
    .bind(&token_hash(&token)[..])
    .bind(hint(&token))
    .bind(&scopes)
    .bind(actor.as_uuid())
    .execute(&mut *tx)
    .await?;
    let row = managed_app(&mut tx, actor, id, false).await?;
    let app = app_dto(&mut tx, row, actor).await?;
    tx.commit().await?;
    tracing::info!(workspace_id = %workspace, app_id = %id, "app created");
    Ok(CreatedAppResponse { app, token })
}

/// Gives an app a new token; the old one stops working at once.
pub async fn new_token_for(
    state: &AppState,
    actor: UserId,
    app: AppId,
) -> Result<CreatedAppResponse, AppError> {
    let mut tx = state.pool.begin().await?;
    managed_app(&mut tx, actor, app, true).await?;
    let token = new_token()?;
    sqlx::query("UPDATE apps SET token_hash = $2, token_hint = $3 WHERE id = $1")
        .bind(app.as_uuid())
        .bind(&token_hash(&token)[..])
        .bind(hint(&token))
        .execute(&mut *tx)
        .await?;
    let row = managed_app(&mut tx, actor, app, false).await?;
    let dto = app_dto(&mut tx, row, actor).await?;
    tx.commit().await?;
    tracing::info!(app_id = %app, "app token replaced");
    Ok(CreatedAppResponse { app: dto, token })
}

/// Changes what an app's token may do. Takes effect on its next request.
pub async fn set_scopes(
    state: &AppState,
    actor: UserId,
    app: AppId,
    scopes: &[AppScope],
) -> Result<App, AppError> {
    let names = scope_names(scopes)?;
    let mut tx = state.pool.begin().await?;
    managed_app(&mut tx, actor, app, true).await?;
    sqlx::query("UPDATE apps SET scopes = $2 WHERE id = $1")
        .bind(app.as_uuid())
        .bind(&names)
        .execute(&mut *tx)
        .await?;
    let row = managed_app(&mut tx, actor, app, false).await?;
    let dto = app_dto(&mut tx, row, actor).await?;
    tx.commit().await?;
    Ok(dto)
}

/// Adds the bot to a channel (`conversation`), announced like any member added.
async fn add_bot(
    pool: &PgPool,
    actor: UserId,
    bot: UserId,
    conversation: ConversationId,
    workspace: WorkspaceId,
    as_admin: bool,
) -> Result<(), AppError> {
    let (mut tx, loaded) =
        conversations::begin_visible(pool, actor, conversation, Lock::Exclusive).await?;
    if loaded.workspace_id != workspace {
        return Err(AppError::not_found("conversation not found"));
    }
    if as_admin
        && !loaded
            .facts
            .workspace_role
            .is_some_and(|role| role.can_manage_integrations())
    {
        return Err(manage_error());
    }
    if loaded.facts.kind != ConversationKind::Channel {
        return Err(AppError::invalid(
            "an app's bot can be added to channels only",
        ));
    }
    if loaded.facts.archived {
        return Err(AppError::conflict("this channel is archived"));
    }
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
    Ok(())
}

/// Adds the app's bot to a channel the admin can read.
pub async fn add_channel(
    state: &AppState,
    actor: UserId,
    app: AppId,
    conversation: ConversationId,
) -> Result<App, AppError> {
    let row = {
        let mut conn = state.pool.acquire().await?;
        managed_app(&mut conn, actor, app, false).await?
    };
    add_bot(
        &state.pool,
        actor,
        UserId::from_uuid(row.bot_user_id),
        conversation,
        WorkspaceId::from_uuid(row.workspace_id),
        true,
    )
    .await?;
    let mut conn = state.pool.acquire().await?;
    let row = managed_app(&mut conn, actor, app, false).await?;
    app_dto(&mut conn, row, actor).await
}

/// Takes the app's bot out of a channel. Not in it is a quiet success.
pub async fn remove_channel(
    state: &AppState,
    actor: UserId,
    app: AppId,
    conversation: ConversationId,
) -> Result<App, AppError> {
    let row = {
        let mut conn = state.pool.acquire().await?;
        managed_app(&mut conn, actor, app, false).await?
    };
    let bot = UserId::from_uuid(row.bot_user_id);
    // Loaded as the bot, a member of the channel whether or not the admin is (they can only have
    // seen it in the app's list if they can read it).
    if let Ok((mut tx, loaded)) =
        conversations::begin_visible(&state.pool, bot, conversation, Lock::Exclusive).await
        && loaded.workspace_id == WorkspaceId::from_uuid(row.workspace_id)
    {
        conversations::remove_and_announce(&mut tx, &loaded, bot).await?;
        tx.commit().await?;
    }
    let mut conn = state.pool.acquire().await?;
    let row = managed_app(&mut conn, actor, app, false).await?;
    app_dto(&mut conn, row, actor).await
}

/// Deletes an app: its token stops working and its bot leaves every channel. Gone already is a
/// quiet success.
pub async fn delete(state: &AppState, actor: UserId, app: AppId) -> Result<(), AppError> {
    let row = {
        let mut tx = state.pool.begin().await?;
        let row = match managed_app(&mut tx, actor, app, true).await {
            Ok(row) => row,
            Err(AppError::Api(api)) if api.code == ErrorCode::NotFound => return Ok(()),
            Err(other) => return Err(other),
        };
        crate::commands::forget_app(&mut tx, app).await?;
        sqlx::query(
            "UPDATE apps SET deleted_at = now(), token_hash = NULL, token_hint = '' WHERE id = $1",
        )
        .bind(app.as_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        row
    };
    let bot = UserId::from_uuid(row.bot_user_id);
    let channels: Vec<Uuid> =
        sqlx::query_scalar("SELECT conversation_id FROM conversation_members WHERE user_id = $1")
            .bind(bot.as_uuid())
            .fetch_all(&state.pool)
            .await?;
    for channel in channels {
        let Ok((mut tx, loaded)) = conversations::begin_visible(
            &state.pool,
            bot,
            ConversationId::from_uuid(channel),
            Lock::Exclusive,
        )
        .await
        else {
            continue;
        };
        conversations::remove_and_announce(&mut tx, &loaded, bot).await?;
        tx.commit().await?;
    }
    tracing::info!(app_id = %app, "app deleted");
    Ok(())
}

// ----- The bot API -----------------------------------------------------------------------------

pub fn me(bot: &BotPrincipal) -> BotMeResponse {
    BotMeResponse {
        app_id: bot.app_id,
        name: bot.name.clone(),
        bot_user_id: bot.bot_user_id,
        workspace_id: bot.workspace_id,
        scopes: bot.scopes.clone(),
    }
}

/// The channels the bot is in (`channels:read`).
pub async fn conversations_of(
    state: &AppState,
    bot: &BotPrincipal,
) -> Result<BotConversationsResponse, AppError> {
    bot.require(AppScope::ChannelsRead)?;
    let mut conn = state.pool.acquire().await?;
    let conversations = channels_of(&mut conn, bot.bot_user_id.as_uuid(), None).await?;
    Ok(BotConversationsResponse { conversations })
}

/// Joins a public channel (`channels:join`). A private one, or one in another workspace, is
/// `not_found`: the bot cannot see it.
pub async fn join(
    state: &AppState,
    bot: &BotPrincipal,
    conversation: ConversationId,
) -> Result<(), AppError> {
    bot.require(AppScope::ChannelsJoin)?;
    let public: Option<(Uuid, String, String)> =
        sqlx::query_as("SELECT workspace_id, kind, visibility FROM conversations WHERE id = $1")
            .bind(conversation.as_uuid())
            .fetch_optional(&state.pool)
            .await?;
    match public {
        Some((workspace, kind, visibility))
            if workspace == bot.workspace_id.as_uuid()
                && kind == "channel"
                && visibility == "public" => {}
        _ => return Err(AppError::not_found("conversation not found")),
    }
    add_bot(
        &state.pool,
        bot.bot_user_id,
        bot.bot_user_id,
        conversation,
        bot.workspace_id,
        false,
    )
    .await
}

/// The channel a post names: a conversation id, or a channel name the bot is in.
async fn resolve_channel(
    pool: &PgPool,
    bot: &BotPrincipal,
    channel: &str,
) -> Result<ConversationId, AppError> {
    let channel = channel.trim();
    if let Ok(id) = Uuid::parse_str(channel) {
        return Ok(ConversationId::from_uuid(id));
    }
    let name = channel.trim_start_matches('#').to_ascii_lowercase();
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT c.id FROM conversations c \
         JOIN conversation_members m ON m.conversation_id = c.id AND m.user_id = $3 \
         WHERE c.workspace_id = $1 AND c.kind = 'channel' AND c.name = $2",
    )
    .bind(bot.workspace_id.as_uuid())
    .bind(&name)
    .bind(bot.bot_user_id.as_uuid())
    .fetch_optional(pool)
    .await?;
    found.map(ConversationId::from_uuid).ok_or_else(|| {
        AppError::not_found(
            "no channel by that name has this app in it; add the app to the channel first",
        )
    })
}

/// The operation id of a post: the same for the same app and key, so a retry is the same send;
/// new each time without a key.
fn operation_id(app: AppId, key: Option<&str>) -> OperationId {
    match key {
        Some(key) => OperationId::from_uuid(Uuid::new_v5(&app.as_uuid(), key.as_bytes())),
        None => OperationId::generate(),
    }
}

/// The limiter key of an app: its id in the webhook limiter's key type. Apps have a limiter of
/// their own, so these never meet a webhook's.
pub fn limiter_key(app: AppId) -> WebhookId {
    WebhookId::from_uuid(app.as_uuid())
}

/// Posts a message as the bot (`chat:write`): `body` is Lazy's `{text}` or Slack's body, plus
/// `channel` and an optional `thread_root`.
pub async fn post_message(
    state: &AppState,
    bot: &BotPrincipal,
    headers: &HeaderMap,
    body: &Value,
) -> Result<BotMessageResponse, AppError> {
    bot.require(AppScope::ChatWrite)?;
    let key = match headers.get("idempotency-key") {
        Some(value) => Some(crate::webhooks::check_idempotency_key(value.as_bytes())?),
        None => None,
    };
    let channel = body["channel"]
        .as_str()
        .ok_or_else(|| AppError::invalid("name the channel to post in: \"channel\""))?;
    let conversation = resolve_channel(&state.pool, bot, channel).await?;
    let thread_root = match body.get("thread_root").or_else(|| body.get("thread_ts")) {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|text| Uuid::parse_str(text).ok())
                .map(MessageId::from_uuid)
                .ok_or_else(|| AppError::invalid("thread_root must be a message id"))?,
        ),
    };
    let text = crate::slack_compat::to_markdown(body).ok_or_else(|| {
        AppError::invalid("there is nothing to post: send text, blocks, or attachments")
    })?;
    let source = lazy_domain::emoji::replace_shortcodes(&text);
    let document = lazy_domain::MessageDocument::from_markdown(&source)
        .map_err(|e| AppError::invalid(format!("the text cannot be sent: {e}")))?;
    let request = SendMessageRequest {
        operation_id: operation_id(bot.app_id, key),
        document,
        thread_root,
        also_in_channel: false,
        // A bot posts words only, as a webhook does.
        attachment_ids: Vec::new(),
    };
    let message =
        messages::send_message(&state.pool, bot.bot_user_id, conversation, request).await?;
    Ok(BotMessageResponse {
        message_id: message.id,
        conversation_id: conversation,
    })
}
