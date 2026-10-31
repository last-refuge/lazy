//! Managing incoming webhooks inside Lazy (D30): the workspace's list of integrations, each
//! webhook's delivery log, and giving a webhook a new address.

mod common;

use std::time::Duration;

use common::*;
use lazy_domain::{MAX_WEBHOOK_DELIVERIES_KEPT, WebhookId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    CreatedWebhook, ErrorCode, WebhookDeliveriesResponse, WebhookDelivery,
    WebhookDeliveryOutcome as Outcome, WorkspaceWebhook, WorkspaceWebhooksResponse,
};
use lazy_server::{IntegrationOptions, WebhookRate};
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// An owner, an admin, and a member.
struct Team {
    app: App,
    owner: Account,
    boss: Account,
    alice: Account,
}

async fn team_with(app: App) -> Team {
    let owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    Team {
        app,
        owner,
        boss,
        alice,
    }
}

async fn team(pool: PgPool) -> Team {
    team_with(App::start(pool).await).await
}

async fn get(app: &App, who: &Account, path: &str) -> reqwest::Response {
    app.call(reqwest::Method::GET, path, who, None).await
}

async fn deliveries(app: &App, who: &Account, webhook: WebhookId) -> Vec<WebhookDelivery> {
    let response = get(app, who, &paths::webhook_deliveries(webhook)).await;
    assert!(response.status().is_success(), "{}", response.status());
    response
        .json::<WebhookDeliveriesResponse>()
        .await
        .unwrap()
        .deliveries
}

async fn workspace_list(app: &App, who: &Account) -> Vec<WorkspaceWebhook> {
    let response = get(app, who, &paths::workspace_webhooks(app.workspace_id)).await;
    assert!(response.status().is_success(), "{}", response.status());
    response
        .json::<WorkspaceWebhooksResponse>()
        .await
        .unwrap()
        .webhooks
}

async fn new_address_raw(app: &App, who: &Account, webhook: WebhookId) -> reqwest::Response {
    app.call(
        reqwest::Method::POST,
        &paths::webhook_address(webhook),
        who,
        None,
    )
    .await
}

fn outcomes(log: &[WebhookDelivery]) -> Vec<Outcome> {
    log.iter().map(|entry| entry.outcome).collect()
}

#[sqlx::test]
async fn the_log_says_what_became_of_each_request_and_holds_no_content(pool: PgPool) {
    let t = team(pool).await;
    let app = &t.app;
    let ops = app.create_channel(&t.boss, "ops", "public").await;
    let hook = app.create_webhook(&t.boss, ops.id, "Deploys").await;
    assert!(deliveries(app, &t.boss, hook.webhook.id).await.is_empty());

    let posted = app.deliver(&hook, "release 1.2 shipped").await;
    let key = [("Idempotency-Key", "build-9")];
    assert_eq!(
        app.deliver_raw(&hook.url, "build 9 green", &key)
            .await
            .status()
            .as_u16(),
        200
    );
    assert_eq!(
        app.deliver_raw(&hook.url, "build 9 green", &key)
            .await
            .status()
            .as_u16(),
        200
    );
    // The same key with other words is the sender's mistake.
    denied(
        app.deliver_raw(&hook.url, "build 9 red", &key).await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.deliver_body_raw(&hook.url, "not json".to_owned(), &[])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let big = "x".repeat(lazy_domain::MAX_WEBHOOK_BODY_BYTES + 1);
    denied(
        app.deliver_raw(&hook.url, &big, &[]).await,
        ErrorCode::PayloadTooLarge,
    )
    .await;
    // A wrong address belongs to no webhook and is not in anyone's log.
    let wrong = format!(
        "{}/hooks/{}/{}",
        app.server.base_url,
        hook.webhook.id,
        "1".repeat(64)
    );
    denied(app.deliver_raw(&wrong, "x", &[]).await, ErrorCode::NotFound).await;
    assert!(
        app.act(&t.boss, ops.id, "archive")
            .await
            .status()
            .is_success()
    );
    denied(
        app.deliver_raw(&hook.url, "into the archive", &[]).await,
        ErrorCode::Conflict,
    )
    .await;

    let log = deliveries(app, &t.boss, hook.webhook.id).await;
    assert_eq!(
        outcomes(&log),
        vec![
            Outcome::Archived,
            Outcome::TooLarge,
            Outcome::Invalid,
            Outcome::Invalid,
            Outcome::Duplicate,
            Outcome::Posted,
            Outcome::Posted,
        ],
        "newest first"
    );
    assert_eq!(log[6].message_id, Some(posted));
    assert_eq!(
        log[4].message_id, log[5].message_id,
        "a duplicate answers the earlier message"
    );
    assert!(log[4].idempotency_key && log[3].idempotency_key && !log[6].idempotency_key);
    assert!(log[1].body_bytes > lazy_domain::MAX_WEBHOOK_BODY_BYTES as u32);
    assert_eq!(log[2].body_bytes, "not json".len() as u32);
    assert!(
        log.iter()
            .all(|entry| entry.outcome.posted() == entry.message_id.is_some())
    );
    for window in log.windows(2) {
        assert!(window[0].received_at >= window[1].received_at);
    }

    // Nothing that was sent is kept: not the words, not the key.
    let raw = get(app, &t.boss, &paths::webhook_deliveries(hook.webhook.id))
        .await
        .text()
        .await
        .unwrap();
    for secret in ["release 1.2", "build 9", "build-9", "not json", &hook.url] {
        assert!(!raw.contains(secret), "{secret} leaked into the log");
    }
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT row_to_json(d)::text FROM webhook_deliveries d WHERE webhook_id = $1",
    )
    .bind(hook.webhook.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    for row in &stored {
        for secret in ["release 1.2", "build 9", "build-9", "not json"] {
            assert!(!row.contains(secret), "{secret} stored in {row}");
        }
    }
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_run_of_rate_limited_requests_is_one_entry(pool: PgPool) {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            webhook_rate: WebhookRate {
                burst: 2,
                refill_every: Duration::from_secs(3600),
            },
            ..IntegrationOptions::default()
        },
    )
    .await;
    let t = team_with(app).await;
    let app = &t.app;
    let hook = app.create_webhook(&t.boss, app.general, "Noisy").await;
    app.deliver(&hook, "one").await;
    app.deliver(&hook, "two").await;
    for _ in 0..25 {
        denied(
            app.deliver_raw(&hook.url, "too many", &[]).await,
            ErrorCode::RateLimited,
        )
        .await;
    }
    let log = deliveries(app, &t.boss, hook.webhook.id).await;
    assert_eq!(
        outcomes(&log),
        vec![Outcome::RateLimited, Outcome::Posted, Outcome::Posted]
    );
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_log_keeps_only_the_newest_entries(pool: PgPool) {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            webhook_rate: WebhookRate {
                burst: 1000,
                refill_every: Duration::from_secs(1),
            },
            ..IntegrationOptions::default()
        },
    )
    .await;
    let t = team_with(app).await;
    let app = &t.app;
    let hook = app.create_webhook(&t.boss, app.general, "Chatty").await;
    let other = app.create_webhook(&t.boss, app.general, "Quiet").await;
    app.deliver(&other, "untouched").await;
    // Fill the log with refusals (they cost no messages), then post once more.
    for _ in 0..MAX_WEBHOOK_DELIVERIES_KEPT + 5 {
        denied(
            app.deliver_body_raw(&hook.url, "{}".to_owned(), &[]).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    app.deliver(&hook, "last").await;
    let log = deliveries(app, &t.boss, hook.webhook.id).await;
    assert_eq!(log.len(), MAX_WEBHOOK_DELIVERIES_KEPT);
    assert_eq!(log[0].outcome, Outcome::Posted, "the newest is kept");
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM webhook_deliveries WHERE webhook_id = $1")
            .bind(hook.webhook.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(
        stored, MAX_WEBHOOK_DELIVERIES_KEPT as i64,
        "older rows are gone"
    );
    assert_eq!(
        deliveries(app, &t.boss, other.webhook.id).await.len(),
        1,
        "another webhook's log is its own"
    );
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_workspace_list_shows_each_webhook_the_admin_can_see_with_how_it_is_doing(
    pool: PgPool,
) {
    let t = team(pool).await;
    let app = &t.app;
    let ops = app.create_channel(&t.boss, "ops", "public").await;
    let boss_only = app.create_channel(&t.boss, "boss-only", "private").await;
    let owner_only = app.create_channel(&t.owner, "owner-only", "private").await;
    let general = app.create_webhook(&t.boss, app.general, "Deploys").await;
    let quiet = app.create_webhook(&t.boss, ops.id, "Quiet").await;
    let hidden = app.create_webhook(&t.boss, boss_only.id, "Hidden").await;
    let secret = app.create_webhook(&t.owner, owner_only.id, "Owner's").await;
    let gone = app.create_webhook(&t.boss, ops.id, "Gone").await;
    assert!(
        app.call(
            reqwest::Method::DELETE,
            &paths::webhook(gone.webhook.id),
            &t.boss,
            None
        )
        .await
        .status()
        .is_success()
    );
    app.deliver(&general, "ok").await;
    denied(
        app.deliver_body_raw(&general.url, "{}".to_owned(), &[])
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    app.deliver(&hidden, "psst").await;

    let ids = |list: &[WorkspaceWebhook]| -> Vec<WebhookId> {
        list.iter().map(|entry| entry.webhook.id).collect()
    };
    // Each admin sees the public channels' webhooks and those of private channels they are in;
    // a deleted webhook is not listed.
    let boss_view = workspace_list(app, &t.boss).await;
    assert_eq!(
        ids(&boss_view),
        vec![general.webhook.id, quiet.webhook.id, hidden.webhook.id],
        "oldest first"
    );
    let owner_view = workspace_list(app, &t.owner).await;
    assert_eq!(
        ids(&owner_view),
        vec![general.webhook.id, quiet.webhook.id, secret.webhook.id]
    );

    let entry = &boss_view[0];
    assert_eq!(entry.webhook, general.webhook);
    assert_eq!(entry.channel_name.as_str(), "general");
    assert_eq!(boss_view[2].channel_name.as_str(), "boss-only");
    let last = entry.last_delivery.as_ref().unwrap();
    assert_eq!(last.outcome, Outcome::Invalid);
    assert_eq!(entry.recent_failures, 1);
    assert!(boss_view[1].last_delivery.is_none());
    assert_eq!(boss_view[1].recent_failures, 0);
    assert_eq!(
        boss_view[2].last_delivery.as_ref().unwrap().outcome,
        Outcome::Posted
    );
    // The list never carries an address.
    let raw = get(app, &t.boss, &paths::workspace_webhooks(app.workspace_id))
        .await
        .text()
        .await
        .unwrap();
    assert!(!raw.contains("/hooks/"));

    // A member may not list; nor read a log.
    denied(
        get(app, &t.alice, &paths::workspace_webhooks(app.workspace_id)).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        get(
            app,
            &t.alice,
            &paths::webhook_deliveries(general.webhook.id),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    // An admin outside a private channel is told its webhook is not there; so is anyone asking
    // about one that never existed.
    denied(
        get(app, &t.boss, &paths::webhook_deliveries(secret.webhook.id)).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        get(
            app,
            &t.boss,
            &paths::webhook_deliveries(WebhookId::generate()),
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    // A deleted webhook keeps its log for whoever could read it.
    app.deliver(&quiet, "before").await;
    assert!(
        app.call(
            reqwest::Method::DELETE,
            &paths::webhook(quiet.webhook.id),
            &t.boss,
            None
        )
        .await
        .status()
        .is_success()
    );
    assert_eq!(deliveries(app, &t.boss, quiet.webhook.id).await.len(), 1);
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_new_address_replaces_the_old_one_and_keeps_everything_else(pool: PgPool) {
    let t = team(pool).await;
    let app = &t.app;
    let hook = app.create_webhook(&t.boss, app.general, "Deploys").await;
    app.deliver(&hook, "before").await;

    denied(
        new_address_raw(app, &t.alice, hook.webhook.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        new_address_raw(app, &t.boss, WebhookId::generate()).await,
        ErrorCode::NotFound,
    )
    .await;

    let response = new_address_raw(app, &t.owner, hook.webhook.id).await;
    assert!(response.status().is_success(), "{}", response.status());
    let renewed: CreatedWebhook = response.json().await.unwrap();
    assert_eq!(renewed.webhook, hook.webhook, "the same webhook and bot");
    assert_ne!(renewed.url, hook.url);
    assert!(renewed.url.starts_with(&app.server.base_url));

    // The old address is dead at once and answers like any wrong one; the new one posts as the
    // same bot.
    denied(
        app.deliver_raw(&hook.url, "old", &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    app.deliver(&renewed, "after").await;
    let log = deliveries(app, &t.boss, hook.webhook.id).await;
    assert_eq!(outcomes(&log), vec![Outcome::Posted, Outcome::Posted]);
    let authors: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT DISTINCT author_id FROM messages WHERE author_id = $1")
            .bind(hook.webhook.bot_user_id.as_uuid())
            .fetch_all(&app.pool)
            .await
            .unwrap();
    assert_eq!(authors.len(), 1);

    // A deleted webhook cannot be given an address again.
    assert!(
        app.call(
            reqwest::Method::DELETE,
            &paths::webhook(hook.webhook.id),
            &t.boss,
            None
        )
        .await
        .status()
        .is_success()
    );
    denied(
        new_address_raw(app, &t.boss, hook.webhook.id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.deliver_raw(&renewed.url, "after deletion", &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    t.app.server.shutdown().await.unwrap();
}
