//! Incoming webhooks and bot identity: creating and revoking them, delivering through them, what
//! the bot is and is not, rate limits, idempotency, delivery and replay, and what is and is not
//! logged.

mod common;

use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use common::*;
use futures_util::future::join_all;
use lazy_domain::{ConversationId, MessageDocument, MessageId, UserId, WebhookId};
use lazy_protocol::{
    CreatedWebhook, ErrorCode, Event, EventEnvelope, IncomingWebhookResponse, PROTOCOL_VERSION,
};
use lazy_server::{IntegrationOptions, WebhookRate};
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// An owner and an admin, and three ordinary members, all in #general.
struct Chat {
    app: App,
    owner: Account,
    boss: Account,
    alice: Account,
    bob: Account,
    carol: Account,
}

async fn chat_with(app: App) -> Chat {
    let owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    Chat {
        app,
        owner,
        boss,
        alice,
        bob,
        carol,
    }
}

async fn chat(pool: PgPool) -> Chat {
    chat_with(App::start(pool).await).await
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

/// The secret in a webhook's address.
fn secret_of(hook: &CreatedWebhook) -> String {
    hook.url.rsplit('/').next().unwrap().to_owned()
}

async fn replay_from(
    app: &App,
    who: &Account,
    cursor: lazy_domain::EventSeq,
) -> Vec<EventEnvelope> {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, cursor).await;
    ws.replay().await.0
}

async fn bot_messages(app: &App, bot: UserId) -> Vec<(MessageId, i64)> {
    let rows: Vec<(uuid::Uuid, i64)> =
        sqlx::query_as("SELECT id, seq FROM messages WHERE author_id = $1 ORDER BY seq")
            .bind(bot.as_uuid())
            .fetch_all(&app.pool)
            .await
            .unwrap();
    rows.into_iter()
        .map(|(id, seq)| (MessageId::from_uuid(id), seq))
        .collect()
}

async fn members_of(app: &App, who: &Account, conversation: ConversationId) -> Vec<UserId> {
    let response = app
        .call(
            reqwest::Method::GET,
            &lazy_protocol::api::paths::conversation_action(conversation, "members"),
            who,
            None,
        )
        .await;
    assert!(response.status().is_success());
    response
        .json::<lazy_protocol::ConversationMembersResponse>()
        .await
        .unwrap()
        .user_ids
}

fn is_bot_username(name: &str) -> bool {
    name.strip_prefix("bot-").is_some_and(|rest| {
        rest.len() == 8
            && rest
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

// --- Creating, listing, revoking ------------------------------------------------------------

#[sqlx::test]
async fn admins_and_owners_make_webhooks_and_each_has_a_bot_in_the_channel(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let before = app.bootstrap_ok(&c.alice).await.workspaces[0]
        .conversations
        .iter()
        .find(|conversation| conversation.id == app.general)
        .unwrap()
        .member_count;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    let cursor = app.head().await;

    // An ordinary member may not; someone who cannot read the channel is told it is not there.
    denied(
        app.create_webhook_raw(&c.alice, app.general, "Deploys")
            .await,
        ErrorCode::Forbidden,
    )
    .await;
    denied(
        app.create_webhook_raw(&c.boss, hush.id, "Deploys").await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.create_webhook_raw(&c.owner, hush.id, "Deploys").await,
        ErrorCode::NotFound,
    )
    .await;
    let nobody = app
        .http
        .post(app.url(&lazy_protocol::api::paths::webhooks(app.general)))
        .json(&serde_json::json!({"name": "Deploys"}))
        .send()
        .await
        .unwrap();
    denied(nobody, ErrorCode::Unauthenticated).await;
    // A name has to be a name.
    denied(
        app.create_webhook_raw(&c.boss, app.general, "   ").await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // Nothing was made by any of that.
    assert_eq!(app.head().await, cursor);

    // An admin may.
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    assert_eq!(hook.webhook.name.as_str(), "Deploys");
    assert_eq!(hook.webhook.conversation_id, app.general);
    assert_eq!(hook.webhook.created_by, c.boss.user.id);
    let bot = hook.webhook.bot_user_id;

    // The bot: a member of the workspace with the role `member`, named from the webhook's id, shown
    // by the webhook's name, marked as a bot, with no way to sign in.
    let boot = app.bootstrap_ok(&c.alice).await;
    let member = boot.workspaces[0]
        .members
        .iter()
        .find(|user| user.id == bot)
        .expect("the bot is in the member list");
    assert!(member.is_bot && is_bot_username(member.username.as_str()));
    assert_eq!(member.display_name.as_str(), "Deploys");
    assert!(
        boot.workspaces[0]
            .members
            .iter()
            .filter(|user| user.id != bot)
            .all(|user| !user.is_bot)
    );
    let (role, is_bot, hash): (String, bool, String) = sqlx::query_as(
        "SELECT wm.role, u.is_bot, u.password_hash FROM users u \
         JOIN workspace_members wm ON wm.user_id = u.id WHERE u.id = $1",
    )
    .bind(bot.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(
        (role.as_str(), is_bot, hash.as_str()),
        ("member", true, "!")
    );
    // It is in the channel, and the channel counts it.
    assert!(members_of(app, &c.alice, app.general).await.contains(&bot));
    let after = boot.workspaces[0]
        .conversations
        .iter()
        .find(|conversation| conversation.id == app.general)
        .unwrap()
        .member_count;
    assert_eq!(after, before + 1);
    // Everyone hears of it as of any member: the bot joined the workspace, then the channel.
    let events = replay_from(app, &c.carol, cursor).await;
    assert_eq!(events.len(), 2);
    match (&events[0].event, &events[1].event) {
        (
            Event::MemberJoined { user },
            Event::MemberAdded {
                conversation,
                user_id,
            },
        ) => {
            assert!(user.is_bot && user.id == bot);
            assert_eq!(
                (*user_id, conversation.id, conversation.member_count),
                (bot, app.general, after)
            );
        }
        other => panic!("unexpected events {other:?}"),
    }
    assert_eq!(events[1].seq.get(), events[0].seq.get() + 1);

    // An owner may too.
    let second = app.create_webhook(&c.owner, app.general, "Alerts").await;
    assert_ne!(second.webhook.id, hook.webhook.id);

    // Only channels take them, and an archived one does not.
    let direct = app.open(&c.boss, &[&c.alice]).await;
    denied(
        app.create_webhook_raw(&c.boss, direct.id, "Deploys").await,
        ErrorCode::InvalidRequest,
    )
    .await;
    let ops = app.create_channel(&c.boss, "ops", "public").await;
    assert!(
        app.act(&c.boss, ops.id, "archive")
            .await
            .status()
            .is_success()
    );
    denied(
        app.create_webhook_raw(&c.boss, ops.id, "Deploys").await,
        ErrorCode::Conflict,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn a_channel_has_at_most_ten_webhooks_and_revoking_one_makes_room(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let ops = app.create_channel(&c.boss, "ops", "public").await;
    let mut hooks = Vec::new();
    for n in 0..lazy_domain::MAX_WEBHOOKS_PER_CONVERSATION {
        hooks.push(
            app.create_webhook(&c.boss, ops.id, &format!("Hook {n}"))
                .await,
        );
    }
    let response = app
        .create_webhook_raw(&c.boss, ops.id, "One too many")
        .await;
    assert_eq!(response.status().as_u16(), 422);
    denied(response, ErrorCode::LimitReached).await;
    // Another channel has its own ten.
    app.create_webhook(&c.boss, app.general, "Elsewhere").await;
    // Revoking one frees a place; the usernames of all the bots are different.
    assert_eq!(
        app.delete_webhook_raw(&c.boss, hooks[3].webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );
    app.create_webhook(&c.boss, ops.id, "Replacement").await;
    let names: std::collections::HashSet<String> =
        sqlx::query_scalar("SELECT username FROM users WHERE is_bot")
            .fetch_all(&app.pool)
            .await
            .unwrap()
            .into_iter()
            .collect();
    assert_eq!(names.len(), lazy_domain::MAX_WEBHOOKS_PER_CONVERSATION + 2);
    assert!(names.iter().all(|name| is_bot_username(name)));
    finish(c).await;
}

#[sqlx::test]
async fn the_address_is_built_from_the_public_url_or_the_listening_address(pool: PgPool) {
    // Without a setting: http:// and the address the server listens on.
    let c = chat(pool.clone()).await;
    let hook = c
        .app
        .create_webhook(&c.boss, c.app.general, "Deploys")
        .await;
    let expected = format!("{}/hooks/{}/", c.app.server.base_url, hook.webhook.id);
    assert!(hook.url.starts_with(&expected), "{}", hook.url);
    let secret = secret_of(&hook);
    assert_eq!(secret.len(), 64);
    assert!(
        secret
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    // The address works as given.
    c.app.deliver(&hook, "hello").await;
    finish(c).await;
}

#[sqlx::test]
async fn a_configured_public_url_is_used_and_a_trailing_slash_does_not_double(pool: PgPool) {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            public_url: Some("https://chat.example.com/lazy/".into()),
            ..IntegrationOptions::default()
        },
    )
    .await;
    let c = chat_with(app).await;
    let hook = c
        .app
        .create_webhook(&c.boss, c.app.general, "Deploys")
        .await;
    let prefix = format!("https://chat.example.com/lazy/hooks/{}/", hook.webhook.id);
    assert!(hook.url.starts_with(&prefix), "{}", hook.url);
    assert_eq!(hook.url.len(), prefix.len() + 64);
    finish(c).await;
}

#[sqlx::test]
async fn listing_is_for_admins_and_owners_and_never_shows_a_secret(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let first = app.create_webhook(&c.boss, app.general, "First").await;
    let second = app.create_webhook(&c.owner, app.general, "Second").await;
    let gone = app.create_webhook(&c.boss, app.general, "Gone").await;
    assert_eq!(
        app.delete_webhook_raw(&c.boss, gone.webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );

    for who in [&c.boss, &c.owner] {
        let listed = app.list_webhooks(who, app.general).await;
        assert_eq!(
            listed,
            vec![first.webhook.clone(), second.webhook.clone()],
            "oldest first, revoked left out"
        );
    }
    // The answer has no address and no secret in any form.
    let raw = app
        .list_webhooks_raw(&c.boss, app.general)
        .await
        .text()
        .await
        .unwrap();
    for hook in [&first, &second] {
        assert!(
            !raw.contains(&secret_of(hook)) && !raw.contains("/hooks/") && !raw.contains("url")
        );
    }
    denied(
        app.list_webhooks_raw(&c.alice, app.general).await,
        ErrorCode::Forbidden,
    )
    .await;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    denied(
        app.list_webhooks_raw(&c.boss, hush.id).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.list_webhooks_raw(&c.boss, ConversationId::generate())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn revoking_stops_the_address_removes_the_bot_and_keeps_what_it_said(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let bot = hook.webhook.bot_user_id;
    let first = app.deliver(&hook, "deployed 1").await;
    let second = app.deliver(&hook, "deployed 2").await;
    let members_before = app.state_of(&c.alice, app.general).await.unread_count;
    let count_before = app.bootstrap_ok(&c.alice).await.workspaces[0]
        .conversations
        .iter()
        .find(|conversation| conversation.id == app.general)
        .unwrap()
        .member_count;
    let cursor = app.head().await;

    // Only an admin or owner who can see the webhook may; others are told what they may know.
    denied(
        app.delete_webhook_raw(&c.alice, hook.webhook.id).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert_eq!(app.head().await, cursor);
    assert_eq!(
        app.delete_webhook_raw(&c.boss, hook.webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );

    // The bot left the channel, in the open: it is told to the members and the count follows.
    let events = replay_from(app, &c.carol, cursor).await;
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| match &e.event {
            Event::MemberRemoved { user_id, .. } if *user_id == bot => "member_removed",
            Event::ConversationUpdated { conversation }
                if conversation.member_count == count_before - 1 =>
            {
                "conversation_updated"
            }
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["member_removed", "conversation_updated"]);
    assert!(!members_of(app, &c.alice, app.general).await.contains(&bot));

    // The address is gone, like one that never existed; revoking again is quiet.
    let after = app.deliver_raw(&hook.url, "too late", &[]).await;
    denied(after, ErrorCode::NotFound).await;
    let head = app.head().await;
    assert_eq!(
        app.delete_webhook_raw(&c.boss, hook.webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(
        app.delete_webhook_raw(&c.owner, WebhookId::generate())
            .await
            .status()
            .as_u16(),
        204
    );
    assert_eq!(app.head().await, head);
    assert!(app.list_webhooks(&c.boss, app.general).await.is_empty());

    // What it said stays, with the bot as its author, and the bot is still a user people can see.
    let page = app.history_ok(&c.alice, app.general, &[]).await;
    let ours: Vec<&lazy_protocol::Message> = page
        .messages
        .iter()
        .filter(|m| m.author_id == bot)
        .collect();
    assert_eq!(
        ours.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![first, second]
    );
    let boot = app.bootstrap_ok(&c.alice).await;
    let user = boot.workspaces[0]
        .members
        .iter()
        .find(|u| u.id == bot)
        .expect("kept");
    assert!(user.is_bot && user.display_name.as_str() == "Deploys");
    assert_eq!(
        app.state_of(&c.alice, app.general).await.unread_count,
        members_before
    );

    // Revoking one webhook does not touch another's.
    let other = app.create_webhook(&c.boss, app.general, "Other").await;
    let third = app.create_webhook(&c.boss, app.general, "Third").await;
    app.delete_webhook_raw(&c.boss, other.webhook.id).await;
    app.deliver(&third, "still here").await;
    finish(c).await;
}

// --- Delivering -------------------------------------------------------------------------------

#[sqlx::test]
async fn a_delivery_is_a_message_by_the_bot_with_formatting_and_emoji_and_no_mentions(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let bot = hook.webhook.bot_user_id;
    let cursor = app.head().await;

    // Composer source: formatting, shortcodes, code blocks.
    let source =
        "**Deploy finished** :tada: in `3.2s`\n\n```\ncargo build --release\n```\n\n- one\n- two";
    let id = app.deliver(&hook, source).await;
    let message = app.get_message(&c.alice, id).await;
    assert_eq!(message.author_id, bot);
    let expected =
        MessageDocument::from_markdown(&lazy_domain::emoji::replace_shortcodes(source)).unwrap();
    assert_eq!(message.document, expected);
    let text = message.document.plain_text();
    assert!(
        text.contains('🎉') && !text.contains(":tada:") && text.contains("cargo build --release")
    );
    assert_eq!(
        (message.revision, message.deleted, message.thread_root),
        (1, false, None)
    );

    // @names stay text: nobody is notified, nothing is stored as a mention, and nobody is counted.
    let id = app
        .deliver(&hook, "@alice please look, @channel and @carol too")
        .await;
    let message = app.get_message(&c.alice, id).await;
    assert!(message.mentions.is_empty());
    assert_eq!(
        message.document.plain_text(),
        "@alice please look, @channel and @carol too"
    );
    assert!(message.document.mentions().is_empty());
    assert_eq!(app.state_of(&c.alice, app.general).await.mention_count, 0);
    assert_eq!(app.state_of(&c.carol, app.general).await.mention_count, 0);
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM message_mentions WHERE message_id = $1")
            .bind(id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored, 0);

    // It went through the ordinary send: the event, the unread counts, the bot's own position.
    let events = replay_from(app, &c.bob, cursor).await;
    let created: Vec<&lazy_protocol::Message> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert_eq!(created.len(), 2);
    assert!(
        created
            .iter()
            .all(|m| m.author_id == bot && m.conversation_id == app.general)
    );
    assert_eq!(app.state_of(&c.alice, app.general).await.unread_count, 2);
    let bot_state: i64 = sqlx::query_scalar(
        "SELECT last_read FROM conversation_user_state WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(bot.as_uuid())
    .bind(app.general.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(bot_state, created[1].seq.get());
    finish(c).await;
}

#[sqlx::test]
async fn a_delivery_that_cannot_be_made_says_why(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let head = app.head().await;

    // Nothing to say is no message.
    for body in [
        r#"{"text": ""}"#,
        r#"{"text": "   \n "}"#,
        "{}",
        r#"{"text": 5}"#,
        "not json",
        "",
    ] {
        let response = app.deliver_body_raw(&hook.url, body.to_owned(), &[]).await;
        denied(response, ErrorCode::InvalidRequest).await;
    }
    // Too much to say is too much, whether it announces its size or not.
    let big = "x".repeat(lazy_domain::MAX_WEBHOOK_BODY_BYTES + 1);
    let response = app.deliver_raw(&hook.url, &big, &[]).await;
    assert_eq!(response.status().as_u16(), 413);
    denied(response, ErrorCode::PayloadTooLarge).await;
    assert_eq!(app.head().await, head, "none of that was posted");
    // A body within the limit can still hold too much text for one message: a message is at most
    // 40,000 bytes of text, less than the body may carry.
    let long = "w".repeat(lazy_domain::document::MAX_TEXT_BYTES + 1);
    denied(
        app.deliver_raw(&hook.url, &long, &[]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    assert_eq!(app.head().await, head, "none of that was posted");
    let longest = "w".repeat(lazy_domain::document::MAX_TEXT_BYTES);
    assert_eq!(
        app.deliver_raw(&hook.url, &longest, &[])
            .await
            .status()
            .as_u16(),
        200
    );
    let head = app.head().await;
    // The same without announcing its size: sent in chunks.
    let path = hook.url.strip_prefix(&app.server.base_url).unwrap();
    let chunks = [
        "{\"text\": \"".to_owned(),
        "y".repeat(lazy_domain::MAX_WEBHOOK_BODY_BYTES),
        "\"}".to_owned(),
    ];
    let (status, body) = chunked_post(app.server.addr, path, &chunks).await;
    assert_eq!(status, 413, "{body}");
    assert!(body.contains("payload_too_large"));
    assert_eq!(app.head().await, head, "nothing of that was posted");

    // An archived channel takes nothing; once it is open again it does.
    let ops = app.create_channel(&c.boss, "ops", "public").await;
    let ops_hook = app.create_webhook(&c.boss, ops.id, "Ops").await;
    assert!(
        app.act(&c.boss, ops.id, "archive")
            .await
            .status()
            .is_success()
    );
    let response = app.deliver_raw(&ops_hook.url, "hello", &[]).await;
    assert_eq!(response.status().as_u16(), 409);
    denied(response, ErrorCode::Conflict).await;
    assert!(
        app.act(&c.boss, ops.id, "unarchive")
            .await
            .status()
            .is_success()
    );
    app.deliver(&ops_hook, "hello again").await;
    finish(c).await;
}

/// A `POST` with a chunked body, written by hand: the client library announces a size, and this
/// one must not.
async fn chunked_post(addr: std::net::SocketAddr, path: &str, chunks: &[String]) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
    );
    for chunk in chunks {
        request.push_str(&format!("{:x}\r\n{chunk}\r\n", chunk.len()));
    }
    request.push_str("0\r\n\r\n");
    // The server may answer and close before all of a large body is written.
    let _ = stream.write_all(request.as_bytes()).await;
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, text)
}

#[sqlx::test]
async fn a_wrong_unknown_or_revoked_address_all_answer_the_same(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let live = app.create_webhook(&c.boss, app.general, "Live").await;
    let revoked = app.create_webhook(&c.boss, app.general, "Revoked").await;
    assert_eq!(
        app.delete_webhook_raw(&c.boss, revoked.webhook.id)
            .await
            .status()
            .as_u16(),
        204
    );

    let wrong_secret = format!(
        "{}/hooks/{}/{}",
        app.server.base_url,
        live.webhook.id,
        "0".repeat(64)
    );
    let short_secret = format!("{}/hooks/{}/abc", app.server.base_url, live.webhook.id);
    let unknown_id = format!(
        "{}/hooks/{}/{}",
        app.server.base_url,
        WebhookId::generate(),
        secret_of(&live)
    );
    let not_an_id = format!(
        "{}/hooks/not-an-id/{}",
        app.server.base_url,
        secret_of(&live)
    );
    let revoked_address = revoked.url.clone();
    let other_secret = format!(
        "{}/hooks/{}/{}",
        app.server.base_url,
        revoked.webhook.id,
        secret_of(&live)
    );
    let mut seen: Vec<(u16, String)> = Vec::new();
    for url in [
        &wrong_secret,
        &short_secret,
        &unknown_id,
        &not_an_id,
        &revoked_address,
        &other_secret,
    ] {
        let response = app.deliver_raw(url, "hello", &[]).await;
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        // Whatever was guessed is never in the answer.
        let guessed = url.rsplit('/').next().unwrap();
        assert!(!body.contains(guessed) || guessed.len() < 4, "{body}");
        seen.push((status, body));
    }
    assert!(seen.iter().all(|answer| answer == &seen[0]), "{seen:?}");
    assert_eq!(seen[0].0, 404);
    let error: lazy_protocol::ApiError = serde_json::from_str(&seen[0].1).unwrap();
    assert_eq!(error.code, ErrorCode::NotFound);
    // And the real one still works: none of the guesses harmed it.
    app.deliver(&live, "hello").await;
    // The address is not a way to read or list anything: only POST is served.
    let response = app.http.get(&live.url).send().await.unwrap();
    assert!(response.status().is_client_error());
    finish(c).await;
}

#[sqlx::test]
async fn an_idempotency_key_makes_a_retry_post_once(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let other = app.create_webhook(&c.boss, app.general, "Other").await;
    let key = [("Idempotency-Key", "build-4711")];
    let cursor = app.head().await;

    let first = app.deliver_raw(&hook.url, "built", &key).await;
    assert_eq!(first.status().as_u16(), 200);
    let first = first
        .json::<IncomingWebhookResponse>()
        .await
        .unwrap()
        .message_id;
    let head = app.head().await;
    let again = app.deliver_raw(&hook.url, "built", &key).await;
    assert_eq!(again.status().as_u16(), 200);
    assert_eq!(
        again
            .json::<IncomingWebhookResponse>()
            .await
            .unwrap()
            .message_id,
        first
    );
    assert_eq!(app.head().await, head, "the retry posted nothing");
    assert_eq!(bot_messages(app, hook.webhook.bot_user_id).await.len(), 1);

    // Another key posts again; the same key on another webhook is another message; the same key
    // with other words is a conflict, not a second message; without a key every delivery is new.
    let second = app
        .deliver_raw(&hook.url, "built", &[("Idempotency-Key", "build-4712")])
        .await;
    assert_ne!(
        second
            .json::<IncomingWebhookResponse>()
            .await
            .unwrap()
            .message_id,
        first
    );
    let elsewhere = app.deliver_raw(&other.url, "built", &key).await;
    assert_ne!(
        elsewhere
            .json::<IncomingWebhookResponse>()
            .await
            .unwrap()
            .message_id,
        first
    );
    denied(
        app.deliver_raw(&hook.url, "different words", &key).await,
        ErrorCode::Conflict,
    )
    .await;
    let (a, b) = (
        app.deliver(&hook, "same").await,
        app.deliver(&hook, "same").await,
    );
    assert_ne!(a, b);
    assert_eq!(bot_messages(app, hook.webhook.bot_user_id).await.len(), 4);

    // Keys are 1 to 64 printable ASCII characters.
    for bad in ["x".repeat(65), String::new()] {
        let response = app
            .deliver_raw(&hook.url, "hello", &[("Idempotency-Key", bad.as_str())])
            .await;
        denied(response, ErrorCode::InvalidRequest).await;
    }
    let longest = "k".repeat(64);
    assert_eq!(
        app.deliver_raw(&hook.url, "hello", &[("Idempotency-Key", longest.as_str())])
            .await
            .status()
            .as_u16(),
        200
    );
    let spaced = app
        .deliver_raw(&hook.url, "hello", &[("Idempotency-Key", "with spaces ~!")])
        .await;
    assert_eq!(spaced.status().as_u16(), 200);

    // A retry of an address that has been revoked is as dead as any other delivery to it.
    app.delete_webhook_raw(&c.boss, hook.webhook.id).await;
    denied(
        app.deliver_raw(&hook.url, "built", &key).await,
        ErrorCode::NotFound,
    )
    .await;
    // The retry is in the log once: one created event for it.
    let created = replay_from(app, &c.alice, cursor)
        .await
        .iter()
        .filter(
            |e| matches!(&e.event, Event::MessageCreated { message, .. } if message.id == first),
        )
        .count();
    assert_eq!(created, 1);
    finish(c).await;
}

// --- Rate limit -------------------------------------------------------------------------------

#[sqlx::test]
async fn a_webhook_that_posts_too_fast_is_refused_until_a_token_comes_back(pool: PgPool) {
    let app = App::start_with_integrations(
        pool,
        IntegrationOptions {
            webhook_rate: WebhookRate {
                burst: 3,
                refill_every: Duration::from_millis(1500),
            },
            ..IntegrationOptions::default()
        },
    )
    .await;
    let c = chat_with(app).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Noisy").await;
    let calm = app.create_webhook(&c.boss, app.general, "Calm").await;

    for n in 0..3 {
        app.deliver(&hook, &format!("burst {n}")).await;
    }
    let refused = app.deliver_raw(&hook.url, "one too many", &[]).await;
    assert_eq!(refused.status().as_u16(), 429);
    let retry_after: u64 = refused
        .headers()
        .get("retry-after")
        .expect("a 429 says when to retry")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!((1..=2).contains(&retry_after), "{retry_after}");
    denied(refused, ErrorCode::RateLimited).await;
    // Another webhook has its own allowance.
    app.deliver(&calm, "unaffected").await;
    assert_eq!(bot_messages(app, hook.webhook.bot_user_id).await.len(), 3);

    // A token comes back, one at a time.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = app.deliver_raw(&hook.url, "refilled", &[]).await;
        if response.status().as_u16() == 200 {
            break;
        }
        assert_eq!(response.status().as_u16(), 429);
        assert!(Instant::now() < deadline, "no token came back");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let again = app.deliver_raw(&hook.url, "no second token yet", &[]).await;
    assert_eq!(again.status().as_u16(), 429);
    assert_eq!(bot_messages(app, hook.webhook.bot_user_id).await.len(), 4);
    // Guessing at an address uses nobody's allowance: wrong secrets are 404 and never 429.
    for _ in 0..10 {
        let wrong = format!(
            "{}/hooks/{}/{}",
            app.server.base_url,
            hook.webhook.id,
            "1".repeat(64)
        );
        denied(app.deliver_raw(&wrong, "x", &[]).await, ErrorCode::NotFound).await;
    }
    finish(c).await;
}

#[sqlx::test]
async fn forty_five_deliveries_at_once_get_the_burst_and_no_more(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Flood").await;
    let cursor = app.head().await;

    let responses = join_all((0..45).map(|n| {
        let (app, url) = (&c.app, hook.url.clone());
        async move { app.deliver_raw(&url, &format!("flood {n}"), &[]).await }
    }))
    .await;
    let (mut posted, mut refused) = (0usize, 0usize);
    for response in responses {
        match response.status().as_u16() {
            200 => posted += 1,
            429 => {
                refused += 1;
                assert!(response.headers().get("retry-after").is_some());
            }
            other => panic!("unexpected status {other}"),
        }
    }
    // The burst is 30; a token may have come back while the flood was being served.
    assert!((30..=32).contains(&posted), "posted {posted}");
    assert_eq!(posted + refused, 45);

    // What was posted is all there, each once, in an order that is the log's, with no hole.
    let ours = bot_messages(app, hook.webhook.bot_user_id).await;
    assert_eq!(ours.len(), posted);
    assert!(ours.windows(2).all(|pair| pair[0].1 < pair[1].1));
    let positions = app.log_positions_after(cursor).await;
    assert!(
        positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "gapless"
    );
    assert_eq!(positions.len(), posted);
    let page = app
        .history_ok(&c.alice, app.general, &[("limit", "100".into())])
        .await;
    let texts: std::collections::HashSet<String> = page
        .messages
        .iter()
        .map(|m| m.document.plain_text())
        .collect();
    assert_eq!(texts.len(), posted, "no message twice");
    finish(c).await;
}

#[sqlx::test]
async fn a_revocation_racing_deliveries_leaves_every_delivery_before_it_and_none_after(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..10 {
        let hook = app
            .create_webhook(&c.boss, app.general, &format!("Racer {round}"))
            .await;
        let bot = hook.webhook.bot_user_id;
        let cursor = app.head().await;
        let (revoked, responses) = tokio::join!(
            async { app.delete_webhook_raw(&c.boss, hook.webhook.id).await },
            join_all((0..20).map(|n| {
                let (app, url) = (&c.app, hook.url.clone());
                async move { app.deliver_raw(&url, &format!("race {n}"), &[]).await }
            }))
        );
        assert_eq!(revoked.status().as_u16(), 204);
        let mut landed = 0usize;
        for response in responses {
            match response.status().as_u16() {
                200 => landed += 1,
                404 => {}
                other => panic!("round {round}: unexpected status {other}"),
            }
        }
        // Everything acknowledged is stored, and every one of them is before the bot's removal.
        let ours = bot_messages(app, bot).await;
        assert_eq!(ours.len(), landed, "round {round}");
        let removed_at: i64 = sqlx::query_scalar(
            "SELECT seq FROM events WHERE workspace_id = $1 AND kind = 'member_removed' \
             AND payload ->> 'user_id' = $2",
        )
        .bind(app.workspace_id.as_uuid())
        .bind(bot.to_string())
        .fetch_one(&app.pool)
        .await
        .unwrap();
        assert!(
            ours.iter().all(|(_, seq)| *seq < removed_at),
            "round {round}"
        );
        let positions = app.log_positions_after(cursor).await;
        assert!(positions.windows(2).all(|pair| pair[1] == pair[0] + 1));
        denied(
            app.deliver_raw(&hook.url, "after", &[]).await,
            ErrorCode::NotFound,
        )
        .await;
    }
    finish(c).await;
}

// --- The bot ---------------------------------------------------------------------------------

#[sqlx::test]
async fn a_bot_cannot_sign_in_and_nobody_can_become_one_by_registering(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let username = {
        let boot = app.bootstrap_ok(&c.alice).await;
        boot.workspaces[0]
            .members
            .iter()
            .find(|u| u.id == hook.webhook.bot_user_id)
            .unwrap()
            .username
            .as_str()
            .to_owned()
    };

    // Signing in as the bot fails exactly as a wrong password does, whatever the password is.
    let (wrong_status, wrong) = api_error(app.try_login("alice", "not the password").await).await;
    for password in [
        "!",
        "",
        "correct horse battery",
        "not the password",
        "$argon2id$v=19$m=8,t=1,p=1$c2FsdA$aGFzaA",
    ] {
        if password.is_empty() {
            continue;
        }
        let (status, error) = api_error(app.try_login(&username, password).await).await;
        assert_eq!((status, &error), (wrong_status, &wrong));
    }
    let (status, unknown) = api_error(app.try_login("nobody", "not the password").await).await;
    assert_eq!((status, &unknown), (wrong_status, &wrong));
    // No session can ever name it: the database refuses, even to a program that tries.
    let attempt = sqlx::query(
        "INSERT INTO sessions (id, user_id, token_hash, expires_at) VALUES ($1, $2, $3, now() + interval '1 day')",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(hook.webhook.bot_user_id.as_uuid())
    .bind(vec![7u8; 32])
    .execute(&app.pool)
    .await;
    assert!(attempt.is_err());
    let sessions: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE user_id = $1")
        .bind(hook.webhook.bot_user_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(sessions, 0);
    // Nor can a person be made into one, or a bot given a password, by accident.
    let turned = sqlx::query("UPDATE users SET is_bot = true WHERE id = $1")
        .bind(c.alice.user.id.as_uuid())
        .execute(&app.pool)
        .await;
    assert!(turned.is_err());
    let given = sqlx::query("UPDATE users SET password_hash = 'x' WHERE id = $1")
        .bind(hook.webhook.bot_user_id.as_uuid())
        .execute(&app.pool)
        .await;
    assert!(given.is_err());

    // A person cannot register under the name a bot has, or one shaped like it.
    for name in [username.as_str(), "bot-0123abcd", "bot-ffffffff"] {
        let response = app.try_register(name, PASSWORD).await;
        denied(response, ErrorCode::InvalidRequest).await;
    }
    assert!(
        app.try_register("bot-abc", PASSWORD)
            .await
            .status()
            .is_success()
    );
    assert!(
        app.try_register("robot-0123abcd", PASSWORD)
            .await
            .status()
            .is_success()
    );
    // People are not bots on the wire; the field is there only when it is true.
    let session = app.try_login("alice", PASSWORD).await;
    let raw = session.text().await.unwrap();
    assert!(!raw.contains("is_bot"));
    finish(c).await;
}

#[sqlx::test]
async fn a_bot_is_never_offered_as_a_person_to_message_mention_group_or_add(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let bot_id = hook.webhook.bot_user_id;
    let bot = Account {
        token: String::new(),
        user: app.bootstrap_ok(&c.alice).await.workspaces[0]
            .members
            .iter()
            .find(|u| u.id == bot_id)
            .unwrap()
            .clone(),
    };
    assert!(bot.user.is_bot);

    // No conversation with it, direct or in a group.
    let response = app
        .open_raw(&c.alice, &[&bot], lazy_domain::OperationId::generate())
        .await;
    denied(response, ErrorCode::InvalidRequest).await;
    let response = app
        .open_raw(
            &c.alice,
            &[&bot, &c.bob],
            lazy_domain::OperationId::generate(),
        )
        .await;
    denied(response, ErrorCode::InvalidRequest).await;
    // Which is not the same as the refusal for a body that is wrong: people are fine.
    assert!(
        app.open_raw(&c.alice, &[&c.bob], lazy_domain::OperationId::generate())
            .await
            .status()
            .is_success()
    );
    // No adding it to another channel.
    let ops = app.create_channel(&c.boss, "ops", "public").await;
    denied(
        app.add_members(&c.boss, ops.id, &[&bot]).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // No group containing it.
    let response = app
        .create_group_raw(
            &c.boss,
            "team",
            "Team",
            &[&bot, &c.alice],
            lazy_domain::OperationId::generate(),
        )
        .await;
    denied(response, ErrorCode::InvalidRequest).await;
    // No mention of it.
    let response = app
        .send_doc_raw(
            &c.alice,
            app.general,
            App::mention_document(
                "@bot hello",
                &[(
                    "bot",
                    lazy_domain::mention::MentionTarget::User { id: bot_id },
                )],
            ),
        )
        .await;
    denied(response, ErrorCode::InvalidRequest).await;
    // Nor can it be removed by hand: the webhook is what keeps it there. (Tried in a channel where
    // an admin may remove anyone; the default channel would refuse anyone for its own reasons.)
    let ops_hook = app.create_webhook(&c.boss, ops.id, "Ops").await;
    let ops_bot = Account {
        token: String::new(),
        user: app.bootstrap_ok(&c.alice).await.workspaces[0]
            .members
            .iter()
            .find(|u| u.id == ops_hook.webhook.bot_user_id)
            .unwrap()
            .clone(),
    };
    assert!(
        app.add_members(&c.boss, ops.id, &[&c.carol])
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        app.remove_member(&c.boss, ops.id, &c.carol)
            .await
            .status()
            .as_u16(),
        204
    );
    denied(
        app.remove_member(&c.boss, ops.id, &ops_bot).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert!(
        members_of(app, &c.boss, ops.id)
            .await
            .contains(&ops_bot.user.id)
    );
    assert!(
        members_of(app, &c.carol, app.general)
            .await
            .contains(&bot_id)
    );
    finish(c).await;
}

#[sqlx::test]
async fn what_a_bot_says_is_moderated_reacted_to_and_answered_like_anything_else(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let id = app.deliver(&hook, "v1.2 is out").await;
    let message = app.get_message(&c.alice, id).await;

    // Nobody can edit it: not a member, not an admin, not the owner (the bot is its author, and
    // cannot sign in).
    for who in [&c.alice, &c.boss, &c.owner] {
        let response = app
            .edit_raw(who, id, lazy_domain::OperationId::generate(), 1, "changed")
            .await;
        denied(response, ErrorCode::Forbidden).await;
    }
    assert_eq!(app.get_message(&c.alice, id).await, message);
    // Anyone who can post can react, and reply in a thread under it.
    let reacted = app.react(&c.bob, id, "👍", true).await;
    assert_eq!(reacted.reactions.len(), 1);
    let reply = app.reply(&c.carol, &message, "thanks").await;
    assert_eq!(reply.thread_root, Some(id));
    let root = app.get_message(&c.alice, id).await;
    assert_eq!(root.thread.unwrap().reply_count, 1);
    // A member cannot delete it; a moderator can, and then it is a tombstone attributed to the bot.
    denied(app.delete_raw(&c.alice, id).await, ErrorCode::Forbidden).await;
    let tombstone = app.delete(&c.boss, id).await;
    assert!(tombstone.deleted && tombstone.author_id == hook.webhook.bot_user_id);
    // Unread counts follow the ordinary rules around it.
    let second = app.deliver(&hook, "v1.3 is out").await;
    assert_eq!(app.state_of(&c.alice, app.general).await.unread_count, 1);
    app.delete(&c.owner, second).await;
    assert_eq!(app.state_of(&c.alice, app.general).await.unread_count, 0);
    finish(c).await;
}

// --- Replay -----------------------------------------------------------------------------------

#[sqlx::test]
async fn a_subscriber_who_was_away_sees_the_bot_arrive_and_then_what_it_said(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let first = app.deliver(&hook, "one").await;
    let second = app.deliver(&hook, "two").await;
    app.delete_webhook_raw(&c.boss, hook.webhook.id).await;

    let events = replay_from(app, &c.carol, cursor).await;
    let shape: Vec<String> = events
        .iter()
        .map(|e| match &e.event {
            Event::MemberJoined { user } => format!("joined:{}", user.is_bot),
            Event::MemberAdded { .. } => "added".into(),
            Event::MessageCreated { message, .. } => {
                format!("message:{}", message.document.plain_text())
            }
            Event::MemberRemoved { .. } => "removed".into(),
            Event::ConversationUpdated { .. } => "updated".into(),
            _ => "other".into(),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "joined:true",
            "added",
            "message:one",
            "message:two",
            "removed",
            "updated"
        ]
    );
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    let authors: Vec<UserId> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. } => Some(message.author_id),
            _ => None,
        })
        .collect();
    assert_eq!(authors, vec![hook.webhook.bot_user_id; 2]);
    assert!(first != second);
    finish(c).await;
}

// --- What is logged ---------------------------------------------------------------------------

static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct CaptureWriter;

impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Capture;

impl<'a> MakeWriter<'a> for Capture {
    type Writer = CaptureWriter;

    fn make_writer(&'a self) -> CaptureWriter {
        CaptureWriter
    }
}

/// Sends everything the server logs (as much as it will say) into [`LOGS`], for this whole test
/// binary. The client's own logging is left out: it would show the address it was given.
fn capture_logs() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(Capture)
            .with_ansi(false)
            .with_env_filter(EnvFilter::new(
                "info,lazy_server=trace,tower_http=trace,axum=trace,sqlx=debug",
            ))
            .try_init();
    });
}

#[sqlx::test]
async fn the_secret_in_an_address_is_never_logged_or_echoed(pool: PgPool) {
    capture_logs();
    let c = chat(pool).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let secret = secret_of(&hook);
    let guess = "feedface".repeat(8);
    let mut answers = Vec::new();

    // A delivery that works, and every way one can fail.
    answers.push(
        app.deliver_raw(&hook.url, "hello", &[])
            .await
            .text()
            .await
            .unwrap(),
    );
    answers.push(
        app.deliver_raw(&hook.url, "", &[])
            .await
            .text()
            .await
            .unwrap(),
    );
    answers.push(
        app.deliver_raw(
            &hook.url,
            &"z".repeat(lazy_domain::MAX_WEBHOOK_BODY_BYTES),
            &[],
        )
        .await
        .text()
        .await
        .unwrap(),
    );
    answers.push(
        app.deliver_raw(&hook.url, "hello", &[("Idempotency-Key", &"k".repeat(65))])
            .await
            .text()
            .await
            .unwrap(),
    );
    let wrong = format!("{}/hooks/{}/{guess}", app.server.base_url, hook.webhook.id);
    answers.push(
        app.deliver_raw(&wrong, "hello", &[])
            .await
            .text()
            .await
            .unwrap(),
    );
    let unknown = format!(
        "{}/hooks/{}/{guess}?token={secret}",
        app.server.base_url,
        WebhookId::generate()
    );
    answers.push(
        app.deliver_raw(&unknown, "hello", &[])
            .await
            .text()
            .await
            .unwrap(),
    );
    let extra = format!("{}/extra/{guess}", hook.url);
    answers.push(
        app.deliver_raw(&extra, "hello", &[])
            .await
            .text()
            .await
            .unwrap(),
    );
    app.delete_webhook_raw(&c.boss, hook.webhook.id).await;
    answers.push(
        app.deliver_raw(&hook.url, "hello", &[])
            .await
            .text()
            .await
            .unwrap(),
    );

    // No answer carries either secret...
    for answer in &answers {
        assert!(
            !answer.contains(&secret) && !answer.contains(&guess),
            "{answer}"
        );
    }
    // ...and neither does the log, which did see the requests (it shows them by their route
    // pattern, which has no secret in it: the path never reaches the log).
    let logs = String::from_utf8_lossy(&LOGS.lock().unwrap()).into_owned();
    assert!(!logs.contains(&secret), "the secret was logged");
    assert!(!logs.contains(&guess), "a guessed secret was logged");
    assert!(
        logs.contains("route=/hooks/{webhook_id}/{secret}"),
        "the capture saw nothing of the requests"
    );
    assert!(
        !logs.contains(&hook.webhook.id.to_string()),
        "the webhook's id, which is in its address, was logged"
    );
    finish(c).await;
}

// --- Version ----------------------------------------------------------------------------------

#[sqlx::test]
async fn the_server_reports_a_protocol_version_that_has_webhooks(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    const { assert!(PROTOCOL_VERSION >= 7) };
    let health: lazy_protocol::HealthResponse = app
        .http
        .get(app.url("/healthz"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        app.bootstrap_ok(&c.alice).await.protocol_version,
        PROTOCOL_VERSION
    );
    finish(c).await;
}

/// Signing in as a bot does the work of checking a password, like signing in as a person with a
/// wrong one or as someone who does not exist, so it cannot be told apart by how long it takes.
/// (With the cheap parameters the other tests use the work is too small to see; this server hashes
/// at the real cost.)
#[sqlx::test]
async fn signing_in_as_a_bot_costs_as_much_as_a_wrong_password(pool: PgPool) {
    use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions};
    let app = App::start_with(
        pool,
        ServerOptions {
            open_registration: true,
            open_workspace_creation: true,
            password_cost: PasswordCost::DEFAULT,
            retention: RetentionPolicy::disabled(),
        },
    )
    .await;
    let c = chat_with(app).await;
    let app = &c.app;
    let hook = app.create_webhook(&c.boss, app.general, "Deploys").await;
    let bot = app.bootstrap_ok(&c.alice).await.workspaces[0]
        .members
        .iter()
        .find(|u| u.id == hook.webhook.bot_user_id)
        .unwrap()
        .username
        .as_str()
        .to_owned();

    async fn fastest(app: &App, username: &str) -> Duration {
        let mut best = Duration::MAX;
        for _ in 0..4 {
            let start = Instant::now();
            let response = app.try_login(username, "not the password").await;
            assert_eq!(response.status().as_u16(), 401);
            best = best.min(start.elapsed());
        }
        best
    }
    let person = fastest(app, "alice").await;
    let nobody = fastest(app, "nobody").await;
    let robot = fastest(app, &bot).await;
    // Hashing at this cost takes tens of milliseconds; skipping it takes about one.
    assert!(person > Duration::from_millis(5), "{person:?}");
    assert!(
        robot * 3 > person,
        "bot {robot:?} against person {person:?}"
    );
    assert!(
        nobody * 3 > person,
        "nobody {nobody:?} against person {person:?}"
    );
    finish(c).await;
}
