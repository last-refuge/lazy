//! Button presses (llm-docs/DECISIONS.md, D40), sent to the app as Slack sends `block_actions`, so
//! an app written for Slack's interactivity answers Lazy's with its address and secret changed.
//!
//! # Checks on every press
//!
//! A press names a message, a block, and an action id. The server checks, each time: the person
//! can see the message and post in its conversation now (a member, not archived); the message is
//! not deleted; the button is in the message's current rich content and is an action, not a link;
//! the message's author is the bot of a live app with an interactivity address and a signing
//! secret. The button's `value` comes from the stored message, never from the client. Presses
//! count against the app's rate (the bot API's, D35).
//!
//! # The request and the answer
//!
//! A form with one field, `payload`, holding Slack's `block_actions` JSON (`user`, `team`,
//! `channel`, `container`, `message`, `actions`, `response_url`, `trigger_id`), signed like a
//! slash command (D36) and with the same 3 seconds and answer rules. An app's answer, or a later
//! one through `response_url`, can also `replace_original` (the message is edited by the app's
//! bot, with new text and blocks) or `delete_original`.

use lazy_domain::{AppId, MessageId, OperationId, RichContent, UserId};
use lazy_protocol::{
    EditMessageRequest, ErrorCode, PressButtonRequest, RunCommandResponse,
    SetAppInteractivityRequest,
};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::app::AppState;
use crate::commands::{self, Answer};
use crate::conversations::{self, Lock};
use crate::credentials::{self, CredentialId};
use crate::error::AppError;
use crate::messages;

/// `PUT /v1/apps/{id}/interactivity`: where presses go, or nowhere for an empty `url`. Makes the
/// app's signing secret if it has none, so presses are always signed.
pub async fn set_interactivity(
    state: &AppState,
    actor: UserId,
    app: AppId,
    request: SetAppInteractivityRequest,
) -> Result<lazy_protocol::App, AppError> {
    let url = request.url.trim().to_owned();
    if url.is_empty() {
        crate::apps::get(state, actor, app).await?;
        sqlx::query("UPDATE apps SET interactivity_url = NULL WHERE id = $1")
            .bind(app.as_uuid())
            .execute(&state.pool)
            .await?;
        return crate::apps::get(state, actor, app).await;
    }
    if !commands::url_ok(&url) {
        return Err(AppError::invalid(
            "the address must start with https:// (or http://localhost)",
        ));
    }
    // Checks the caller is the app's admin, and makes the secret if there is none yet.
    commands::signing_secret(state, actor, app).await?;
    sqlx::query("UPDATE apps SET interactivity_url = $2 WHERE id = $1")
        .bind(app.as_uuid())
        .bind(&url)
        .execute(&state.pool)
        .await?;
    crate::apps::get(state, actor, app).await
}

#[derive(sqlx::FromRow)]
struct AppRow {
    id: Uuid,
    name: String,
    signing_secret_id: Option<Uuid>,
    interactivity_url: Option<String>,
}

fn gone() -> AppError {
    AppError::not_found("that button is no longer on the message")
}

/// `POST /v1/messages/{id}/actions`: a press of a button on a message.
pub async fn press(
    state: &AppState,
    actor: UserId,
    id: MessageId,
    request: PressButtonRequest,
) -> Result<RunCommandResponse, AppError> {
    // Visible to the caller now, or not found.
    let message = messages::get_message(&state.pool, actor, id).await?;
    if message.deleted {
        return Err(gone());
    }
    let (tx, loaded) =
        conversations::begin_visible(&state.pool, actor, message.conversation_id, Lock::Shared)
            .await?;
    drop(tx);
    if !loaded.access().can_post() {
        return Err(AppError::forbidden(
            "you can't use buttons in this conversation",
        ));
    }
    let block = usize::try_from(request.block).unwrap_or(usize::MAX);
    let rich = message.rich.as_ref().ok_or_else(gone)?;
    let button = rich
        .button(block, &request.action_id)
        .filter(|button| button.is_action())
        .ok_or_else(gone)?
        .clone();
    let app: Option<AppRow> = sqlx::query_as(
        "SELECT id, name, signing_secret_id, interactivity_url FROM apps \
         WHERE bot_user_id = $1 AND deleted_at IS NULL",
    )
    .bind(message.author_id.as_uuid())
    .fetch_optional(&state.pool)
    .await?;
    let Some(app) = app else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            "the app that sent this message is no longer installed",
        ));
    };
    let (Some(url), Some(secret_id)) = (&app.interactivity_url, app.signing_secret_id) else {
        return Err(AppError::new(
            ErrorCode::Unavailable,
            format!("{} doesn't take button presses", app.name),
        ));
    };
    if let Err(refused) = state
        .integrations
        .app_limiter
        .take(crate::apps::limiter_key(AppId::from_uuid(app.id)))
    {
        return Err(AppError::new(
            ErrorCode::RateLimited,
            format!(
                "{} is busy; try again in {} seconds",
                app.name,
                refused.wait.as_secs().max(1)
            ),
        ));
    }
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
    .bind(message.conversation_id.as_uuid())
    .fetch_one(&mut *conn)
    .await?;
    let response_url = commands::response_address(
        state,
        &mut conn,
        app.id,
        message.conversation_id,
        actor,
        Some(id),
    )
    .await?;
    drop(conn);

    let workspace = loaded.workspace_id.to_string();
    let channel = message.conversation_id.to_string();
    let now = OffsetDateTime::now_utc();
    let action_ts = format!("{}.{:06}", now.unix_timestamp(), now.nanosecond() / 1_000);
    let block_id = rich
        .block_id(block)
        .map_or_else(|| block.to_string(), str::to_owned);
    let mut action = json!({
        "type": "button",
        "action_id": button.action_id,
        "block_id": block_id,
        "text": {"type": "plain_text", "text": button.text},
        "action_ts": action_ts,
    });
    if let Some(value) = &button.value {
        action["value"] = json!(value);
    }
    match button.style {
        lazy_domain::ButtonStyle::Primary => action["style"] = json!("primary"),
        lazy_domain::ButtonStyle::Danger => action["style"] = json!("danger"),
        lazy_domain::ButtonStyle::Default => {}
    }
    let mut shown = json!({
        "type": "message",
        "ts": id.to_string(),
        "user": message.author_id.to_string(),
        "text": message.document.plain_text(),
    });
    if let Some(root) = message.thread_root {
        shown["thread_ts"] = json!(root.to_string());
    }
    let payload = json!({
        "type": "block_actions",
        "token": "",
        "api_app_id": app.id.to_string(),
        "team": {"id": workspace, "domain": commands::public_host(state)},
        "user": {"id": actor.to_string(), "username": user_name, "name": user_name,
                 "team_id": workspace},
        "channel": {"id": channel, "name": channel_name.as_deref().unwrap_or("directmessage")},
        "container": {"type": "message", "message_ts": id.to_string(), "channel_id": channel,
                      "is_ephemeral": false},
        "message": shown,
        "trigger_id": request.press_id.to_string(),
        "response_url": response_url,
        "actions": [action],
    });
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("payload", &payload.to_string())
        .finish();
    let unreachable = AppError::new(
        ErrorCode::Unavailable,
        format!(
            "{} didn't answer the button. Try again, or tell an admin",
            app.name
        ),
    );
    let answer =
        commands::call_app(http, url, secret, body, (app.id, &app.name), unreachable).await?;

    let bot = message.author_id;
    let mut out = RunCommandResponse {
        app_name: app.name,
        reply: None,
        posted: None,
    };
    // The same press answered twice posts once: its operation is the press's.
    let operation = OperationId::from_uuid(Uuid::new_v5(&request.press_id.as_uuid(), b"answer"));
    match answer {
        Answer::Nothing => {}
        Answer::Private(markdown) => out.reply = Some(commands::document(&markdown)?),
        Answer::InChannel(markdown, rich) => {
            out.posted = Some(
                commands::post_in_channel(
                    state,
                    actor,
                    bot,
                    message.conversation_id,
                    (&markdown, rich),
                    operation,
                )
                .await?,
            );
        }
        Answer::Replace(markdown, rich) => {
            replace(state, bot, id, (&markdown, rich), operation).await?;
        }
        Answer::Delete => {
            messages::delete_message(&state.pool, bot, id).await?;
        }
    }
    Ok(out)
}

/// Replaces a message of an app's bot with new text and blocks (`replace_original`), as an edit
/// by the bot, so it is checked and announced like any edit.
pub(crate) async fn replace(
    state: &AppState,
    bot: UserId,
    id: MessageId,
    (markdown, rich): (&str, Option<RichContent>),
    operation: OperationId,
) -> Result<(), AppError> {
    let current = messages::get_message(&state.pool, bot, id).await?;
    if current.deleted {
        return Err(messages::message_not_found());
    }
    if current.author_id != bot {
        return Err(AppError::forbidden(
            "an app can replace only its own messages",
        ));
    }
    let request = EditMessageRequest {
        operation_id: operation,
        base_revision: current.revision,
        document: commands::document(markdown)?,
    };
    messages::edit_message_with(&state.pool, bot, id, request, rich).await?;
    Ok(())
}
