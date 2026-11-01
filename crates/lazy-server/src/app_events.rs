//! Events for apps (llm-docs/DECISIONS.md, D38): an app is told what happens in its channels, as
//! Slack's Events API tells a Slack app, so a program written for Slack's events (an assistant, an
//! automation) works with its address and secret changed.
//!
//! # Subscribing
//!
//! An admin gives the app a request address and chooses its events: `message` (each message a
//! person posts in a channel the bot is in) and `app_mention` (a message that names it,
//! `@App name`). Before the address is saved it must answer Slack's challenge: a signed
//! `{"type": "url_verification", "challenge": ...}` answered with the challenge, as text or as
//! `{"challenge": ...}`. A tool that cannot answer one (Zapier's, n8n's, or Make's catch hooks)
//! can be saved without it; its deliveries are signed all the same.
//!
//! # Delivering
//!
//! A message is queued for each subscribed app whose bot is in its channel in the same transaction
//! that posts it, so an event exists exactly when the message does. Messages by bots (any app,
//! webhook, or connector) are not sent, so two apps cannot answer each other forever. Each is one
//! `app.event` job: a signed `POST` (as commands, D36) of Slack's `event_callback` body with
//! Slack's event fields (`channel`, `user`, `text`, `ts`, `thread_ts`), and Lazy's names for the
//! same (`message_id`, `channel_name`). Mentions are plain text: Lazy does not notify anyone for
//! `@App name`, it only tells the app. A `2xx` within 3 seconds is delivered; anything else is
//! tried again (with `X-Slack-Retry-Num`), up to 4 times.

use std::time::Duration;

use lazy_domain::{AppId, ConversationId, UserId, WorkspaceId};
use lazy_protocol::{AppEvents, ErrorCode, Message, SetAppEventsRequest};
use serde_json::{Value, json};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::AppState;
use crate::credentials::{self, CredentialId, CredentialKeys};
use crate::error::AppError;
use crate::jobs::{self, JobContext, NewJob, Outcome};

/// The job that delivers one event.
pub const EVENT_JOB: &str = "app.event";
/// The event types an app can choose.
pub const TYPES: [&str; 2] = ["message", "app_mention"];
const ANSWER_SECONDS: u64 = 3;

/// What a delivery needs beyond the database: the keys that open the signing secret, and an
/// HTTPS client.
#[derive(Clone)]
pub struct Services {
    pub keys: CredentialKeys,
    pub http: Option<reqwest::Client>,
}

fn random_hex() -> Result<String, AppError> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| AppError::internal(format!("no randomness: {e}")))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sends a signed JSON body to an app; answers its status and body, or why it could not.
async fn send(
    http: &reqwest::Client,
    url: &str,
    secret: &[u8],
    body: &str,
    retry: Option<u32>,
) -> Result<(u16, String), String> {
    let timestamp = OffsetDateTime::now_utc().unix_timestamp();
    let mut request = http
        .post(url)
        .timeout(Duration::from_secs(ANSWER_SECONDS))
        .header("content-type", "application/json")
        .header("x-slack-request-timestamp", timestamp.to_string())
        .header(
            "x-slack-signature",
            crate::commands::slack_signature(secret, timestamp, body),
        )
        .body(body.to_owned());
    if let Some(n) = retry {
        request = request
            .header("x-slack-retry-num", n.to_string())
            .header("x-slack-retry-reason", "http_error");
    }
    let response = request
        .send()
        .await
        .map_err(|_| "the app did not answer in time".to_owned())?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    Ok((status, text))
}

/// Whether an answer to the challenge echoes it, as text or as `{"challenge": ...}`.
fn echoes(answer: &str, challenge: &str) -> bool {
    let answer = answer.trim();
    answer == challenge
        || serde_json::from_str::<Value>(answer)
            .ok()
            .is_some_and(|v| v["challenge"] == challenge)
}

/// Sends Slack's challenge to an events address and checks the answer echoes it.
async fn check_address(http: &reqwest::Client, url: &str, secret: &[u8]) -> Result<(), AppError> {
    let challenge = random_hex()?;
    let body = json!({"token": "", "challenge": challenge, "type": "url_verification"}).to_string();
    let answered = send(http, url, secret, &body, None).await;
    match answered {
        Ok((status, text)) if (200..300).contains(&status) && echoes(&text, &challenge) => {}
        Ok((status, _)) if !(200..300).contains(&status) => {
            return Err(AppError::invalid(format!(
                "the events address answered the challenge with status {status}"
            )));
        }
        Ok(_) => {
            return Err(AppError::invalid(
                "the events address did not answer with the challenge",
            ));
        }
        Err(why) => return Err(AppError::invalid(format!("the events address: {why}"))),
    }
    Ok(())
}

/// Sets (or, with an empty address, turns off) an app's events, after its address answers the
/// challenge. Admins and owners.
pub async fn set_events(
    state: &AppState,
    actor: UserId,
    app: AppId,
    request: SetAppEventsRequest,
) -> Result<lazy_protocol::App, AppError> {
    let url = request.url.trim().to_owned();
    if url.is_empty() {
        // Checked as its admin by `get`, then cleared.
        crate::apps::get(state, actor, app).await?;
        sqlx::query("UPDATE apps SET event_url = NULL, event_types = '{}' WHERE id = $1")
            .bind(app.as_uuid())
            .execute(&state.pool)
            .await?;
        return crate::apps::get(state, actor, app).await;
    }
    if !crate::commands::url_ok(&url) {
        return Err(AppError::invalid(
            "the events address must start with https:// (or http://localhost)",
        ));
    }
    let mut types: Vec<String> = request
        .types
        .into_iter()
        .filter(|t| TYPES.contains(&t.as_str()))
        .collect();
    types.sort_unstable();
    types.dedup();
    if types.is_empty() {
        return Err(AppError::invalid("choose at least one kind of event"));
    }
    // Checks the caller is the app's admin, and makes the secret if there is none yet.
    let secret = crate::commands::signing_secret(state, actor, app).await?;
    let Some(http) = &state.integrations.http else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "this server cannot make HTTPS requests",
        ));
    };
    if !request.skip_check {
        check_address(http, &url, secret.as_bytes()).await?;
    }
    sqlx::query("UPDATE apps SET event_url = $2, event_types = $3 WHERE id = $1")
        .bind(app.as_uuid())
        .bind(&url)
        .bind(&types)
        .execute(&state.pool)
        .await?;
    crate::apps::get(state, actor, app).await
}

/// An app's events as its admin sees them.
pub fn events_of(url: Option<String>, types: Vec<String>) -> Option<AppEvents> {
    url.map(|url| AppEvents { url, types })
}

/// Whether `text` names the app: `@` and its name (or its name without spaces), not followed by a
/// letter or digit.
fn names(text: &str, app_name: &str) -> bool {
    let lower = text.to_lowercase();
    let full = app_name.trim().to_lowercase();
    let squeezed: String = full.chars().filter(|c| !c.is_whitespace()).collect();
    [full, squeezed]
        .iter()
        .filter(|n| !n.is_empty())
        .any(|name| {
            let needle = format!("@{name}");
            lower.match_indices(&needle).any(|(at, _)| {
                let before_ok = lower[..at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric());
                let after_ok = lower[at + needle.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric());
                before_ok && after_ok
            })
        })
}

/// Queues the events a message makes, in the transaction that posts it. A message by a bot makes
/// none.
pub async fn message_posted(
    tx: &mut PgConnection,
    workspace: WorkspaceId,
    conversation: ConversationId,
    message: &Message,
) -> Result<(), AppError> {
    let apps: Vec<(Uuid, String, Uuid, Vec<String>)> = sqlx::query_as(
        "SELECT a.id, a.name, a.bot_user_id, a.event_types FROM apps a \
         JOIN conversation_members m ON m.user_id = a.bot_user_id AND m.conversation_id = $2 \
         WHERE a.workspace_id = $1 AND a.deleted_at IS NULL AND a.event_url IS NOT NULL",
    )
    .bind(workspace.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_all(&mut *tx)
    .await?;
    if apps.is_empty() {
        return Ok(());
    }
    let (author_is_bot, user_name, channel_name): (bool, String, Option<String>) = sqlx::query_as(
        "SELECT u.is_bot, u.username, c.name FROM users u, conversations c \
         WHERE u.id = $1 AND c.id = $2",
    )
    .bind(message.author_id.as_uuid())
    .bind(conversation.as_uuid())
    .fetch_one(&mut *tx)
    .await?;
    if author_is_bot {
        return Ok(());
    }
    let text = message.document.plain_text();
    let now = OffsetDateTime::now_utc().unix_timestamp();
    for (app, name, bot, types) in apps {
        if bot == message.author_id.as_uuid() {
            continue;
        }
        let kind = if types.iter().any(|t| t == "app_mention") && names(&text, &name) {
            "app_mention"
        } else if types.iter().any(|t| t == "message") {
            "message"
        } else {
            continue;
        };
        let event_id = format!(
            "Ev{}",
            Uuid::new_v5(&app, message.id.to_string().as_bytes()).simple()
        );
        let mut event = json!({
            "type": kind,
            "channel": conversation.to_string(),
            "channel_type": "channel",
            "channel_name": channel_name,
            "user": message.author_id.to_string(),
            "user_name": user_name,
            "text": text,
            "ts": message.id.to_string(),
            "event_ts": message.id.to_string(),
            "message_id": message.id.to_string(),
        });
        if let Some(root) = message.thread_root {
            event["thread_ts"] = json!(root.to_string());
        }
        let payload = json!({
            "app_id": app.to_string(),
            "body": {
                "token": "",
                "team_id": workspace.to_string(),
                "api_app_id": app.to_string(),
                "type": "event_callback",
                "event_id": event_id,
                "event_time": now,
                "event": event,
            },
        });
        jobs::enqueue(
            &mut *tx,
            NewJob::new(EVENT_JOB, payload)
                .workspace(workspace)
                .dedupe(event_id)
                .max_attempts(4),
        )
        .await?;
    }
    Ok(())
}

/// An app's events address and signing secret, if it still has both.
type Delivery = (Option<String>, Option<Uuid>);

/// Delivers one event. The job's handler.
pub async fn handle_event(services: Services, ctx: JobContext) -> Outcome {
    let Some(app) = ctx.payload["app_id"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
    else {
        return Outcome::fail("the stored event could not be read");
    };
    let row: Result<Option<Delivery>, sqlx::Error> = sqlx::query_as(
        "SELECT event_url, signing_secret_id FROM apps WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(app)
    .fetch_optional(&ctx.pool)
    .await;
    let (url, secret_id) = match row {
        Ok(Some((Some(url), Some(secret)))) => (url, secret),
        Ok(_) => return Outcome::fail("the app no longer takes events"),
        Err(_) => return Outcome::retry("the database failed"),
    };
    let Some(http) = &services.http else {
        return Outcome::fail("this server cannot make HTTPS requests");
    };
    let secret = {
        let Ok(mut conn) = ctx.pool.acquire().await else {
            return Outcome::retry("the database failed");
        };
        match credentials::load(&mut conn, &services.keys, CredentialId(secret_id)).await {
            Ok(secret) => secret,
            Err(_) => return Outcome::fail("the app's signing secret could not be opened"),
        }
    };
    let body = ctx.payload["body"].to_string();
    let retry = (ctx.attempt > 1).then(|| ctx.attempt - 1);
    match send(http, &url, secret.expose(), &body, retry).await {
        Ok((status, _)) if (200..300).contains(&status) => Outcome::Done,
        Ok((status, _)) => Outcome::retry(format!("the app answered with status {status}")),
        Err(why) => Outcome::retry(why),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_app_is_named_by_its_name_alone() {
        assert!(names("hey @Claude what's up", "Claude"));
        assert!(names("@deploy bot run it", "Deploy bot"));
        assert!(names("ask @deploybot.", "Deploy bot"));
        assert!(!names("@Claudette hi", "Claude"));
        assert!(!names("mail me@claude.ai", "Claude"));
        assert!(!names("Claude, hi", "Claude"));
    }

    #[test]
    fn the_challenge_is_echoed_as_text_or_json() {
        assert!(echoes("abc", "abc"));
        assert!(echoes(" {\"challenge\": \"abc\"} ", "abc"));
        assert!(!echoes("{\"challenge\": \"xyz\"}", "abc"));
        assert!(!echoes("", "abc"));
    }
}
