//! Slash commands (D36), end to end against a stand-in app server that checks Slack's signature:
//! adding a command, the signing secret, running it with private, posted, and later answers, an app
//! that fails or is slow, and who may do what.

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
use lazy_protocol::{
    AppSigningSecretResponse, CreatedAppResponse, ErrorCode, RunCommandResponse,
    WorkspaceCommandsResponse,
};
use lazy_server::IntegrationOptions;
use lazy_server::credentials::CredentialKeys;
use serde_json::json;
use sqlx::PgPool;

/// What the stand-in app knows: the signing secret to check against, and what it received.
/// One request's form fields, in order.
type Fields = Vec<(String, String)>;

#[derive(Clone, Default)]
struct Stand {
    secret: Arc<Mutex<String>>,
    seen: Arc<Mutex<Vec<Fields>>>,
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
    let signature = headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if signature != expected {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let fields: Fields = form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    stand.seen.lock().unwrap().push(fields.clone());
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    match field("text").as_str() {
        "private" => "Only *you* see this".into_response(),
        "public" => {
            axum::Json(json!({"response_type": "in_channel", "text": "Deployed `web` :tada:"}))
                .into_response()
        }
        "ack" => StatusCode::OK.into_response(),
        "later" => {
            let url = field("response_url");
            tokio::spawn(async move {
                let client = reqwest::Client::new();
                let _ = client
                    .post(&url)
                    .json(&json!({"response_type": "in_channel", "text": "Finished later"}))
                    .send()
                    .await;
                let _ = client
                    .post(&url)
                    .json(&json!({"text": "private later"}))
                    .send()
                    .await;
            });
            StatusCode::OK.into_response()
        }
        "slow" => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            "too late".into_response()
        }
        _ => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
    }
}

async fn stand_in(stand: Stand) -> String {
    let router = Router::new().route("/cmd", post(answer)).with_state(stand);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}/cmd")
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

async fn add(
    app: &App,
    who: &Account,
    app_id: lazy_domain::AppId,
    name: &str,
    url: String,
) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::app_commands(app_id),
        who,
        Some(json!({"name": name, "url": url, "description": "Deploy a service", "usage_hint": "[service]"})),
    )
    .await
}

async fn run(
    app: &App,
    who: &Account,
    conversation: lazy_domain::ConversationId,
    text: &str,
) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::run_command(conversation),
        who,
        Some(json!({"command": "deploy", "text": text})),
    )
    .await
}

#[sqlx::test]
async fn a_command_goes_to_its_app_signed_and_its_answers_come_back(pool: PgPool) {
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

    // Adding: names and addresses are checked; members cannot.
    denied(
        add(app, &t.boss, app_id, "Bad Name", url.clone()).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        add(
            app,
            &t.boss,
            app_id,
            "deploy",
            "http://example.com/cmd".into(),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let added: lazy_protocol::App = add(app, &t.boss, app_id, "/deploy", url.clone())
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(added.commands.len(), 1);
    assert_eq!(added.commands[0].name, "deploy");
    denied(
        app.call(
            reqwest::Method::POST,
            &paths::app_commands(app_id),
            &t.alice,
            Some(json!({"name": "x", "url": url})),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;

    // Members see the workspace's commands.
    let listed: WorkspaceCommandsResponse = app
        .call(
            reqwest::Method::GET,
            &paths::workspace_commands(app.workspace_id),
            &t.alice,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(listed.commands[0].name, "deploy");
    assert_eq!(listed.commands[0].app_name, "Deployer");

    // Before the secret is made, the command cannot be signed.
    denied(
        run(app, &t.alice, app.general, "private").await,
        ErrorCode::Unavailable,
    )
    .await;
    // The admin reads the secret (the same each time); members cannot.
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
    let again: AppSigningSecretResponse = app
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
    assert_eq!(secret.secret, again.secret);
    denied(
        app.call(
            reqwest::Method::GET,
            &paths::app_signing_secret(app_id),
            &t.alice,
            None,
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    *stand.secret.lock().unwrap() = secret.secret.clone();

    // A private answer comes back to the person, and nothing is posted.
    let head = app.head().await;
    let private: RunCommandResponse = run(app, &t.alice, app.general, "private")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(private.app_name, "Deployer");
    assert_eq!(private.reply.unwrap().plain_text(), "Only *you* see this");
    assert!(private.posted.is_none());
    assert_eq!(app.head().await, head, "nothing was posted");
    // The app was told who, where, and what, in Slack's fields.
    let seen = stand.seen.lock().unwrap().last().cloned().unwrap();
    let field = |name: &str| {
        seen.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap()
    };
    assert_eq!(field("command"), "/deploy");
    assert_eq!(field("text"), "private");
    assert_eq!(field("user_name"), "alice");
    assert_eq!(field("channel_name"), "general");
    assert_eq!(field("channel_id"), app.general.to_string());
    assert!(field("response_url").contains("/commands/respond/"));

    // An in-channel answer is posted by the app's bot, which joins the channel.
    let public: RunCommandResponse = run(app, &t.alice, app.general, "public")
        .await
        .json()
        .await
        .unwrap();
    let posted = app.get_message(&t.alice, public.posted.unwrap()).await;
    assert_eq!(posted.author_id, made.app.bot_user_id);
    assert_eq!(posted.document.plain_text(), "Deployed web 🎉");

    // An acknowledgement is nothing; a later in-channel answer is posted; a later private one is
    // refused (the stand-in sends both).
    let ack: RunCommandResponse = run(app, &t.alice, app.general, "later")
        .await
        .json()
        .await
        .unwrap();
    assert!(ack.reply.is_none() && ack.posted.is_none());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let later: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM messages WHERE author_id = $1 AND document::text LIKE '%Finished later%'",
        )
        .bind(made.app.bot_user_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        if later == 1 {
            break;
        }
        assert!(Instant::now() < deadline, "the later answer was not posted");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // An app that fails, or is slow, is reported, and nothing is posted.
    denied(
        run(app, &t.alice, app.general, "fail").await,
        ErrorCode::Unavailable,
    )
    .await;
    let started = Instant::now();
    denied(
        run(app, &t.alice, app.general, "slow").await,
        ErrorCode::Unavailable,
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "it gave up after 3 seconds"
    );

    // Only in a conversation the person is in; only commands that exist.
    let secret_room = app.create_channel(&t.boss, "secret", "private").await.id;
    denied(
        run(app, &t.alice, secret_room, "private").await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.call(
            reqwest::Method::POST,
            &paths::run_command(app.general),
            &t.alice,
            Some(json!({"command": "nope", "text": ""})),
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;

    // Removing the command, or deleting the app, ends it.
    let command = added.commands[0].id;
    let removed = app
        .call(
            reqwest::Method::DELETE,
            &paths::app_command(command),
            &t.boss,
            None,
        )
        .await;
    assert_eq!(removed.status().as_u16(), 204);
    denied(
        run(app, &t.alice, app.general, "private").await,
        ErrorCode::NotFound,
    )
    .await;
    t.app.server.shutdown().await.unwrap();
}
