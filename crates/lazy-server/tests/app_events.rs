//! Events for apps (D38), end to end against a stand-in app server: the challenge before an
//! address is saved, signed `event_callback` deliveries of messages and mentions, no events for
//! bots' messages, retries, and turning events off.

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
use lazy_protocol::api::paths;
use lazy_protocol::{AppSigningSecretResponse, CreatedAppResponse, ErrorCode};
use lazy_server::IntegrationOptions;
use lazy_server::credentials::CredentialKeys;
use lazy_server::jobs::WorkerOptions;
use serde_json::{Value, json};
use sqlx::PgPool;

/// One event as received, with the retry number it came with.
type Received = (Value, Option<String>);

#[derive(Clone, Default)]
struct Stand {
    secret: Arc<Mutex<String>>,
    /// Events received, with the retry number they came with.
    events: Arc<Mutex<Vec<Received>>>,
    /// How many deliveries to refuse before taking one.
    refuse: Arc<Mutex<u32>>,
    /// Whether to answer the challenge wrongly.
    wrong: Arc<Mutex<bool>>,
}

async fn receive(
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
    let value: Value = serde_json::from_str(&body).unwrap();
    if value["type"] == "url_verification" {
        if *stand.wrong.lock().unwrap() {
            return "nope".into_response();
        }
        return axum::Json(json!({"challenge": value["challenge"]})).into_response();
    }
    {
        let mut refuse = stand.refuse.lock().unwrap();
        if *refuse > 0 {
            *refuse -= 1;
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    }
    let retry = headers
        .get("x-slack-retry-num")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    stand.events.lock().unwrap().push((value, retry));
    StatusCode::OK.into_response()
}

async fn stand_in(stand: Stand) -> String {
    let router = Router::new()
        .route("/events", post(receive))
        .with_state(stand);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/events")
}

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// Waits until the stand-in has `n` events.
async fn until_events(stand: &Stand, n: usize) -> Vec<Received> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = stand.events.lock().unwrap().clone();
        if events.len() >= n {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {n} events arrived",
            events.len()
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[sqlx::test]
async fn an_app_is_told_what_happens_in_its_channels(pool: PgPool) {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            credential_keys: CredentialKeys::from_bytes([4; 32], &[]),
            jobs: Some(WorkerOptions {
                poll: Duration::from_millis(50),
                backoff_base: Duration::from_millis(50),
                backoff_cap: Duration::from_millis(200),
                ..WorkerOptions::default()
            }),
            ..IntegrationOptions::default()
        },
    )
    .await;
    let _owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    let stand = Stand::default();
    let url = stand_in(stand.clone()).await;

    let made: CreatedAppResponse = app
        .call(
            reqwest::Method::POST,
            &paths::apps(app.workspace_id),
            &boss,
            Some(json!({"name": "Helper", "scopes": [AppScope::ChatWrite]})),
        )
        .await
        .json()
        .await
        .unwrap();
    let id = made.app.id;
    let secret: AppSigningSecretResponse = app
        .call(
            reqwest::Method::GET,
            &paths::app_signing_secret(id),
            &boss,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    *stand.secret.lock().unwrap() = secret.secret;
    let in_general = app
        .call(
            reqwest::Method::PUT,
            &paths::app_channel(id, app.general),
            &boss,
            None,
        )
        .await;
    assert!(in_general.status().is_success());

    // The address must answer the challenge; members cannot set it.
    let events_path = paths::app_events(id);
    let set = |types: Value| {
        app.call(
            reqwest::Method::PUT,
            &events_path,
            &boss,
            Some(json!({"url": url, "types": types})),
        )
    };
    *stand.wrong.lock().unwrap() = true;
    denied(set(json!(["message"])).await, ErrorCode::InvalidRequest).await;
    // A tool that cannot answer the challenge is saved when the check is skipped.
    let skipped = app
        .call(
            reqwest::Method::PUT,
            &events_path,
            &boss,
            Some(json!({"url": url, "types": ["message"], "skip_check": true})),
        )
        .await;
    assert!(skipped.status().is_success(), "{}", skipped.status());
    *stand.wrong.lock().unwrap() = false;
    denied(set(json!(["nonsense"])).await, ErrorCode::InvalidRequest).await;
    denied(
        app.call(
            reqwest::Method::PUT,
            &paths::app_events(id),
            &alice,
            Some(json!({"url": url, "types": ["message"]})),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    let saved: lazy_protocol::App = set(json!(["message"])).await.json().await.unwrap();
    assert_eq!(saved.events.unwrap().types, ["message"]);

    // A person's message is delivered once, signed, in Slack's shape; a refusal is retried.
    *stand.refuse.lock().unwrap() = 1;
    let posted = app.send(&alice, app.general, "Deploy please").await;
    let events = until_events(&stand, 1).await;
    let (body, retry) = &events[0];
    assert_eq!(body["type"], "event_callback");
    assert_eq!(body["api_app_id"], id.to_string());
    let event = &body["event"];
    assert_eq!(event["type"], "message");
    assert_eq!(event["channel"], app.general.to_string());
    assert_eq!(event["channel_name"], "general");
    assert_eq!(event["user_name"], "alice");
    assert_eq!(event["text"], "Deploy please");
    assert_eq!(event["ts"], posted.id.to_string());
    assert_eq!(retry.as_deref(), Some("1"), "it was the retry that arrived");

    // The bot's own messages, and any bot's, make no events.
    let bot_post = app
        .http
        .post(app.url(paths::BOT_MESSAGES_ROUTE))
        .bearer_auth(&made.token)
        .json(&json!({"channel": "general", "text": "On it", "thread_ts": posted.id.to_string()}))
        .send()
        .await
        .unwrap();
    assert!(bot_post.status().is_success());

    // Mentions only: a message that names it is sent, one that does not is not.
    let mentions: lazy_protocol::App = set(json!(["app_mention"])).await.json().await.unwrap();
    assert_eq!(mentions.events.unwrap().types, ["app_mention"]);
    app.send(&alice, app.general, "nothing for the app").await;
    let named = app
        .send(&alice, app.general, "@Helper what's the status?")
        .await;
    let events = until_events(&stand, 2).await;
    assert_eq!(events.len(), 2, "{events:?}");
    assert_eq!(events[1].0["event"]["type"], "app_mention");
    assert_eq!(events[1].0["event"]["ts"], named.id.to_string());

    // Turned off: nothing more is sent.
    let off: lazy_protocol::App = app
        .call(
            reqwest::Method::PUT,
            &paths::app_events(id),
            &boss,
            Some(json!({"url": ""})),
        )
        .await
        .json()
        .await
        .unwrap();
    assert!(off.events.is_none());
    app.send(&alice, app.general, "@Helper anyone?").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(stand.events.lock().unwrap().len(), 2);
    app.server.shutdown().await.unwrap();
}
