//! Apps and the bot API (D35), end to end: making an app and its token, scopes, channels chosen by
//! an admin or joined by the bot, posting in Lazy's and Slack's shapes, idempotency, the rate
//! limit, tokens kept apart from sessions, a new token, and deleting.

mod common;

use common::*;
use lazy_domain::app::AppScope;
use lazy_protocol::api::paths;
use lazy_protocol::{
    AppsResponse, BotConversationsResponse, BotMeResponse, BotMessageResponse, CreatedAppResponse,
    ErrorCode,
};
use lazy_server::{IntegrationOptions, WebhookRate};
use serde_json::{Value, json};
use sqlx::PgPool;

struct Team {
    app: App,
    boss: Account,
    alice: Account,
}

async fn team(pool: PgPool, rate: WebhookRate) -> Team {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            webhook_rate: rate,
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

async fn create(t: &Team, name: &str, scopes: &[AppScope]) -> reqwest::Response {
    t.app
        .call(
            reqwest::Method::POST,
            &paths::apps(t.app.workspace_id),
            &t.boss,
            Some(json!({ "name": name, "scopes": scopes })),
        )
        .await
}

/// A request to the bot API with a raw token.
async fn bot(
    app: &App,
    method: reqwest::Method,
    path: &str,
    token: &str,
    body: Option<Value>,
    key: Option<&str>,
) -> reqwest::Response {
    let mut request = app.http.request(method, app.url(path)).bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    request.send().await.unwrap()
}

async fn post(app: &App, token: &str, body: Value, key: Option<&str>) -> reqwest::Response {
    bot(
        app,
        reqwest::Method::POST,
        paths::BOT_MESSAGES_ROUTE,
        token,
        Some(body),
        key,
    )
    .await
}

#[sqlx::test]
async fn an_app_posts_as_its_bot_within_its_scopes(pool: PgPool) {
    let t = team(pool, WebhookRate::DEFAULT).await;
    let app = &t.app;

    // Only admins and owners make apps; a name and a scope are needed.
    let as_member = app
        .call(
            reqwest::Method::POST,
            &paths::apps(app.workspace_id),
            &t.alice,
            Some(json!({ "name": "Deployer", "scopes": ["chat:write"] })),
        )
        .await;
    denied(as_member, ErrorCode::Forbidden).await;
    denied(
        create(&t, "  ", &[AppScope::ChatWrite]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    denied(create(&t, "Deployer", &[]).await, ErrorCode::InvalidRequest).await;

    let made: CreatedAppResponse = create(&t, "Deployer", &AppScope::DEFAULT)
        .await
        .json()
        .await
        .unwrap();
    let token = made.token.clone();
    assert!(token.starts_with("lzb_") && token.len() == 68, "{token}");
    assert_eq!(made.app.token_hint, &token[token.len() - 4..]);
    // Only a hash of the token is kept, and the list never shows it.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM apps WHERE id = $1")
        .bind(made.app.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    let listed = app
        .call(
            reqwest::Method::GET,
            &paths::apps(app.workspace_id),
            &t.boss,
            None,
        )
        .await
        .text()
        .await
        .unwrap();
    assert!(!listed.contains(&token));
    let listed: AppsResponse = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed.apps.len(), 1);

    // The token is the bot's, not a session; a session is not a bot token.
    let me: BotMeResponse = bot(
        app,
        reqwest::Method::GET,
        paths::BOT_ME_ROUTE,
        &token,
        None,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(
        (me.app_id, me.bot_user_id),
        (made.app.id, made.app.bot_user_id)
    );
    denied(
        app.http
            .get(app.url(&paths::apps(app.workspace_id)))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap(),
        ErrorCode::Unauthenticated,
    )
    .await;
    denied(
        bot(
            app,
            reqwest::Method::GET,
            paths::BOT_ME_ROUTE,
            &t.boss.token,
            None,
            None,
        )
        .await,
        ErrorCode::Unauthenticated,
    )
    .await;
    denied(
        bot(
            app,
            reqwest::Method::GET,
            paths::BOT_ME_ROUTE,
            "lzb_nope",
            None,
            None,
        )
        .await,
        ErrorCode::Unauthenticated,
    )
    .await;

    // Not in a channel yet: posting there is refused; the admin adds it to #ops.
    let ops = app.create_channel(&t.boss, "ops", "public").await.id;
    denied(
        post(app, &token, json!({"channel": "#ops", "text": "hi"}), None).await,
        ErrorCode::NotFound,
    )
    .await;
    let with_ops: lazy_protocol::App = app
        .call(
            reqwest::Method::PUT,
            &paths::app_channel(made.app.id, ops),
            &t.boss,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(with_ops.channels.len(), 1);
    let channels: BotConversationsResponse = bot(
        app,
        reqwest::Method::GET,
        paths::BOT_CONVERSATIONS_ROUTE,
        &token,
        None,
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(channels.conversations[0].name.as_str(), "ops");

    // Lazy's text, by name; Slack's body, by id; a reply in its thread.
    let first: BotMessageResponse = post(
        app,
        &token,
        json!({"channel": "ops", "text": "Deploy *done*"}),
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(first.conversation_id, ops);
    let message = app.get_message(&t.boss, first.message_id).await;
    assert_eq!(message.author_id, made.app.bot_user_id);
    assert_eq!(message.document.plain_text(), "Deploy done");
    let slack: BotMessageResponse = post(
        app,
        &token,
        json!({"channel": ops.to_string(), "text": "fallback",
               "attachments": [{"title": "Build 7", "title_link": "https://ci.example/7"}],
               "thread_ts": first.message_id.to_string()}),
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    let reply = app.get_message(&t.boss, slack.message_id).await;
    assert_eq!(reply.thread_root, Some(first.message_id));
    assert!(reply.document.plain_text().contains("Build 7"));

    // The same Idempotency-Key posts once.
    let a: BotMessageResponse = post(
        app,
        &token,
        json!({"channel": "ops", "text": "once"}),
        Some("k-1"),
    )
    .await
    .json()
    .await
    .unwrap();
    let b: BotMessageResponse = post(
        app,
        &token,
        json!({"channel": "ops", "text": "once"}),
        Some("k-1"),
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(a.message_id, b.message_id);

    // Without channels:join it cannot join #general; given the scope, it can.
    let join = paths::bot_join(app.general);
    denied(
        bot(app, reqwest::Method::POST, &join, &token, None, None).await,
        ErrorCode::Forbidden,
    )
    .await;
    let widened = app
        .call(
            reqwest::Method::PATCH,
            &paths::app(made.app.id),
            &t.boss,
            Some(json!({"scopes": ["chat:write", "channels:read", "channels:join"]})),
        )
        .await;
    assert!(widened.status().is_success());
    assert_eq!(
        bot(app, reqwest::Method::POST, &join, &token, None, None)
            .await
            .status()
            .as_u16(),
        204
    );
    // A private channel it is not in is not found, joining or posting.
    let secret = app.create_channel(&t.boss, "secret", "private").await.id;
    denied(
        bot(
            app,
            reqwest::Method::POST,
            &paths::bot_join(secret),
            &token,
            None,
            None,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        post(
            app,
            &token,
            json!({"channel": secret.to_string(), "text": "x"}),
            None,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;

    // Without chat:write it cannot post.
    let narrowed = app
        .call(
            reqwest::Method::PATCH,
            &paths::app(made.app.id),
            &t.boss,
            Some(json!({"scopes": ["channels:read"]})),
        )
        .await;
    assert!(narrowed.status().is_success());
    denied(
        post(app, &token, json!({"channel": "ops", "text": "x"}), None).await,
        ErrorCode::Forbidden,
    )
    .await;

    // A new token replaces the old at once.
    let renewed: CreatedAppResponse = app
        .call(
            reqwest::Method::POST,
            &paths::app_token(made.app.id),
            &t.boss,
            None,
        )
        .await
        .json()
        .await
        .unwrap();
    assert_ne!(renewed.token, token);
    denied(
        bot(
            app,
            reqwest::Method::GET,
            paths::BOT_ME_ROUTE,
            &token,
            None,
            None,
        )
        .await,
        ErrorCode::Unauthenticated,
    )
    .await;

    // Deleting stops the token and takes the bot out of its channels; its messages stay.
    let gone = app
        .call(
            reqwest::Method::DELETE,
            &paths::app(made.app.id),
            &t.boss,
            None,
        )
        .await;
    assert_eq!(gone.status().as_u16(), 204);
    denied(
        bot(
            app,
            reqwest::Method::GET,
            paths::BOT_ME_ROUTE,
            &renewed.token,
            None,
            None,
        )
        .await,
        ErrorCode::Unauthenticated,
    )
    .await;
    let still_in: i64 =
        sqlx::query_scalar("SELECT count(*) FROM conversation_members WHERE user_id = $1")
            .bind(made.app.bot_user_id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(still_in, 0);
    assert_eq!(
        app.get_message(&t.boss, first.message_id).await.author_id,
        made.app.bot_user_id
    );
    t.app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn posting_is_rate_limited_per_app(pool: PgPool) {
    let t = team(
        pool,
        WebhookRate {
            burst: 2,
            refill_every: std::time::Duration::from_secs(3600),
        },
    )
    .await;
    let app = &t.app;
    let made: CreatedAppResponse = create(&t, "Chatty", &[AppScope::ChatWrite])
        .await
        .json()
        .await
        .unwrap();
    let put = app
        .call(
            reqwest::Method::PUT,
            &paths::app_channel(made.app.id, app.general),
            &t.boss,
            None,
        )
        .await;
    assert!(put.status().is_success());
    for n in 0..2 {
        let ok = post(
            app,
            &made.token,
            json!({"channel": "general", "text": format!("{n}")}),
            None,
        )
        .await;
        assert!(ok.status().is_success(), "{}", ok.status());
    }
    let limited = post(
        app,
        &made.token,
        json!({"channel": "general", "text": "3"}),
        None,
    )
    .await;
    assert_eq!(limited.status().as_u16(), 429);
    assert!(limited.headers().get("retry-after").is_some());
    t.app.server.shutdown().await.unwrap();
}
