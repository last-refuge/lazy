//! Webhook connectors (D34), end to end: connecting Linear and Jira, joining up the signing
//! secret both ways, signed deliveries routed by team or project and kind, duplicates and
//! unsigned or stale deliveries, permissions, retrying, and disconnecting.

mod common;

use std::time::{Duration, Instant};

use aws_lc_rs::hmac;
use common::*;
use lazy_domain::connector::ConnectorProvider;
use lazy_domain::{ConversationId, UserId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    ConnectorSecretResponse, ConnectorStatusResponse, ConnectorSubscriptionResponse, ErrorCode,
    GithubDeliveryState,
};
use lazy_server::credentials::CredentialKeys;
use lazy_server::jobs::WorkerOptions;
use lazy_server::{IntegrationOptions, WebhookRate};
use serde_json::{Value, json};
use sqlx::PgPool;

const LINEAR: ConnectorProvider = ConnectorProvider::Linear;
const JIRA: ConnectorProvider = ConnectorProvider::Jira;
const LINEAR_SECRET: &str = "lin_wh_5f3c9a1b7e2d";

fn options(key: bool) -> IntegrationOptions {
    IntegrationOptions {
        webhook_rate: WebhookRate::DEFAULT,
        credential_keys: if key {
            CredentialKeys::from_bytes([5; 32], &[])
        } else {
            CredentialKeys::none()
        },
        jobs: Some(WorkerOptions {
            poll: Duration::from_millis(50),
            backoff_base: Duration::from_millis(50),
            backoff_cap: Duration::from_millis(200),
            ..WorkerOptions::default()
        }),
        ..IntegrationOptions::default()
    }
}

struct Team {
    app: App,
    boss: Account,
    alice: Account,
}

async fn team(pool: PgPool, key: bool) -> Team {
    let app = App::start_with_integrations(pool, options(key)).await;
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

async fn call(
    app: &App,
    method: reqwest::Method,
    path: &str,
    who: &Account,
    body: Option<Value>,
) -> reqwest::Response {
    app.call(method, path, who, body).await
}

async fn status(app: &App, who: &Account, provider: ConnectorProvider) -> ConnectorStatusResponse {
    let response = call(
        app,
        reqwest::Method::GET,
        &paths::connector(app.workspace_id, provider),
        who,
        None,
    )
    .await;
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

async fn connect(app: &App, who: &Account, provider: ConnectorProvider) -> reqwest::Response {
    call(
        app,
        reqwest::Method::POST,
        &paths::connector(app.workspace_id, provider),
        who,
        None,
    )
    .await
}

async fn subscribe(
    app: &App,
    who: &Account,
    channel: ConversationId,
    provider: ConnectorProvider,
    scope: &str,
    events: &[&str],
) -> reqwest::Response {
    call(
        app,
        reqwest::Method::POST,
        &paths::connector_subscriptions(channel, provider),
        who,
        Some(json!({ "scope": scope, "events": events })),
    )
    .await
}

fn hex_hmac(secret: &[u8], body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::sign(&key, body)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_ms() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap()
}

/// Posts to a connector's address with the given headers; answers the status and body.
async fn post(url: &str, body: &Value, headers: &[(&str, String)]) -> (u16, String) {
    let bytes = serde_json::to_vec(body).unwrap();
    let mut request = reqwest::Client::new()
        .post(url)
        .header("content-type", "application/json")
        .body(bytes);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    let response = request.send().await.unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

async fn linear(url: &str, secret: &str, delivery: &str, body: &Value) -> (u16, String) {
    let bytes = serde_json::to_vec(body).unwrap();
    post(
        url,
        body,
        &[
            ("linear-signature", hex_hmac(secret.as_bytes(), &bytes)),
            ("linear-delivery", delivery.to_owned()),
            ("linear-event", "Issue".to_owned()),
        ],
    )
    .await
}

async fn jira(url: &str, secret: &str, delivery: &str, body: &Value) -> (u16, String) {
    let bytes = serde_json::to_vec(body).unwrap();
    post(
        url,
        body,
        &[
            (
                "x-hub-signature",
                format!("sha256={}", hex_hmac(secret.as_bytes(), &bytes)),
            ),
            ("x-atlassian-webhook-identifier", delivery.to_owned()),
        ],
    )
    .await
}

fn linear_issue(team: &str, number: u32, title: &str) -> Value {
    json!({
        "action": "create", "type": "Issue",
        "actor": {"name": "Ada"},
        "url": format!("https://linear.app/acme/issue/{team}-{number}/x"),
        "data": {"identifier": format!("{team}-{number}"), "title": title,
                 "team": {"key": team, "name": team}},
        "webhookTimestamp": now_ms()
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
            "SELECT count(*) FROM jobs WHERE kind = 'connector.event' \
             AND state IN ('queued', 'running')",
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

#[sqlx::test]
async fn connecting_needs_an_admin_and_a_credentials_key(pool: PgPool) {
    let t = team(pool, false).await;
    let app = &t.app;
    denied(connect(app, &t.alice, LINEAR).await, ErrorCode::Forbidden).await;
    denied(connect(app, &t.boss, LINEAR).await, ErrorCode::Unavailable).await;
    let s = status(app, &t.boss, LINEAR).await;
    assert!(!s.available && s.connection.is_none());
    denied(
        call(
            app,
            reqwest::Method::GET,
            &format!("/v1/workspaces/{}/connectors/trello", app.workspace_id),
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
async fn linear_end_to_end(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;

    // Connecting makes the address and the bot; doing it again changes nothing.
    let first: ConnectorStatusResponse = connect(app, &t.boss, LINEAR).await.json().await.unwrap();
    let again: ConnectorStatusResponse = connect(app, &t.boss, LINEAR).await.json().await.unwrap();
    let connection = first.connection.clone().unwrap();
    assert_eq!(
        again.connection.unwrap().webhook_url,
        connection.webhook_url
    );
    assert!(!connection.secret_set);
    assert!(
        connection
            .webhook_url
            .starts_with(&format!("{}/connectors/linear/", app.server.base_url)),
        "{}",
        connection.webhook_url
    );
    let url = connection.webhook_url.clone();
    let bot = connection.bot_user_id;

    // Before a secret is set, nothing is taken.
    assert_eq!(
        linear(&url, LINEAR_SECRET, "d-0", &linear_issue("ENG", 1, "x"))
            .await
            .0,
        401
    );

    // The admin pastes Linear's secret; members cannot.
    let secret_path = paths::connector_secret(app.workspace_id, LINEAR);
    denied(
        call(
            app,
            reqwest::Method::PUT,
            &secret_path,
            &t.alice,
            Some(json!({"secret": LINEAR_SECRET})),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        call(
            app,
            reqwest::Method::PUT,
            &secret_path,
            &t.boss,
            Some(json!({"secret": "short"})),
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let set = call(
        app,
        reqwest::Method::PUT,
        &secret_path,
        &t.boss,
        Some(json!({"secret": LINEAR_SECRET})),
    )
    .await;
    assert_eq!(set.status().as_u16(), 204);
    assert!(
        status(app, &t.boss, LINEAR)
            .await
            .connection
            .unwrap()
            .secret_set
    );
    // The secret is sealed: not in the database as text, never in the status.
    let rows: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT ciphertext FROM integration_credentials WHERE ciphertext IS NOT NULL",
    )
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert!(rows.iter().all(|c| {
        !c.windows(LINEAR_SECRET.len())
            .any(|w| w == LINEAR_SECRET.as_bytes())
    }));
    let shown = serde_json::to_string(&status(app, &t.boss, LINEAR).await).unwrap();
    assert!(!shown.contains(LINEAR_SECRET));

    // #general follows ENG's new issues; #ops follows everything, all kinds.
    let ops = app.create_channel(&t.boss, "ops", "public").await.id;
    denied(
        subscribe(
            app,
            &t.alice,
            app.general,
            LINEAR,
            "ENG",
            &["issue_created"],
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
            LINEAR,
            "not a key",
            &["issue_created"],
        )
        .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(
        subscribe(app, &t.boss, app.general, LINEAR, "ENG", &["nonsense"]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let made: ConnectorSubscriptionResponse = subscribe(
        app,
        &t.boss,
        app.general,
        LINEAR,
        "https://linear.app/acme/team/eng/active",
        &["issue_created"],
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(made.subscription.scope, "ENG");
    let all_kinds: Vec<&str> = LINEAR.kinds().iter().map(|k| k.key).collect();
    assert!(
        subscribe(app, &t.boss, ops, LINEAR, "", &all_kinds)
            .await
            .status()
            .is_success()
    );

    // A signed ENG issue goes to both; a duplicate is not posted again; OPS goes to #ops only.
    let eng = linear_issue("ENG", 12, "Fix login");
    assert_eq!(
        linear(&url, LINEAR_SECRET, "d-1", &eng).await,
        (200, "queued".into())
    );
    assert_eq!(
        linear(&url, LINEAR_SECRET, "d-1", &eng).await,
        (200, "already received".into())
    );
    assert_eq!(
        linear(
            &url,
            LINEAR_SECRET,
            "d-2",
            &linear_issue("OPS", 3, "Disk full")
        )
        .await,
        (200, "queued".into())
    );
    // A status change: #ops follows it, #general does not.
    let moved = json!({
        "action": "update", "type": "Issue", "actor": {"name": "Ada"},
        "url": "https://linear.app/acme/issue/ENG-12/x",
        "data": {"identifier": "ENG-12", "title": "Fix login", "state": {"name": "Done"}},
        "updatedFrom": {"stateId": "s0"}, "webhookTimestamp": now_ms()
    });
    assert_eq!(linear(&url, LINEAR_SECRET, "d-3", &moved).await.0, 200);
    // Wrongly signed, stale, and ignored deliveries.
    assert_eq!(linear(&url, "other-secret", "d-4", &eng).await.0, 401);
    let mut stale = eng.clone();
    stale["webhookTimestamp"] = json!(now_ms() - 3_600_000);
    assert_eq!(linear(&url, LINEAR_SECRET, "d-5", &stale).await.0, 401);
    let label =
        json!({"action": "create", "type": "IssueLabel", "data": {}, "webhookTimestamp": now_ms()});
    assert_eq!(
        linear(&url, LINEAR_SECRET, "d-6", &label).await,
        (200, "ignored".into())
    );
    until_jobs_settle(app).await;

    let general = bot_texts(app, bot, app.general).await;
    assert_eq!(
        general,
        ["Ada created ENG-12 Fix login (https://linear.app/acme/issue/ENG-12/x) in ENG"]
    );
    let ops_texts = bot_texts(app, bot, ops).await;
    assert_eq!(ops_texts.len(), 3, "{ops_texts:?}");
    assert!(ops_texts[2].ends_with("to Done"), "{ops_texts:?}");
    // The bot is named after Linear and joined both channels.
    let name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(bot.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(name, "Linear");

    // The deliveries are listed for admins, newest first, with a label.
    let s = status(app, &t.boss, LINEAR).await;
    assert_eq!(s.deliveries.len(), 3);
    assert_eq!(s.deliveries[0].summary, "ENG-12 status change");
    assert!(
        s.deliveries
            .iter()
            .all(|d| d.state == GithubDeliveryState::Posted)
    );
    assert_eq!(s.subscriptions.len(), 2);
    // Retrying a posted delivery is refused; a member cannot retry.
    let retry = paths::connector_delivery_retry(s.deliveries[0].id);
    denied(
        call(app, reqwest::Method::POST, &retry, &t.boss, None).await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        call(app, reqwest::Method::POST, &retry, &t.alice, None).await,
        ErrorCode::Forbidden,
    )
    .await;

    // Removing #general's subscription takes the bot out of it.
    let removed = call(
        app,
        reqwest::Method::DELETE,
        &paths::connector_subscription(made.subscription.id),
        &t.boss,
        None,
    )
    .await;
    assert_eq!(removed.status().as_u16(), 204);
    let member: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(app.general.as_uuid())
    .bind(bot.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(!member);

    // Disconnecting refuses deliveries, erases the secret, and ends the subscriptions.
    let gone = call(
        app,
        reqwest::Method::DELETE,
        &paths::connector(app.workspace_id, LINEAR),
        &t.boss,
        None,
    )
    .await;
    assert_eq!(gone.status().as_u16(), 204);
    assert_eq!(
        linear(&url, LINEAR_SECRET, "d-7", &linear_issue("ENG", 13, "y"))
            .await
            .0,
        404
    );
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM integration_credentials WHERE ciphertext IS NOT NULL",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(left, 0);
    let s = status(app, &t.boss, LINEAR).await;
    assert!(s.connection.is_none() && s.subscriptions.is_empty());
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn jira_takes_a_secret_lazy_makes(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    let s: ConnectorStatusResponse = connect(app, &t.boss, JIRA).await.json().await.unwrap();
    let connection = s.connection.unwrap();
    let url = connection.webhook_url;
    let bot = connection.bot_user_id;
    let made: ConnectorSecretResponse = call(
        app,
        reqwest::Method::POST,
        &paths::connector_secret(app.workspace_id, JIRA),
        &t.boss,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(made.secret.len(), 64);
    assert!(
        subscribe(
            app,
            &t.boss,
            app.general,
            JIRA,
            "web",
            &["issue_created", "comment"]
        )
        .await
        .status()
        .is_success()
    );

    let issue = json!({
        "key": "WEB-42", "self": "https://acme.atlassian.net/rest/api/2/issue/99291",
        "fields": {"summary": "Checkout fails", "project": {"key": "WEB", "name": "Website"}}
    });
    let created = json!({"webhookEvent": "jira:issue_created", "timestamp": now_ms(),
                         "user": {"displayName": "Ada"}, "issue": issue});
    assert_eq!(
        jira(&url, &made.secret, "j-1", &created).await,
        (200, "queued".into())
    );
    assert_eq!(
        jira(&url, &made.secret, "j-1", &created).await,
        (200, "already received".into())
    );
    assert_eq!(jira(&url, "not-the-secret", "j-2", &created).await.0, 401);
    // Another project is not followed here.
    let mut other = created.clone();
    other["issue"]["key"] = json!("APP-1");
    other["issue"]["fields"]["project"] = json!({"key": "APP", "name": "App"});
    assert_eq!(jira(&url, &made.secret, "j-3", &other).await.0, 200);
    until_jobs_settle(app).await;
    assert_eq!(
        bot_texts(app, bot, app.general).await,
        ["Ada created WEB-42 Checkout fails (https://acme.atlassian.net/browse/WEB-42) in Website"]
    );

    // A new secret replaces the old one at once.
    let newer: ConnectorSecretResponse = call(
        app,
        reqwest::Method::POST,
        &paths::connector_secret(app.workspace_id, JIRA),
        &t.boss,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_ne!(newer.secret, made.secret);
    assert_eq!(jira(&url, &made.secret, "j-4", &created).await.0, 401);
    assert_eq!(jira(&url, &newer.secret, "j-4", &created).await.0, 200);
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn sentry_and_pagerduty_route_by_project_and_service(pool: PgPool) {
    let t = team(pool, true).await;
    let app = &t.app;
    let ops = app.create_channel(&t.boss, "ops", "public").await.id;
    let mut urls = Vec::new();
    for (provider, secret) in [
        (ConnectorProvider::Sentry, "sentry-client-secret-1"),
        (ConnectorProvider::PagerDuty, "pd-webhook-secret-1"),
    ] {
        let s: ConnectorStatusResponse =
            connect(app, &t.boss, provider).await.json().await.unwrap();
        let connection = s.connection.unwrap();
        let set = call(
            app,
            reqwest::Method::PUT,
            &paths::connector_secret(app.workspace_id, provider),
            &t.boss,
            Some(json!({"secret": secret})),
        )
        .await;
        assert_eq!(set.status().as_u16(), 204);
        urls.push((connection.webhook_url, connection.bot_user_id, secret));
    }
    let (sentry_url, sentry_bot, sentry_secret) = urls[0].clone();
    let (pd_url, pd_bot, pd_secret) = urls[1].clone();

    // #general follows one Sentry project; #ops follows every PagerDuty service.
    assert!(
        subscribe(
            app,
            &t.boss,
            app.general,
            ConnectorProvider::Sentry,
            "https://acme.sentry.io/settings/projects/web-app/",
            &["issue_created", "alert"]
        )
        .await
        .status()
        .is_success()
    );
    assert!(
        subscribe(
            app,
            &t.boss,
            ops,
            ConnectorProvider::PagerDuty,
            "",
            &["triggered", "note"]
        )
        .await
        .status()
        .is_success()
    );

    let issue = |slug: &str| {
        json!({"action": "created", "actor": {"type": "application", "id": "sentry"},
               "data": {"issue": {"shortId": "WEB-1", "title": "Boom", "web_url": "https://acme.sentry.io/issues/1/",
                                  "project": {"slug": slug, "name": slug}}}})
    };
    let sentry_post = |body: Value, id: &'static str, secret: &'static str| {
        let bytes = serde_json::to_vec(&body).unwrap();
        let url = sentry_url.clone();
        async move {
            post(
                &url,
                &body,
                &[
                    ("sentry-hook-signature", hex_hmac(secret.as_bytes(), &bytes)),
                    ("sentry-hook-resource", "issue".to_owned()),
                    ("request-id", id.to_owned()),
                ],
            )
            .await
        }
    };
    assert_eq!(
        sentry_post(issue("web-app"), "s-1", sentry_secret).await,
        (200, "queued".into())
    );
    assert_eq!(
        sentry_post(issue("api"), "s-2", sentry_secret).await,
        (200, "queued".into())
    );
    assert_eq!(sentry_post(issue("web-app"), "s-3", "wrong").await.0, 401);

    let pd = |event_type: &str| {
        json!({"event": {"event_type": event_type, "agent": {"summary": "Ada"},
            "data": {"number": 7, "title": "Checkout down", "html_url": "https://acme.pagerduty.com/incidents/Q7",
                     "urgency": "high", "service": {"id": "PXYZ123", "summary": "Payments"}}}})
    };
    let pd_post = |body: Value, id: &'static str| {
        let bytes = serde_json::to_vec(&body).unwrap();
        let url = pd_url.clone();
        async move {
            post(
                &url,
                &body,
                &[
                    (
                        "x-pagerduty-signature",
                        format!("v1=00ff, v1={}", hex_hmac(pd_secret.as_bytes(), &bytes)),
                    ),
                    ("x-webhook-id", id.to_owned()),
                ],
            )
            .await
        }
    };
    assert_eq!(
        pd_post(pd("incident.triggered"), "p-1").await,
        (200, "queued".into())
    );
    assert_eq!(
        pd_post(pd("incident.resolved"), "p-2").await,
        (200, "queued".into())
    );
    assert_eq!(
        pd_post(json!({"event": {"event_type": "pagey.ping"}}), "p-3").await,
        (200, "ignored".into())
    );
    until_jobs_settle(app).await;

    assert_eq!(
        bot_texts(app, sentry_bot, app.general).await,
        ["New issue in web-app: WEB-1 Boom (https://acme.sentry.io/issues/1/)"]
    );
    assert_eq!(
        bot_texts(app, pd_bot, ops).await,
        [
            "Triggered: #7 Checkout down (https://acme.pagerduty.com/incidents/Q7) on Payments · high urgency"
        ]
    );
    t.app.server.shutdown().await.unwrap();
}
