//! The GitHub connector (D32), end to end against a stand-in for GitHub: connecting through the
//! manifest flow, installations from signed events, subscriptions, deliveries posted once (and
//! duplicates, unsigned deliveries, filtered and ignored events), retrying, and disconnecting.

mod common;

use std::time::{Duration, Instant};

use aws_lc_rs::hmac;
use axum::Router;
use axum::extract::Path;
use axum::routing::post;
use common::*;
use lazy_domain::github::GithubEvent;
use lazy_domain::{ConversationId, UserId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ErrorCode, GithubDeliveryState, GithubStatusResponse, GithubSubscriptionResponse,
};
use lazy_server::credentials::CredentialKeys;
use lazy_server::github::GithubEndpoints;
use lazy_server::jobs::WorkerOptions;
use lazy_server::{IntegrationOptions, WebhookRate};
use serde_json::{Value, json};
use sqlx::PgPool;

const APP_ID: i64 = 4242;
const WEBHOOK_SECRET: &str = "whsec_test_9f8e7d6c";
const PEM: &str =
    "-----BEGIN RSA PRIVATE KEY-----\nMIIEnotarealkey\n-----END RSA PRIVATE KEY-----\n";
const CLIENT_SECRET: &str = "client_secret_abc123";

/// A stand-in for GitHub's API: converts any manifest code into the same app.
async fn fake_github() -> String {
    let router = Router::new().route(
        "/app-manifests/{code}/conversions",
        post(|Path(code): Path<String>| async move {
            if code == "refused" {
                return (axum::http::StatusCode::NOT_FOUND, axum::Json(json!({}))).into_response();
            }
            axum::Json(json!({
                "id": APP_ID,
                "slug": "lazy-chat-test",
                "name": "Lazy chat.test",
                "html_url": "https://github.example/apps/lazy-chat-test",
                "owner": { "login": "last-refuge" },
                "pem": PEM,
                "webhook_secret": WEBHOOK_SECRET,
                "client_id": "Iv1.abc",
                "client_secret": CLIENT_SECRET
            }))
            .into_response()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

use axum::response::IntoResponse;

fn options(api: String, key: bool) -> IntegrationOptions {
    IntegrationOptions {
        webhook_rate: WebhookRate::DEFAULT,
        credential_keys: if key {
            CredentialKeys::from_bytes([3; 32], &[])
        } else {
            CredentialKeys::none()
        },
        jobs: Some(WorkerOptions {
            poll: Duration::from_millis(50),
            backoff_base: Duration::from_millis(50),
            backoff_cap: Duration::from_millis(200),
            ..WorkerOptions::default()
        }),
        github: GithubEndpoints {
            api,
            web: "https://github.example".to_owned(),
        },
        ..IntegrationOptions::default()
    }
}

struct Team {
    app: App,
    boss: Account,
    alice: Account,
}

async fn team(pool: PgPool, key: bool) -> Team {
    let api = fake_github().await;
    let app = App::start_with_integrations(pool, options(api, key)).await;
    let _owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    Team { app, boss, alice }
}

fn browser() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn status(app: &App, who: &Account) -> GithubStatusResponse {
    let response = app
        .call(
            reqwest::Method::GET,
            &paths::github(app.workspace_id),
            who,
            None,
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

/// Connects GitHub the way an admin's browser does; returns the state token used.
async fn connect(t: &Team) -> String {
    let app = &t.app;
    let response = app
        .call(
            reqwest::Method::POST,
            &paths::github_setup(app.workspace_id),
            &t.boss,
            Some(json!({ "organization": "last-refuge" })),
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    let url = response.json::<Value>().await.unwrap()["url"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(url.starts_with(&format!("{}/github/setup/", app.server.base_url)));
    let token = url.rsplit('/').next().unwrap().to_owned();

    // The page sends the browser to GitHub with the manifest.
    let page = browser().get(&url).send().await.unwrap();
    assert_eq!(page.status().as_u16(), 200);
    let html = page.text().await.unwrap();
    assert!(html.contains(&format!(
        "https://github.example/organizations/last-refuge/settings/apps/new?state={token}"
    )));
    assert!(html.contains(&format!("{}/github/webhook", app.server.base_url)));
    assert!(html.contains("&quot;public&quot;:false"));

    // GitHub sends it back with a code; Lazy trades it and sends the browser on to install.
    let back = browser()
        .get(format!(
            "{}/github/callback?code=abc123&state={token}",
            app.server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(back.status().as_u16(), 303);
    assert_eq!(
        back.headers()["location"],
        "https://github.example/apps/lazy-chat-test/installations/new"
    );
    token
}

fn sign(body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, WEBHOOK_SECRET.as_bytes());
    let tag = hmac::sign(&key, body);
    let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
}

async fn deliver(
    app: &App,
    event: &str,
    delivery: &str,
    payload: &Value,
    signature: Option<String>,
) -> (u16, String) {
    let body = payload.to_string();
    let signature = signature.unwrap_or_else(|| sign(body.as_bytes()));
    let response = reqwest::Client::new()
        .post(format!("{}/github/webhook", app.server.base_url))
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-github-hook-installation-target-id", APP_ID.to_string())
        .header("x-github-hook-installation-target-type", "integration")
        .header("x-hub-signature-256", signature)
        .body(body)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

fn repository() -> Value {
    json!({
        "full_name": "last-refuge/lazy",
        "html_url": "https://github.com/last-refuge/lazy",
        "default_branch": "main"
    })
}

fn installation(action: &str) -> Value {
    json!({
        "action": action,
        "installation": {
            "id": 777,
            "account": { "login": "last-refuge" },
            "repository_selection": "all"
        },
        "sender": { "login": "jasona" }
    })
}

fn pull_request(action: &str, number: i64, title: &str) -> Value {
    json!({
        "action": action,
        "pull_request": {
            "number": number,
            "title": title,
            "html_url": format!("https://github.com/last-refuge/lazy/pull/{number}"),
            "body": "Adds the thing.\n\nAnd tests.",
            "draft": false,
            "merged": action == "closed"
        },
        "repository": repository(),
        "sender": { "login": "jasona" }
    })
}

/// The bot's messages in a channel, oldest first, as plain text.
async fn bot_texts(app: &App, bot: UserId, channel: ConversationId) -> Vec<String> {
    let rows: Vec<Value> = sqlx::query_scalar(
        "SELECT document FROM messages WHERE author_id = $1 AND conversation_id = $2 ORDER BY seq",
    )
    .bind(bot.as_uuid())
    .bind(channel.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|doc| {
            serde_json::from_value::<lazy_domain::MessageDocument>(doc)
                .unwrap()
                .plain_text()
        })
        .collect()
}

async fn until_jobs_settle(app: &App) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'github.event' AND state IN ('queued', 'running')",
        )
        .fetch_one(&app.pool)
        .await
        .unwrap();
        if open == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "deliveries did not settle");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

async fn subscribe(
    app: &App,
    who: &Account,
    channel: ConversationId,
    repo: &str,
    events: &[GithubEvent],
) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::github_subscriptions(channel),
        who,
        Some(json!({ "repository": repo, "events": events })),
    )
    .await
}

async fn start_raw(app: &App, who: &Account) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::github_setup(app.workspace_id),
        who,
        Some(json!({ "organization": "last-refuge" })),
    )
    .await
}

async fn retry_raw(app: &App, who: &Account, id: i64) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::github_delivery_retry(id),
        who,
        None,
    )
    .await
}

#[sqlx::test]
async fn connecting_needs_an_admin_and_a_credentials_key(pool: PgPool) {
    let t = team(pool, false).await;
    let app = &t.app;
    denied(start_raw(app, &t.alice).await, ErrorCode::Forbidden).await;
    denied(start_raw(app, &t.boss).await, ErrorCode::Unavailable).await;
    let s = status(app, &t.boss).await;
    assert!(!s.available && s.connection.is_none());
    denied(
        app.call(
            reqwest::Method::GET,
            &paths::github(app.workspace_id),
            &t.alice,
            None,
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn an_admin_connects_github_and_the_secrets_are_sealed(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    // A bad organization name is refused before anything starts.
    denied(
        app.call(
            reqwest::Method::POST,
            &paths::github_setup(app.workspace_id),
            &t.boss,
            Some(json!({ "organization": "not an org" })),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let token = connect(&t).await;

    // The state works once.
    let again = browser()
        .get(format!(
            "{}/github/callback?code=abc123&state={token}",
            app.server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(again.status().as_u16(), 404);
    let page_again = browser()
        .get(format!("{}/github/setup/{token}", app.server.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(page_again.status().as_u16(), 404);

    let s = status(app, &t.boss).await;
    assert!(s.available);
    let connection = s.connection.expect("connected");
    assert_eq!(connection.app_slug, "lazy-chat-test");
    assert_eq!(connection.owner, "last-refuge");
    assert!(
        s.installations.is_empty(),
        "not installed until GitHub says so"
    );
    // The bot is a workspace member, an app.
    let boot = app.bootstrap_ok(&t.alice).await;
    let bot = boot.workspaces[0]
        .members
        .iter()
        .find(|m| m.id == connection.bot_user_id)
        .expect("the bot is a member");
    assert!(bot.is_bot);
    assert_eq!(bot.display_name.as_str(), "GitHub");

    // Nothing GitHub sent is readable in the database or in the status.
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT row_to_json(c)::text FROM integration_credentials c")
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        for secret in [WEBHOOK_SECRET, CLIENT_SECRET, "MIIEnotarealkey"] {
            assert!(!row.contains(secret), "{secret} stored readable");
        }
    }
    let raw = app
        .call(
            reqwest::Method::GET,
            &paths::github(app.workspace_id),
            &t.boss,
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    for secret in [WEBHOOK_SECRET, CLIENT_SECRET, "MIIEnotarealkey"] {
        assert!(!raw.contains(secret));
    }

    // Connecting again while connected is refused.
    denied(
        app.call(
            reqwest::Method::POST,
            &paths::github_setup(app.workspace_id),
            &t.boss,
            Some(json!({ "organization": "last-refuge" })),
        )
        .await,
        ErrorCode::Conflict,
    )
    .await;
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn deliveries_are_checked_and_posted_once_to_the_channels_that_follow_them(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    connect(&t).await;
    let bot = status(app, &t.boss).await.connection.unwrap().bot_user_id;

    // Before installation, a subscription is refused.
    denied(
        subscribe(
            app,
            &t.boss,
            app.general,
            "last-refuge/lazy",
            &GithubEvent::DEFAULT,
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;

    // Unsigned, wrongly signed, and unknown-app deliveries are refused.
    let install = installation("created");
    let (code, _) = deliver(
        app,
        "installation",
        "i-0",
        &install,
        Some("sha256=00".into()),
    )
    .await;
    assert_eq!(code, 401);
    let (code, _) = deliver(app, "installation", "i-0", &install, Some(sign(b"other"))).await;
    assert_eq!(code, 401);
    let unknown = reqwest::Client::new()
        .post(format!("{}/github/webhook", app.server.base_url))
        .header("x-github-hook-installation-target-id", "1")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status().as_u16(), 404);
    assert_eq!(
        deliver(app, "ping", "p-1", &json!({"zen": "hi"}), None)
            .await
            .0,
        200
    );

    // GitHub says the app was installed on the organization.
    assert_eq!(
        deliver(app, "installation", "i-1", &install, None).await.0,
        200
    );
    let s = status(app, &t.boss).await;
    assert_eq!(s.installations.len(), 1);
    assert!(s.installations[0].all_repositories && s.installations[0].account == "last-refuge");

    // Members may not subscribe; other organizations' repositories are refused; the admin
    // subscribes #general, and the bot joins it.
    denied(
        subscribe(
            app,
            &t.alice,
            app.general,
            "last-refuge/lazy",
            &GithubEvent::DEFAULT,
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        subscribe(
            app,
            &t.boss,
            app.general,
            "someone-else/repo",
            &GithubEvent::DEFAULT,
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        subscribe(app, &t.boss, app.general, "lazy", &GithubEvent::DEFAULT).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let response = subscribe(
        app,
        &t.boss,
        app.general,
        "https://github.com/Last-Refuge/Lazy",
        &[GithubEvent::PullRequest, GithubEvent::Push],
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());
    let sub = response
        .json::<GithubSubscriptionResponse>()
        .await
        .unwrap()
        .subscription;
    assert_eq!(sub.repository, "last-refuge/lazy");
    assert_eq!(sub.channel_name.as_str(), "general");
    let members: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT user_id FROM conversation_members WHERE conversation_id = $1")
            .bind(app.general.as_uuid())
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert!(members.contains(&bot.as_uuid()), "the bot joined #general");

    // A pull request is posted once, however often GitHub sends it.
    let opened = pull_request("opened", 12, "Fix *bold* [links] @alice");
    assert_eq!(
        deliver(app, "pull_request", "d-1", &opened, None).await,
        (202, "queued".into())
    );
    assert_eq!(
        deliver(app, "pull_request", "d-1", &opened, None).await,
        (200, "already received".into())
    );
    until_jobs_settle(app).await;
    let texts = bot_texts(app, bot, app.general).await;
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert!(
        texts[0].starts_with(
            "jasona opened pull request #12 Fix *bold* [links] @alice \
             (https://github.com/last-refuge/lazy/pull/12) in last-refuge/lazy"
        ),
        "{}",
        texts[0]
    );
    assert!(
        texts[0].contains("Adds the thing."),
        "the description is quoted"
    );
    // The title's characters stayed text: no mention of alice was made.
    let mentions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM messages WHERE author_id = $1 AND document::text LIKE '%\"mention\"%'",
    )
    .bind(bot.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(mentions, 0);

    // Filtered and ignored: an issue (not followed), a push to another branch, a label change.
    let issue = json!({
        "action": "opened",
        "issue": { "number": 3, "title": "Bug", "html_url": "https://github.com/x", "body": "" },
        "repository": repository(), "sender": { "login": "jasona" }
    });
    assert_eq!(deliver(app, "issues", "d-2", &issue, None).await.0, 202);
    let side_push = json!({
        "ref": "refs/heads/feature", "commits": [{"id": "abcdef1234567", "message": "wip", "url": "u"}],
        "repository": repository(), "pusher": { "name": "jasona" }, "sender": { "login": "jasona" },
        "compare": "c"
    });
    assert_eq!(
        deliver(app, "push", "d-3", &side_push, None).await,
        (200, "ignored".into())
    );
    let labeled = pull_request("labeled", 12, "x");
    assert_eq!(
        deliver(app, "pull_request", "d-4", &labeled, None).await,
        (200, "ignored".into())
    );
    // A push to main is followed.
    let push = json!({
        "ref": "refs/heads/main",
        "commits": [
            {"id": "1111111aaaa", "message": "First\n\nbody", "url": "https://github.com/c/1"},
            {"id": "2222222bbbb", "message": "Second", "url": "https://github.com/c/2"}
        ],
        "repository": repository(), "pusher": { "name": "jasona" }, "sender": { "login": "jasona" },
        "compare": "https://github.com/compare"
    });
    assert_eq!(deliver(app, "push", "d-5", &push, None).await.0, 202);
    until_jobs_settle(app).await;
    let texts = bot_texts(app, bot, app.general).await;
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(
        texts[1].starts_with(
            "jasona pushed 2 commits (https://github.com/compare) to main in last-refuge/lazy"
        ),
        "{}",
        texts[1]
    );
    assert!(
        texts[1].contains("1111111 (https://github.com/c/1) First")
            && texts[1].contains("2222222 (https://github.com/c/2) Second")
            && !texts[1].contains("body"),
        "{}",
        texts[1]
    );

    // The deliveries are listed for admins, newest first.
    let s = status(app, &t.boss).await;
    let listed: Vec<(&str, GithubDeliveryState)> = s
        .deliveries
        .iter()
        .map(|d| (d.event.as_str(), d.state))
        .collect();
    assert_eq!(
        listed,
        vec![
            ("push", GithubDeliveryState::Posted),
            ("issues", GithubDeliveryState::Posted),
            ("pull_request", GithubDeliveryState::Posted),
        ]
    );
    assert_eq!(
        s.deliveries[0].repository.as_deref(),
        Some("last-refuge/lazy")
    );

    // Nothing of the payload beyond the summary is stored.
    let stored: Vec<String> = sqlx::query_scalar("SELECT payload::text FROM jobs")
        .fetch_all(&app.pool)
        .await
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|p| !p.contains("draft") && !p.contains("default_branch"))
    );
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_failed_delivery_can_be_retried_by_an_admin(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    connect(&t).await;
    deliver(app, "installation", "i-1", &installation("created"), None).await;
    subscribe(
        app,
        &t.boss,
        app.general,
        "last-refuge/lazy",
        &GithubEvent::DEFAULT,
    )
    .await;
    deliver(
        app,
        "pull_request",
        "d-1",
        &pull_request("opened", 1, "A"),
        None,
    )
    .await;
    until_jobs_settle(app).await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE kind = 'github.event'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    // A posted delivery is not run again.
    denied(retry_raw(app, &t.boss, id).await, ErrorCode::Conflict).await;
    sqlx::query(
        "UPDATE jobs SET state = 'failed', last_error_class = 'retryable', \
         last_error = 'the database failed' WHERE id = $1",
    )
    .bind(id)
    .execute(&app.pool)
    .await
    .unwrap();
    denied(retry_raw(app, &t.alice, id).await, ErrorCode::Forbidden).await;
    assert_eq!(retry_raw(app, &t.boss, id).await.status().as_u16(), 204);
    until_jobs_settle(app).await;
    let bot = status(app, &t.boss).await.connection.unwrap().bot_user_id;
    assert_eq!(
        bot_texts(app, bot, app.general).await.len(),
        1,
        "the retry found the message already posted and posted nothing more"
    );
    denied(
        app.call(
            reqwest::Method::POST,
            &paths::github_delivery_retry(999_999),
            &t.boss,
            None,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn unsubscribing_and_disconnecting_take_everything_back(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    connect(&t).await;
    deliver(app, "installation", "i-1", &installation("created"), None).await;
    let ops = app.create_channel(&t.boss, "ops", "public").await;
    let sub = subscribe(
        app,
        &t.boss,
        app.general,
        "last-refuge/lazy",
        &GithubEvent::DEFAULT,
    )
    .await
    .json::<GithubSubscriptionResponse>()
    .await
    .unwrap()
    .subscription;
    subscribe(
        app,
        &t.boss,
        ops.id,
        "last-refuge/lazy",
        &GithubEvent::DEFAULT,
    )
    .await;
    let bot = status(app, &t.boss).await.connection.unwrap().bot_user_id;
    let is_member = |channel: ConversationId| async move {
        sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM conversation_members \
             WHERE conversation_id = $1 AND user_id = $2)",
        )
        .bind(channel.as_uuid())
        .bind(bot.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap()
    };
    assert!(is_member(app.general).await && is_member(ops.id).await);

    // Unsubscribing #general: the bot leaves it; doing it again is fine.
    let path = paths::github_subscription(sub.id);
    denied(
        app.call(reqwest::Method::DELETE, &path, &t.alice, None)
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(
        app.call(reqwest::Method::DELETE, &path, &t.boss, None)
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(
        app.call(reqwest::Method::DELETE, &path, &t.boss, None)
            .await
            .status()
            .as_u16(),
        204
    );
    assert!(!is_member(app.general).await && is_member(ops.id).await);

    // An installation removed on GitHub disappears from the status.
    deliver(app, "installation", "i-2", &installation("deleted"), None).await;
    assert!(status(app, &t.boss).await.installations.is_empty());
    deliver(app, "installation", "i-3", &installation("created"), None).await;

    // Disconnecting: secrets erased, subscriptions gone, the bot leaves, GitHub is refused.
    assert_eq!(
        app.call(
            reqwest::Method::DELETE,
            &paths::github(app.workspace_id),
            &t.boss,
            None
        )
        .await
        .status()
        .as_u16(),
        204
    );
    assert!(!is_member(ops.id).await);
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_credentials WHERE ciphertext IS NOT NULL",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(live, 0);
    let s = status(app, &t.boss).await;
    assert!(s.connection.is_none() && s.subscriptions.is_empty());
    let (code, _) = deliver(
        app,
        "pull_request",
        "d-9",
        &pull_request("opened", 2, "B"),
        None,
    )
    .await;
    assert_eq!(code, 404, "a disconnected app's deliveries are refused");
    // And it can be connected again.
    connect(&t).await;
    assert!(status(app, &t.boss).await.connection.is_some());
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_conversion_github_refuses_explains_and_starts_nothing(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    let url = app
        .call(
            reqwest::Method::POST,
            &paths::github_setup(app.workspace_id),
            &t.boss,
            Some(json!({ "organization": "last-refuge" })),
        )
        .await
        .json::<Value>()
        .await
        .unwrap()["url"]
        .as_str()
        .unwrap()
        .to_owned();
    let token = url.rsplit('/').next().unwrap();
    let back = browser()
        .get(format!(
            "{}/github/callback?code=refused&state={token}",
            app.server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(back.status().as_u16(), 502);
    assert!(back.text().await.unwrap().contains("Start again from Lazy"));
    assert!(status(app, &t.boss).await.connection.is_none());
    // A made-up state is a dead link.
    let made_up = browser()
        .get(format!(
            "{}/github/callback?code=x&state=nope",
            app.server.base_url
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(made_up.status().as_u16(), 404);
    t.app.server.shutdown().await.unwrap();
}
