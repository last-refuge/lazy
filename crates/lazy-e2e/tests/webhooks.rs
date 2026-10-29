//! Phase 2E end to end: incoming webhooks and bot identity across real clients, a real server, and
//! real Postgres. The posting side is a plain HTTP client, as an outside service would be.

mod support;

use lazy_client_core::{ActionKind, ActionState, ClientCore, Input};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ConversationId, UserId};
use sqlx::PgPool;
use support::{Client, World};

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

fn general(client: &Client) -> ConversationId {
    client.read(|core| {
        core.conversations()
            .iter()
            .find(|c| c.is_default)
            .map(|c| c.id)
            .expect("the default channel")
    })
}

fn row_author(core: &ClientCore, conversation: ConversationId, text: &str) -> Option<UserId> {
    core.timeline(conversation)
        .iter()
        .find(|i| i.document.plain_text() == text)
        .map(|i| i.author_id)
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    text: &str,
    key: Option<&str>,
) -> reqwest::Response {
    let mut request = client.post(url).json(&serde_json::json!({ "text": text }));
    if let Some(key) = key {
        request = request.header("Idempotency-Key", key);
    }
    request.send().await.expect("the webhook request")
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_service_posts_as_a_bot_through_a_webhook_an_admin_made(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    // A member cannot see or make webhooks.
    assert!(bob.read(|core| !core.can_manage_webhooks(general)));
    bob.act(Input::CreateWebhook {
        conversation: general,
        name: "Sneaky".to_owned(),
    });
    bob.wait_until("a refusal", move |core| {
        matches!(
            core.action(ActionKind::CreateWebhook(general)),
            ActionState::Failed(_)
        )
    })
    .await;

    // The owner makes one and is shown its address once.
    alice.act(Input::CreateWebhook {
        conversation: general,
        name: "Deploy Bot".to_owned(),
    });
    alice
        .wait_until("the address", |core| core.created_webhook().is_some())
        .await;
    let created = alice.read(|core| core.created_webhook().cloned().unwrap());
    let url = created.url.as_str().to_owned();
    assert!(
        url.starts_with(&format!("{}/hooks/", world.server.base_url)),
        "{url}"
    );
    // The client never prints the address.
    assert!(!format!("{created:?}").contains(url.rsplit('/').next().unwrap()));
    let bot = created.webhook.bot_user_id;
    alice.act(Input::DismissCreatedWebhook);
    assert!(alice.read(|core| core.created_webhook().is_none()));
    // The list, as the dialog loads it, has the webhook and no secret.
    alice.act(Input::LoadWebhooks(general));
    alice
        .wait_until("the webhook in the list", move |core| {
            core.webhooks(general).len() == 1
        })
        .await;
    assert_eq!(
        alice.read(|core| core.webhooks(general)[0].name.to_string()),
        "Deploy Bot"
    );

    // An outside service posts. Formatting and emoji work; a name stays text and notifies no one.
    let http = reqwest::Client::new();
    let response = post(&http, &url, "*deploy* finished :tada: @bob", None).await;
    assert_eq!(response.status(), 200, "{:?}", response.text().await);
    for client in [&alice, &bob] {
        client
            .wait_until("the bot's message", move |core| {
                row_author(core, general, "deploy finished 🎉 @bob") == Some(bot)
            })
            .await;
    }
    assert!(bob.read(|core| core.is_bot(bot)));
    assert_eq!(
        bob.read(|core| core.display_name(bot).to_owned()),
        "Deploy Bot"
    );
    assert_eq!(
        bob.read(|core| core.user_state(general).unwrap().mention_count),
        0
    );

    // Nobody can edit it; an admin can delete it; the bot is not offered as a person to mention.
    let id = bob.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.author_id == bot)
            .and_then(|i| i.message_id())
            .unwrap()
    });
    assert!(bob.read(|core| !core.can_edit_message(general, id)));
    assert!(alice.read(|core| !core.can_edit_message(general, id)));
    assert!(alice.read(|core| core.can_delete_message(general, id)));
    alice
        .wait_until("the member list", move |core| {
            core.conversation_members(general).is_some()
        })
        .await;
    assert!(
        alice
            .read(|core| core.mention_candidates(general, "deploy"))
            .is_empty()
    );
    assert!(
        alice
            .read(|core| core.mention_candidates(general, "bot"))
            .iter()
            .all(|c| c.display_name != "Deploy Bot")
    );

    // A retry with the same key posts once; a new key posts again.
    let first = post(&http, &url, "retry me", Some("key-1")).await;
    let again = post(&http, &url, "retry me", Some("key-1")).await;
    assert_eq!(
        (first.status().as_u16(), again.status().as_u16()),
        (200, 200)
    );
    let (a, b): (serde_json::Value, serde_json::Value) =
        (first.json().await.unwrap(), again.json().await.unwrap());
    assert_eq!(a["message_id"], b["message_id"]);
    let other = post(&http, &url, "retry me", Some("key-2")).await;
    assert_eq!(other.status(), 200);
    world.wait_for_message_count("retry me", 2).await;

    // A wrong secret is not found, whatever the id; empty text is refused.
    let wrong = url.replacen(url.rsplit('/').next().unwrap(), "not-the-secret", 1);
    assert_eq!(post(&http, &wrong, "hi", None).await.status(), 404);
    assert_eq!(post(&http, &url, "   ", None).await.status(), 400);

    // Deleting the webhook revokes it, removes the bot from the channel, and keeps its messages.
    alice.act(Input::DeleteWebhook(created.webhook.id));
    alice
        .wait_until("the webhook to go", move |core| {
            core.webhooks(general).is_empty()
        })
        .await;
    assert_eq!(post(&http, &url, "too late", None).await.status(), 404);
    alice
        .wait_until("the bot to leave the channel", move |core| {
            core.conversation_members(general)
                .is_some_and(|m| !m.contains(&bot))
        })
        .await;
    assert_eq!(world.count_messages("too late").await, 0);
    let carol = start(&world, dir.path(), "carol");
    carol.register("carol").await;
    assert_eq!(
        carol.read(|core| row_author(core, general, "deploy finished 🎉 @bob")),
        Some(bot)
    );
    assert!(carol.read(|core| core.is_bot(bot)));

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}
