//! Slash commands (llm-docs/DECISIONS.md, D36): `/name text` in a conversation goes to the app that
//! answers `/name`, as Slack sends its slash commands, so a server written for Slack answers Lazy's
//! with its address and secret changed.
//!
//! # The request
//!
//! A `POST` of a form, signed as Slack signs: `X-Slack-Request-Timestamp` and
//! `X-Slack-Signature: v0=` + hex HMAC-SHA256 of `v0:{timestamp}:{body}` under the app's signing
//! secret (a credential, D31, shown to admins on the app's screen). The fields are Slack's:
//! `command` (`/name`), `text`, `user_id`, `user_name`, `channel_id`, `channel_name`, `team_id`,
//! `team_domain`, `api_app_id`, `response_url`, `trigger_id`, and an empty `token` (Slack's
//! deprecated verification token). Only `https://` addresses are called, redirects are not
//! followed, and the app has 3 seconds to answer, as in Slack.
//!
//! # The answer
//!
//! An empty `200` is an acknowledgement. Plain text, or JSON with `response_type: "ephemeral"`
//! (the default), is a reply only the person who ran the command sees. `"in_channel"` is posted in
//! the conversation by the app's bot, which joins it if it must. The JSON is read like an incoming
//! webhook's body (D33).
//!
//! Later answers go to `response_url`, good for 30 minutes and 5 answers. They can only be posted
//! in the conversation for now: a private one would need an event clients do not know yet, so it
//! is refused with a word on why.

use std::time::Duration;

use aws_lc_rs::hmac;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lazy_domain::app::is_command_name;
use lazy_domain::{
    AppCommandId, AppId, ConversationId, MessageDocument, MessageId, OperationId, UserId,
    WorkspaceId,
};
use lazy_protocol::{
    AppCommand, CreateAppCommandRequest, ErrorCode, RunCommandRequest, RunCommandResponse,
    SendMessageRequest, WorkspaceCommand,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::AppState;
use crate::conversations::{self, Lock};
use crate::credentials::{self, CredentialId, Secret};
use crate::error::AppError;
use crate::github::require_admin;
use crate::messages;

/// How long an app has to answer.
const ANSWER_SECONDS: u64 = 3;
/// How long a `response_url` works, and how many answers it takes.
const RESPONSE_MINUTES: i64 = 30;
const RESPONSE_USES: i32 = 5;
/// The largest answer read.
const MAX_ANSWER_BYTES: usize = 64 * 1024;
/// The credential kind of an app's signing secret.
const SECRET_KIND: &str = "app.signing_secret";
/// The most commands an app can have.
const MAX_COMMANDS_PER_APP: i64 = 25;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex() -> Result<String, AppError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(format!("no randomness: {e}")))?;
    Ok(hex(&bytes))
}

/// Whether a command may be sent to `url`: `https://`, or `http://` to this machine only.
fn url_ok(url: &str) -> bool {
    if url.len() > 2048 || url.chars().any(char::is_whitespace) {
        return false;
    }
    if url.starts_with("https://") && url.len() > "https://".len() {
        return true;
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = match host.rsplit_once(':') {
        Some((name, port)) if !name.ends_with(']') || host.starts_with('[') => {
            if port.bytes().all(|b| b.is_ascii_digit()) {
                name
            } else {
                host
            }
        }
        _ => host,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Slack's request signature: `v0=` and the hex HMAC-SHA256 of `v0:{timestamp}:{body}`.
pub fn slack_signature(secret: &[u8], timestamp: i64, body: &str) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let tag = hmac::sign(&key, format!("v0:{timestamp}:{body}").as_bytes());
    format!("v0={}", hex(tag.as_ref()))
}

// ----- Managing (admins) -----------------------------------------------------------------------

/// An app's commands, by name.
pub async fn commands_of(conn: &mut PgConnection, app: Uuid) -> Result<Vec<AppCommand>, AppError> {
    let rows: Vec<(Uuid, String, String, String, String)> = sqlx::query_as(
        "SELECT id, name, url, description, usage_hint FROM app_commands \
         WHERE app_id = $1 ORDER BY name",
    )
    .bind(app)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, name, url, description, usage_hint)| AppCommand {
            id: AppCommandId::from_uuid(id),
            name,
            url,
            description,
            usage_hint,
        })
        .collect())
}

/// The workspace of a live app, with the caller checked as its admin (`not_found` otherwise, for
/// a stranger).
async fn managed_workspace(
    conn: &mut PgConnection,
    actor: UserId,
    app: AppId,
) -> Result<WorkspaceId, AppError> {
    let workspace: Option<Uuid> =
        sqlx::query_scalar("SELECT workspace_id FROM apps WHERE id = $1 AND deleted_at IS NULL")
            .bind(app.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    let workspace =
        WorkspaceId::from_uuid(workspace.ok_or_else(|| AppError::not_found("app not found"))?);
    require_admin(conn, workspace, actor)
        .await
        .map_err(|error| match error {
            AppError::Api(api) if api.code == ErrorCode::NotFound => {
                AppError::not_found("app not found")
            }
            other => other,
        })?;
    Ok(workspace)
}

/// Adds a command to an app, or changes the app's command of that name.
pub async fn add_command(
    state: &AppState,
    actor: UserId,
    app: AppId,
    request: CreateAppCommandRequest,
) -> Result<lazy_protocol::App, AppError> {
    let name = request
        .name
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase();
    if !is_command_name(&name) {
        return Err(AppError::invalid(
            "a command's name is 1 to 32 lower-case letters, digits, - and _",
        ));
    }
    let url = request.url.trim();
    if !url_ok(url) {
        return Err(AppError::invalid(
            "a command's address must start with https:// (or http://localhost)",
        ));
    }
    let description: String = request.description.trim().chars().take(200).collect();
    let usage: String = request.usage_hint.trim().chars().take(100).collect();
    let mut tx = state.pool.begin().await?;
    let workspace = managed_workspace(&mut tx, actor, app).await?;
    sqlx::query("SELECT id FROM apps WHERE id = $1 FOR UPDATE")
        .bind(app.as_uuid())
        .execute(&mut *tx)
        .await?;
    let taken: Option<Uuid> = sqlx::query_scalar(
        "SELECT c.app_id FROM app_commands c JOIN apps a ON a.id = c.app_id \
         WHERE c.workspace_id = $1 AND c.name = $2 AND a.deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(&name)
    .fetch_optional(&mut *tx)
    .await?;
    match taken {
        Some(owner) if owner != app.as_uuid() => {
            return Err(AppError::conflict(format!(
                "another app already answers /{name}"
            )));
        }
        Some(_) => {}
        None => {
            let count: i64 =
                sqlx::query_scalar("SELECT count(*) FROM app_commands WHERE app_id = $1")
                    .bind(app.as_uuid())
                    .fetch_one(&mut *tx)
                    .await?;
            if count >= MAX_COMMANDS_PER_APP {
                return Err(AppError::conflict(format!(
                    "an app can have at most {MAX_COMMANDS_PER_APP} commands"
                )));
            }
            // A deleted app's command of the same name is gone with it.
            sqlx::query("DELETE FROM app_commands WHERE workspace_id = $1 AND name = $2")
                .bind(workspace.as_uuid())
                .bind(&name)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query(
        "INSERT INTO app_commands (id, app_id, workspace_id, name, url, description, usage_hint) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (workspace_id, name) DO UPDATE SET url = EXCLUDED.url, \
             description = EXCLUDED.description, usage_hint = EXCLUDED.usage_hint",
    )
    .bind(AppCommandId::generate().as_uuid())
    .bind(app.as_uuid())
    .bind(workspace.as_uuid())
    .bind(&name)
    .bind(url)
    .bind(&description)
    .bind(&usage)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    crate::apps::get(state, actor, app).await
}

/// Removes a command. Gone already is a quiet success.
pub async fn remove_command(
    state: &AppState,
    actor: UserId,
    command: AppCommandId,
) -> Result<(), AppError> {
    let mut conn = state.pool.acquire().await?;
    let app: Option<Uuid> = sqlx::query_scalar("SELECT app_id FROM app_commands WHERE id = $1")
        .bind(command.as_uuid())
        .fetch_optional(&mut *conn)
        .await?;
    let Some(app) = app else {
        return Ok(());
    };
    managed_workspace(&mut conn, actor, AppId::from_uuid(app)).await?;
    sqlx::query("DELETE FROM app_commands WHERE id = $1")
        .bind(command.as_uuid())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The app's signing secret, made the first time it is asked for. Admins and owners; needs the
/// server's credentials key.
pub async fn signing_secret(
    state: &AppState,
    actor: UserId,
    app: AppId,
) -> Result<String, AppError> {
    let keys = &state.integrations.credentials;
    if !keys.enabled() {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "slash commands need this server's credentials key (LAZY_CREDENTIALS_KEY)",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let workspace = managed_workspace(&mut tx, actor, app).await?;
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT signing_secret_id FROM apps WHERE id = $1 FOR UPDATE")
            .bind(app.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
    let text = match existing {
        Some(id) => credentials::load(&mut tx, keys, CredentialId(id))
            .await?
            .expose_text()
            .map(str::to_owned)
            .ok_or_else(|| AppError::internal("the signing secret is not text"))?,
        None => {
            let text = random_hex()?;
            let stored = credentials::store(
                &mut tx,
                keys,
                workspace,
                SECRET_KIND,
                &Secret::from_text(&text),
            )
            .await?;
            sqlx::query("UPDATE apps SET signing_secret_id = $2 WHERE id = $1")
                .bind(app.as_uuid())
                .bind(stored.0)
                .execute(&mut *tx)
                .await?;
            text
        }
    };
    tx.commit().await?;
    Ok(text)
}

/// Revokes an app's signing secret and its commands (the app is being deleted).
pub async fn forget_app(conn: &mut PgConnection, app: AppId) -> Result<(), AppError> {
    let secret: Option<Option<Uuid>> =
        sqlx::query_scalar("SELECT signing_secret_id FROM apps WHERE id = $1")
            .bind(app.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    if let Some(Some(secret)) = secret {
        credentials::revoke(&mut *conn, CredentialId(secret)).await?;
    }
    sqlx::query("DELETE FROM app_commands WHERE app_id = $1")
        .bind(app.as_uuid())
        .execute(&mut *conn)
        .await?;
    Ok(())
}

// ----- Using (members) -------------------------------------------------------------------------

/// The commands members of the workspace can use, by name.
pub async fn workspace_commands(
    state: &AppState,
    actor: UserId,
    workspace: WorkspaceId,
) -> Result<Vec<WorkspaceCommand>, AppError> {
    let mut conn = state.pool.acquire().await?;
    crate::access::require_member(&mut conn, workspace, actor).await?;
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT c.name, c.description, c.usage_hint, a.name FROM app_commands c \
         JOIN apps a ON a.id = c.app_id \
         WHERE c.workspace_id = $1 AND a.deleted_at IS NULL ORDER BY c.name",
    )
    .bind(workspace.as_uuid())
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(name, description, usage_hint, app_name)| WorkspaceCommand {
                name,
                description,
                usage_hint,
                app_name,
            },
        )
        .collect())
}

/// The command a run names, with its app.
#[derive(sqlx::FromRow)]
struct CommandRow {
    url: String,
    app_id: Uuid,
    app_name: String,
    bot_user_id: Uuid,
    signing_secret_id: Option<Uuid>,
}

/// What an answer asks for.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    Nothing,
    Private(String),
    InChannel(String),
}

/// Reads an app's answer: empty, plain text, or Slack's JSON.
fn read_answer(content_type: Option<&str>, body: &[u8]) -> Result<Answer, AppError> {
    let text = String::from_utf8_lossy(body);
    if text.trim().is_empty() {
        return Ok(Answer::Nothing);
    }
    let json =
        content_type.is_some_and(|t| t.contains("json")) || text.trim_start().starts_with('{');
    if !json {
        return Ok(Answer::Private(lazy_domain::markdown::escape_plain(
            text.trim(),
        )));
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| AppError::invalid("the app's answer is not JSON"))?;
    let Some(markdown) = crate::slack_compat::to_markdown(&value) else {
        return Ok(Answer::Nothing);
    };
    Ok(if value["response_type"] == "in_channel" {
        Answer::InChannel(markdown)
    } else {
        Answer::Private(markdown)
    })
}

fn document(markdown: &str) -> Result<MessageDocument, AppError> {
    let source = lazy_domain::emoji::replace_shortcodes(markdown);
    MessageDocument::from_markdown(&source)
        .map_err(|e| AppError::invalid(format!("the app's answer cannot be shown: {e}")))
}

/// Posts an answer in the conversation as the app's bot, joining it first if it must.
async fn post_in_channel(
    state: &AppState,
    actor: UserId,
    bot: UserId,
    conversation: ConversationId,
    markdown: &str,
    operation: OperationId,
) -> Result<MessageId, AppError> {
    let joined: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(conversation.as_uuid())
    .bind(bot.as_uuid())
    .fetch_one(&state.pool)
    .await?;
    if !joined {
        crate::apps::add_bot_member(&state.pool, actor, bot, conversation).await?;
    }
    let request = SendMessageRequest {
        operation_id: operation,
        document: document(markdown)?,
        thread_root: None,
        also_in_channel: false,
        attachment_ids: Vec::new(),
    };
    Ok(
        messages::send_message(&state.pool, bot, conversation, request)
            .await?
            .id,
    )
}

/// Runs `/command text` in a conversation the caller can post in.
pub async fn run(
    state: &AppState,
    actor: UserId,
    conversation: ConversationId,
    request: RunCommandRequest,
) -> Result<RunCommandResponse, AppError> {
    let name = request
        .command
        .trim()
        .trim_start_matches('/')
        .to_ascii_lowercase();
    let text: String = request.text.trim().chars().take(4000).collect();
    let (tx, loaded) =
        conversations::begin_visible(&state.pool, actor, conversation, Lock::Shared).await?;
    drop(tx);
    if !loaded.facts.is_member || loaded.facts.archived {
        return Err(AppError::forbidden("you can't post here"));
    }
    let workspace = loaded.workspace_id;
    let command: Option<CommandRow> = sqlx::query_as(
        "SELECT c.url, a.id AS app_id, a.name AS app_name, a.bot_user_id, a.signing_secret_id \
         FROM app_commands c JOIN apps a ON a.id = c.app_id \
         WHERE c.workspace_id = $1 AND c.name = $2 AND a.deleted_at IS NULL",
    )
    .bind(workspace.as_uuid())
    .bind(&name)
    .fetch_optional(&state.pool)
    .await?;
    let Some(command) = command else {
        return Err(AppError::not_found(format!(
            "/{name} is not a command here"
        )));
    };
    let Some(secret_id) = command.signing_secret_id else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            format!("{} isn't set up to answer commands yet", command.app_name),
        ));
    };
    let Some(http) = &state.integrations.http else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "this server cannot make HTTPS requests",
        ));
    };
    let mut conn = state.pool.acquire().await?;
    let secret = credentials::load(
        &mut conn,
        &state.integrations.credentials,
        CredentialId(secret_id),
    )
    .await?;
    let (user_name, channel_name): (String, Option<String>) = sqlx::query_as(
        "SELECT u.username, c.name FROM users u, conversations c WHERE u.id = $1 AND c.id = $2",
    )
    .bind(actor.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    // Where a later answer goes.
    let token = random_hex()?;
    let token_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    sqlx::query(
        "INSERT INTO command_responses (token_hash, app_id, conversation_id, user_id, expires_at) \
         VALUES ($1, $2, $3, $4, now() + make_interval(mins => $5))",
    )
    .bind(&token_hash[..])
    .bind(command.app_id)
    .bind(conversation.as_uuid())
    .bind(actor.as_uuid())
    .bind(i32::try_from(RESPONSE_MINUTES).unwrap_or(30))
    .execute(&mut *conn)
    .await?;
    drop(conn);
    let response_url = format!("{}/commands/respond/{token}", state.integrations.public_url);
    let host = state
        .integrations
        .public_url
        .split("://")
        .nth(1)
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let trigger = Uuid::new_v4().simple().to_string();
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("token", "")
        .append_pair("team_id", &workspace.to_string())
        .append_pair("team_domain", &host)
        .append_pair("channel_id", &conversation.to_string())
        .append_pair(
            "channel_name",
            channel_name.as_deref().unwrap_or("directmessage"),
        )
        .append_pair("user_id", &actor.to_string())
        .append_pair("user_name", &user_name)
        .append_pair("command", &format!("/{name}"))
        .append_pair("text", &text)
        .append_pair("api_app_id", &command.app_id.to_string())
        .append_pair("response_url", &response_url)
        .append_pair("trigger_id", &trigger)
        .finish();
    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    let signature = slack_signature(secret.expose(), timestamp, &body);
    drop(secret);
    let sent = http
        .post(&command.url)
        .timeout(Duration::from_secs(ANSWER_SECONDS))
        .header("content-type", "application/x-www-form-urlencoded")
        .header("x-slack-request-timestamp", timestamp.to_string())
        .header("x-slack-signature", signature)
        .body(body)
        .send()
        .await;
    let app_name = command.app_name.clone();
    let unreachable = || {
        AppError::new(
            ErrorCode::Unavailable,
            format!("{app_name} didn't answer /{name}. Try again, or tell an admin"),
        )
    };
    let response = sent.map_err(|_| unreachable())?;
    if !response.status().is_success() {
        tracing::warn!(
            app_id = %command.app_id,
            status = response.status().as_u16(),
            "an app refused a slash command"
        );
        return Err(unreachable());
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bytes = response.bytes().await.map_err(|_| unreachable())?;
    if bytes.len() > MAX_ANSWER_BYTES {
        return Err(AppError::invalid(format!(
            "{} answered with too much",
            command.app_name
        )));
    }
    let answer = read_answer(content_type.as_deref(), &bytes)?;
    let bot = UserId::from_uuid(command.bot_user_id);
    let mut out = RunCommandResponse {
        app_name: command.app_name,
        reply: None,
        posted: None,
    };
    match answer {
        Answer::Nothing => {}
        Answer::Private(markdown) => out.reply = Some(document(&markdown)?),
        Answer::InChannel(markdown) => {
            out.posted = Some(
                post_in_channel(
                    state,
                    actor,
                    bot,
                    conversation,
                    &markdown,
                    OperationId::generate(),
                )
                .await?,
            );
        }
    }
    Ok(out)
}

/// `POST /commands/respond/{token}`: a later answer from the app. Posted in the conversation by the
/// app's bot; a private one is refused for now (see the module's notes).
pub async fn respond(
    state: &AppState,
    token: &str,
    body: &[u8],
    content_type: Option<&str>,
) -> Response {
    match respond_inner(state, token, body, content_type).await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(error) => error.into_response(),
    }
}

async fn respond_inner(
    state: &AppState,
    token: &str,
    body: &[u8],
    content_type: Option<&str>,
) -> Result<(), AppError> {
    let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let mut tx = state.pool.begin().await?;
    let found: Option<(Uuid, Uuid, Uuid, i32)> = sqlx::query_as(
        "SELECT r.app_id, r.conversation_id, r.user_id, r.uses FROM command_responses r \
         JOIN apps a ON a.id = r.app_id \
         WHERE r.token_hash = $1 AND r.expires_at > now() AND a.deleted_at IS NULL FOR UPDATE OF r",
    )
    .bind(&hash[..])
    .fetch_optional(&mut *tx)
    .await?;
    let Some((app, conversation, user, uses)) = found else {
        return Err(AppError::not_found(
            "this response address is unknown or has expired",
        ));
    };
    if uses >= RESPONSE_USES {
        return Err(AppError::conflict(format!(
            "a response address takes at most {RESPONSE_USES} answers"
        )));
    }
    let answer = match read_answer(content_type, body)? {
        Answer::Nothing => return Err(AppError::invalid("there is nothing to post")),
        Answer::Private(_) => {
            return Err(AppError::invalid(
                "Lazy can't show a private answer later yet: answer within 3 seconds, or send \
                 response_type \"in_channel\"",
            ));
        }
        Answer::InChannel(markdown) => markdown,
    };
    sqlx::query("UPDATE command_responses SET uses = uses + 1 WHERE token_hash = $1")
        .bind(&hash[..])
        .execute(&mut *tx)
        .await?;
    let bot: Uuid = sqlx::query_scalar("SELECT bot_user_id FROM apps WHERE id = $1")
        .bind(app)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    // The same answer to the same use is posted once: its operation is the token's and the count.
    let operation = OperationId::from_uuid(Uuid::new_v5(
        &Uuid::from_slice(&hash[..16]).unwrap_or_default(),
        (uses + 1).to_string().as_bytes(),
    ));
    post_in_channel(
        state,
        UserId::from_uuid(user),
        UserId::from_uuid(bot),
        ConversationId::from_uuid(conversation),
        &answer,
        operation,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_slacks_documented_example() {
        // From Slack's "Verifying requests from Slack".
        let secret = b"8f742231b10e8888abcd99yyyzzz85a5";
        let body = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        assert_eq!(
            slack_signature(secret, 1_531_420_618, body),
            "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"
        );
    }

    #[test]
    fn only_https_or_this_machine() {
        assert!(url_ok("https://bot.example.com/slack/commands"));
        assert!(url_ok("http://127.0.0.1:8099/cmd"));
        assert!(url_ok("http://localhost/cmd"));
        assert!(!url_ok("http://bot.example.com/cmd"));
        assert!(!url_ok("http://127.0.0.1.evil.example/cmd"));
        assert!(!url_ok("https://"));
        assert!(!url_ok("ftp://x"));
    }

    #[test]
    fn answers_are_read_as_slack_writes_them() {
        assert_eq!(read_answer(None, b"").unwrap(), Answer::Nothing);
        assert_eq!(
            read_answer(Some("text/plain"), b"Deploying *now*").unwrap(),
            Answer::Private("Deploying \\*now\\*".into())
        );
        assert_eq!(
            read_answer(Some("application/json"), br#"{"text": "Done"}"#).unwrap(),
            Answer::Private("Done".into())
        );
        assert_eq!(
            read_answer(
                None,
                br#"{"response_type": "in_channel", "text": "<!here> shipped"}"#
            )
            .unwrap(),
            Answer::InChannel("\\@here shipped".into())
        );
        assert!(read_answer(Some("application/json"), b"{nope").is_err());
    }
}
