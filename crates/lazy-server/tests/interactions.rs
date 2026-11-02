//! Rich messages and button presses (D40), end to end: blocks from a webhook and from the bot API
//! kept beside their fallback text, presses sent to a stand-in app that checks Slack's signature,
//! its answers (private, posted, later, replacing and deleting the message), and the checks made
//! on every press.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use common::*;
use lazy_domain::app::AppScope;
use lazy_domain::{ButtonStyle, MessageId, OperationId, RichBlock};
use lazy_protocol::api::paths;
use lazy_protocol::{
    AppSigningSecretResponse, BotMessageResponse, CreatedAppResponse, ErrorCode, RunCommandResponse,
};
use lazy_server::IntegrationOptions;
use lazy_server::credentials::CredentialKeys;
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Stand {
    secret: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<Value>>>,
}

async fn answer(
    State(stand): State<Stand>,
    headers: HeaderMap,
    body: String,
) -> axum::response::Response {
    let secret = stand.secret.lock().unwrap().clone();
    let timestamp: i64 = headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or_default();
    let expected = lazy_server::commands::slack_signature(secret.as_bytes(), timestamp, &body);
    if headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        != Some(expected.as_str())
    {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let payload: Value = form_urlencoded::parse(body.as_bytes())
        .find(|(k, _)| k == "payload")
        .and_then(|(_, v)| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    stand.seen.lock().unwrap().push(payload.clone());
    let url = payload["response_url"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    match payload["actions"][0]["action_id"]
        .as_str()
        .unwrap_or_default()
    {
        "ping" => "pong *privately*".into_response(),
        "approve" => axum::Json(json!({
            "replace_original": true,
            "text": "Approved",
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn", "text": "*Approved* by the button"}},
                {"type": "context", "elements": [{"type": "mrkdwn", "text": "deploy 42"}]}
            ]
        }))
        .into_response(),
        "later" => {
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let _ = client
                    .post(&url)
                    .json(&json!({"response_type": "in_channel", "text": "Done later"}))
                    .send()
                    .await;
                let _ = client
                    .post(&url)
                    .json(&json!({"delete_original": true}))
                    .send()
                    .await;
            });
            StatusCode::OK.into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
    }
}

async fn stand_in(stand: Stand) -> String {
    let router = Router::new()
        .route("/interact", post(answer))
        .with_state(stand);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/interact")
}

struct Team {
    app: App,
    boss: Account,
    alice: Account,
}

async fn team(pool: PgPool) -> Team {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            credential_keys: CredentialKeys::from_bytes([9; 32], &[]),
            ..IntegrationOptions::default()
        },
    )
    .await;
    let _owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    Team { app, boss, alice }
}

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn bot_post(app: &App, token: &str, body: Value) -> MessageId {
    let response = app
        .http
        .post(app.url(paths::BOT_MESSAGES_ROUTE))
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    response
        .json::<BotMessageResponse>()
        .await
        .unwrap()
        .message_id
}

async fn press(
    app: &App,
    who: &Account,
    message: MessageId,
    block: u32,
    action: &str,
    press_id: OperationId,
) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::message_actions(message),
        who,
        Some(json!({"block": block, "action_id": action, "press_id": press_id})),
    )
    .await
}

async fn set(app: &App, app_id: lazy_domain::AppId, who: &Account, url: &str) -> reqwest::Response {
    app.call(
        reqwest::Method::PUT,
        &paths::app_interactivity(app_id),
        who,
        Some(json!({ "url": url })),
    )
    .await
}

fn deploy_blocks(channel: &str) -> Value {
    json!({
        "channel": channel,
        "text": "Deploy 42 is waiting",
        "blocks": [
            {"type": "header", "text": {"type": "plain_text", "text": "Deploy 42"}},
            {"type": "actions", "block_id": "deploy", "elements": [
                {"type": "button", "action_id": "approve", "style": "primary", "value": "42",
                 "text": {"type": "plain_text", "text": "Approve"}},
                {"type": "button", "action_id": "ping", "text": {"type": "plain_text", "text": "Ping"}},
                {"type": "button", "action_id": "later", "text": {"type": "plain_text", "text": "Later"}},
                {"type": "button", "action_id": "fail", "text": {"type": "plain_text", "text": "Fail"}},
                {"type": "button", "action_id": "open", "url": "https://ci.example/42",
                 "text": {"type": "plain_text", "text": "Open"}}
            ]}
        ]
    })
}

async fn wait_for(app: &App, what: &str, sql: &'static str, bind: uuid::Uuid) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found: bool = sqlx::query_scalar(sql)
            .bind(bind)
            .fetch_one(&app.pool)
            .await
            .unwrap();
        if found {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[sqlx::test]
async fn a_webhook_keeps_blocks_beside_its_text_and_only_link_buttons(pool: PgPool) {
    let t = team(pool).await;
    let app = &t.app;
    let hook = app.create_webhook(&t.boss, app.general, "CI").await;
    let body = deploy_blocks("ignored").to_string();
    let response = app.deliver_body_raw(&hook.url, body, &[]).await;
    assert!(response.status().is_success(), "{}", response.status());
    let id = response
        .json::<lazy_protocol::IncomingWebhookResponse>()
        .await
        .unwrap()
        .message_id;
    let message = app.get_message(&t.alice, id).await;
    // The fallback text is the blocks as text, for search, notifications, and older clients.
    assert!(message.document.plain_text().starts_with("Deploy 42"));
    let rich = message.rich.expect("rich content");
    assert_eq!(
        rich.blocks()[0],
        RichBlock::Header {
            text: "Deploy 42".into()
        }
    );
    // Nothing would answer a press, so only the link button is kept.
    assert!(!rich.has_actions());
    assert!(rich.button(1, "open").is_some());
    denied(
        press(app, &t.alice, id, 1, "open", OperationId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;

    // Text alone is not rich.
    let plain = app.deliver(&hook, "just words").await;
    assert!(app.get_message(&t.alice, plain).await.rich.is_none());
}

#[sqlx::test]
async fn a_press_goes_to_the_app_signed_and_is_checked_every_time(pool: PgPool) {
    let t = team(pool).await;
    let app = &t.app;
    let stand = Stand::default();
    let url = stand_in(stand.clone()).await;
    let made: CreatedAppResponse = app
        .call(
            reqwest::Method::POST,
            &paths::apps(app.workspace_id),
            &t.boss,
            Some(json!({"name": "Deployer", "scopes": [AppScope::ChatWrite]})),
        )
        .await
        .json()
        .await
        .unwrap();
    let app_id = made.app.id;
    app.call(
        reqwest::Method::PUT,
        &paths::app_channel(app_id, app.general),
        &t.boss,
        None,
    )
    .await;
    let first = bot_post(app, &made.token, deploy_blocks("general")).await;
    let message = app.get_message(&t.alice, first).await;
    let rich = message.rich.clone().expect("rich content");
    let approve = rich.button(1, "approve").unwrap();
    assert_eq!(approve.style, ButtonStyle::Primary);
    assert_eq!(approve.value.as_deref(), Some("42"));

    // No address yet: the app doesn't take presses.
    denied(
        press(app, &t.alice, first, 1, "ping", OperationId::generate()).await,
        ErrorCode::Unavailable,
    )
    .await;
    // Only its admin sets the address, which must be https (or this machine).
    denied(set(app, app_id, &t.alice, &url).await, ErrorCode::Forbidden).await;
    denied(
        set(app, app_id, &t.boss, "http://example.com/x").await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let saved: lazy_protocol::App = set(app, app_id, &t.boss, &url).await.json().await.unwrap();
    assert_eq!(saved.interactivity_url.as_deref(), Some(url.as_str()));
    let secret: AppSigningSecretResponse = app
        .call(
            reqwest::Method::GET,
            &paths::app_signing_secret(app_id),
            &t.boss,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    *stand.secret.lock().unwrap() = secret.secret;

    // A link, an unknown action, or the wrong block is not a press.
    for (block, action) in [(1, "open"), (1, "nope"), (0, "ping"), (9, "ping")] {
        denied(
            press(app, &t.alice, first, block, action, OperationId::generate()).await,
            ErrorCode::NotFound,
        )
        .await;
    }

    // A private answer comes back to the presser, and nothing is posted.
    let head = app.head().await;
    let press_id = OperationId::generate();
    let ping: RunCommandResponse = press(app, &t.alice, first, 1, "ping", press_id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(ping.app_name, "Deployer");
    assert_eq!(ping.reply.unwrap().plain_text(), "pong *privately*");
    assert_eq!(app.head().await, head, "nothing was posted");
    // The app was sent Slack's block_actions, with the stored value and the press's id.
    let seen = stand.seen.lock().unwrap().last().cloned().unwrap();
    assert_eq!(seen["type"], "block_actions");
    assert_eq!(seen["user"]["username"], "alice");
    assert_eq!(seen["channel"]["name"], "general");
    assert_eq!(seen["container"]["message_ts"], first.to_string());
    assert_eq!(seen["message"]["text"], message.document.plain_text());
    assert_eq!(seen["trigger_id"], press_id.to_string());
    assert_eq!(seen["actions"][0]["block_id"], "deploy");
    assert_eq!(seen["actions"][0]["action_id"], "ping");
    assert!(
        seen["response_url"]
            .as_str()
            .unwrap()
            .contains("/commands/respond/")
    );

    // An app that fails is reported to the presser.
    denied(
        press(app, &t.alice, first, 1, "fail", OperationId::generate()).await,
        ErrorCode::Unavailable,
    )
    .await;

    // Someone who cannot see the conversation cannot press: a private channel alice is not in.
    let secret_room = app.create_channel(&t.boss, "secret", "private").await.id;
    app.call(
        reqwest::Method::PUT,
        &paths::app_channel(app_id, secret_room),
        &t.boss,
        None,
    )
    .await;
    let hidden = bot_post(app, &made.token, deploy_blocks("secret")).await;
    denied(
        press(app, &t.alice, hidden, 1, "ping", OperationId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;
    let boss_ping = press(app, &t.boss, hidden, 1, "ping", OperationId::generate()).await;
    assert!(boss_ping.status().is_success());

    // A later answer through response_url is posted, and can delete the message pressed on.
    let second = bot_post(app, &made.token, deploy_blocks("general")).await;
    let later = press(app, &t.alice, second, 1, "later", OperationId::generate()).await;
    assert!(later.status().is_success());
    wait_for(
        app,
        "the later answer",
        "SELECT EXISTS (SELECT 1 FROM messages WHERE author_id = $1 AND plain_text = 'Done later')",
        made.app.bot_user_id.as_uuid(),
    )
    .await;
    wait_for(
        app,
        "the deletion",
        "SELECT deleted FROM messages WHERE id = $1",
        second.as_uuid(),
    )
    .await;
    let gone = app.get_message(&t.alice, second).await;
    assert!(gone.rich.is_none(), "a deleted message keeps no blocks");
    denied(
        press(app, &t.alice, second, 1, "ping", OperationId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;

    // replace_original edits the message as the bot: new text and blocks, no buttons left.
    let approved: RunCommandResponse =
        press(app, &t.alice, first, 1, "approve", OperationId::generate())
            .await
            .json()
            .await
            .unwrap();
    assert!(approved.reply.is_none() && approved.posted.is_none());
    let replaced = app.get_message(&t.alice, first).await;
    assert_eq!(replaced.revision, 2);
    assert!(replaced.edited_at.is_some());
    assert_eq!(
        replaced.document.plain_text(),
        "Approved by the button\n\ndeploy 42"
    );
    let rich = replaced.rich.expect("new blocks");
    assert!(!rich.has_actions());
    assert!(matches!(rich.blocks()[1], RichBlock::Context { .. }));
    // The old buttons are gone with the old blocks.
    denied(
        press(app, &t.alice, first, 1, "approve", OperationId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;

    // Turning presses off.
    let third = bot_post(app, &made.token, deploy_blocks("general")).await;
    set(app, app_id, &t.boss, "").await;
    denied(
        press(app, &t.alice, third, 1, "ping", OperationId::generate()).await,
        ErrorCode::Unavailable,
    )
    .await;
}
