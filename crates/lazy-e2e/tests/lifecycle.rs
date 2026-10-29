//! Phase 2B end to end: editing, deleting, rich text, and permalinks across real clients, a real
//! server, and real Postgres.

mod support;

use lazy_client_core::{ActionKind, ActionState, ClientCore, EditState, Input, ItemKey};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{
    Block, ChannelVisibility, ConversationId, Inline, MessageDocument, MessageId, Permalink,
};
use sqlx::PgPool;
use support::{Client, World};

/// The conversation and message id of the confirmed row with this text, in the selected
/// conversation.
fn row(client: &Client, text: &str) -> (ConversationId, MessageId) {
    client.read(|core| {
        let conversation = core.selected_conversation().expect("a conversation");
        let id = core
            .timeline(conversation)
            .iter()
            .find(|item| item.document.plain_text() == text)
            .and_then(|item| item.message_id())
            .unwrap_or_else(|| panic!("no confirmed row {text:?}"));
        (conversation, id)
    })
}

fn find_row(core: &ClientCore, id: MessageId) -> Option<lazy_client_core::TimelineItem> {
    core.selected_conversation().and_then(|conversation| {
        core.timeline(conversation)
            .iter()
            .find(|item| item.message_id() == Some(id))
            .cloned()
    })
}

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

async fn revision_rows(world: &World, id: MessageId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM message_revisions WHERE message_id = $1")
        .bind(id.as_uuid())
        .fetch_one(&world.pool)
        .await
        .expect("count revisions")
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn formatted_messages_arrive_as_structured_documents(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    alice.send("*bold* and `code` and [the docs](https://example.com/docs)");
    alice.send("```rust\nfn main() {}\n```");
    alice.send("> quoted\n\n- one\n- two");
    bob.wait_for_text("bold and code and the docs (https://example.com/docs)")
        .await;
    bob.wait_for_text("fn main() {}").await;
    bob.wait_for_text("> quoted\n\n- one\n- two").await;

    let items = bob.items();
    let first = items[0].document.blocks();
    assert!(matches!(
        &first[0],
        Block::Paragraph { inlines } if inlines.iter().any(|i| matches!(
            i, Inline::Text { text, marks } if text == "bold" && marks.bold))
            && inlines.iter().any(|i| matches!(
                i, Inline::Text { text, marks } if text == "code" && marks.code))
            && inlines.iter().any(|i| matches!(
                i, Inline::Link { url, .. } if url == "https://example.com/docs"))
    ));
    assert!(matches!(
        &items[1].document.blocks()[0],
        Block::CodeBlock { language: Some(language), text } if language == "rust" && text == "fn main() {}"
    ));
    assert!(matches!(
        &items[2].document.blocks()[0],
        Block::Quote { .. }
    ));
    assert!(
        matches!(&items[2].document.blocks()[1], Block::BulletList { items } if items.len() == 2)
    );

    // What was sent is what the editor offers back, formatting included.
    let (conversation, id) = row(
        &bob,
        "bold and code and the docs (https://example.com/docs)",
    );
    assert_eq!(
        alice.read(|core| core.edit_text(conversation, id)),
        Some("*bold* and `code` and [the docs](https://example.com/docs)".to_owned())
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn an_edit_reaches_everyone_and_the_server_keeps_the_old_revision(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    alice.send("first draft");
    alice.send("second message");
    alice.wait_confirmed("second message").await;
    bob.wait_for_text("second message").await;
    let (conversation, id) = row(&alice, "first draft");
    assert!(alice.read(|core| core.can_edit_message(conversation, id)));
    assert!(
        bob.read(|core| !core.can_edit_message(conversation, id)),
        "only the author edits"
    );

    alice.act(Input::EditMessage {
        conversation,
        message: id,
        text: "*final* draft".to_owned(),
    });
    for (client, who) in [(&alice, "the author"), (&bob, "a reader")] {
        client
            .wait_until(&format!("the edit to reach {who}"), move |core| {
                find_row(core, id).is_some_and(|item| {
                    item.document.plain_text() == "final draft"
                        && item.revision() == Some(2)
                        && item.is_edited()
                        && item.edit == EditState::None
                })
            })
            .await;
    }

    // The message did not move, and nothing was duplicated.
    assert_eq!(alice.texts(), ["final draft", "second message"]);
    assert_eq!(bob.texts(), ["final draft", "second message"]);
    let (revision, edited): (i32, bool) =
        sqlx::query_as("SELECT revision, edited_at IS NOT NULL FROM messages WHERE id = $1")
            .bind(id.as_uuid())
            .fetch_one(&world.pool)
            .await
            .unwrap();
    assert_eq!((revision, edited), (2, true));
    assert_eq!(
        revision_rows(&world, id).await,
        1,
        "the original text is kept on the server"
    );

    // An edit that changes nothing commits nothing.
    alice.act(Input::EditMessage {
        conversation,
        message: id,
        text: "*final* draft".to_owned(),
    });
    alice
        .wait_until("the no-op edit to settle", move |core| {
            find_row(core, id).is_some_and(|item| item.edit == EditState::None)
        })
        .await;
    let revision: i32 = sqlx::query_scalar("SELECT revision FROM messages WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(revision, 2);

    // Someone who arrives later reads the edited version from history.
    let carol = start(&world, dir.path(), "carol");
    carol.register("carol").await;
    assert_eq!(carol.texts(), ["final draft", "second message"]);
    assert!(carol.items()[0].is_edited());

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_stale_edit_keeps_the_persons_text_and_can_replace_the_newer_change(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;
    alice.send("original");
    alice.wait_confirmed("original").await;
    let (conversation, id) = row(&alice, "original");

    // Another device of the same account edited the message and this client never heard about it
    // (its connection lagged). Written straight to the database, with no event.
    let theirs =
        serde_json::to_value(MessageDocument::from_plain_text("their change").unwrap()).unwrap();
    sqlx::query(
        "UPDATE messages SET document = $2, plain_text = 'their change', revision = 2, \
         edited_at = now() WHERE id = $1",
    )
    .bind(id.as_uuid())
    .bind(theirs)
    .execute(&world.pool)
    .await
    .unwrap();

    alice.act(Input::EditMessage {
        conversation,
        message: id,
        text: "my version".to_owned(),
    });
    alice
        .wait_until("the conflict", move |core| {
            find_row(core, id).is_some_and(
                |item| matches!(&item.edit, EditState::Conflict { text } if text == "my version"),
            )
        })
        .await;
    // The row shows the newer version, and the person's text is still there to offer.
    let item = alice.read(|core| find_row(core, id)).unwrap();
    assert_eq!(item.document.plain_text(), "their change");
    assert_eq!(
        alice.read(|core| core.edit_text(conversation, id)),
        Some("my version".to_owned())
    );
    let plain: String = sqlx::query_scalar("SELECT plain_text FROM messages WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(
        plain, "their change",
        "a stale edit changes nothing on the server"
    );

    // The person chooses to keep their version: it goes on top of the newer one.
    alice.act(Input::RetryEdit {
        conversation,
        message: id,
    });
    alice
        .wait_until("the replacement to save", move |core| {
            find_row(core, id).is_some_and(|item| {
                item.edit == EditState::None
                    && item.document.plain_text() == "my version"
                    && item.revision() == Some(3)
            })
        })
        .await;
    let revision: i32 = sqlx::query_scalar("SELECT revision FROM messages WHERE id = $1")
        .bind(id.as_uuid())
        .fetch_one(&world.pool)
        .await
        .unwrap();
    assert_eq!(revision, 3);

    alice.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn deleting_removes_a_message_everywhere_and_leaves_no_text_behind(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    // Bob reads another channel, so what Alice posts in #general counts as unread for him.
    alice.act(Input::CreateChannel {
        name: "design".to_owned(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    alice
        .wait_until("the new channel", |core| {
            core.conversations()
                .iter()
                .any(|c| c.name.is_some() && !c.is_default)
        })
        .await;
    let general = alice.read(|core| core.selected_conversation()).unwrap();
    let design = alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| !c.is_default)
                .map(|c| c.id)
        })
        .unwrap();
    alice.act(Input::SelectConversation(general));
    bob.act(Input::BrowseChannels);
    bob.wait_until("the directory", move |core| {
        core.browse()
            .channels
            .iter()
            .any(|b| b.conversation.id == design)
    })
    .await;
    bob.act(Input::JoinChannel(design));
    bob.wait_until("the join", move |core| core.conversation(design).is_some())
        .await;
    bob.act(Input::SelectConversation(design));
    bob.wait_until("design selected", move |core| {
        core.selected_conversation() == Some(design)
    })
    .await;

    alice.send("to be deleted");
    alice.wait_confirmed("to be deleted").await;
    let (_, doomed) = row(&alice, "to be deleted");
    bob.wait_until("one unread in #general", move |core| {
        core.user_state(general)
            .is_some_and(|s| s.unread_count == 1)
    })
    .await;

    // An edit first, so the server holds three versions of the text.
    alice.act(Input::EditMessage {
        conversation: general,
        message: doomed,
        text: "to be erased".to_owned(),
    });
    alice
        .wait_until("the edit", move |core| {
            find_row(core, doomed).is_some_and(|item| item.revision() == Some(2))
        })
        .await;
    assert_eq!(revision_rows(&world, doomed).await, 1);

    // Bob may not delete Alice's message; Alice may.
    assert!(bob.read(|core| !core.can_delete_message(general, doomed)));
    assert!(alice.read(|core| core.can_delete_message(general, doomed)));
    alice.act(Input::DeleteMessage {
        conversation: general,
        message: doomed,
    });
    alice
        .wait_until("the message to leave Alice's screen", move |core| {
            find_row(core, doomed).is_none()
                && *core.action(ActionKind::DeleteMessage(doomed)) == ActionState::Idle
        })
        .await;
    bob.wait_until("the unread count to drop", move |core| {
        core.user_state(general)
            .is_some_and(|s| s.unread_count == 0)
    })
    .await;
    bob.act(Input::SelectConversation(general));
    bob.wait_until("#general", move |core| {
        core.selected_conversation() == Some(general)
    })
    .await;
    bob.wait_until("history", move |core| {
        matches!(
            core.history_status(general),
            lazy_client_core::HistoryStatus::Ready { .. }
        )
    })
    .await;
    assert!(bob.texts().is_empty(), "{:?}", bob.texts());

    // Nothing of either text remains anywhere on the server.
    let (deleted, plain, revisions): (bool, String, i64) = sqlx::query_as(
        "SELECT deleted, plain_text, \
         (SELECT count(*) FROM message_revisions WHERE message_id = $1) \
         FROM messages WHERE id = $1",
    )
    .bind(doomed.as_uuid())
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(deleted);
    assert_eq!(plain, "");
    assert_eq!(revisions, 0);
    for needle in ["to be deleted", "to be erased"] {
        let leaks: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM events WHERE payload::text LIKE '%' || $1 || '%') \
                  + (SELECT count(*) FROM messages WHERE document::text LIKE '%' || $1 || '%') \
                  + (SELECT count(*) FROM message_revisions WHERE document::text LIKE '%' || $1 || '%')",
        )
        .bind(needle)
        .fetch_one(&world.pool)
        .await
        .unwrap();
        assert_eq!(leaks, 0, "{needle:?} is still stored");
    }

    // The workspace owner can remove someone else's message.
    bob.send("bob's message");
    bob.wait_confirmed("bob's message").await;
    alice.wait_for_text("bob's message").await;
    let (_, theirs) = row(&alice, "bob's message");
    assert!(alice.read(|core| core.can_delete_message(general, theirs)));
    alice.act(Input::DeleteMessage {
        conversation: general,
        message: theirs,
    });
    bob.wait_until("moderation to reach the author", move |core| {
        find_row(core, theirs).is_none()
    })
    .await;

    // Someone who arrives later finds neither message.
    let carol = start(&world, dir.path(), "carol");
    carol.register("carol").await;
    assert!(carol.texts().is_empty(), "{:?}", carol.texts());

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn edits_and_deletions_made_while_a_client_was_away_arrive_by_replay(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    let bob_profile = dir.path().join("bob");
    let bob_secrets = MemoryStorage::new();
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &bob_profile,
        bob_secrets.clone(),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    for text in ["one", "two", "three"] {
        alice.send(text);
        alice.wait_confirmed(text).await;
    }
    bob.wait_for_text("three").await;
    let cached_cursor = bob.last_cached_cursor().expect("bob saved his cache");
    bob.stop().await;

    let (conversation, two) = row(&alice, "two");
    let (_, three) = row(&alice, "three");
    alice.act(Input::EditMessage {
        conversation,
        message: two,
        text: "two (fixed)".to_owned(),
    });
    alice
        .wait_until("the edit", move |core| {
            find_row(core, two).is_some_and(|item| item.revision() == Some(2))
        })
        .await;
    alice.act(Input::DeleteMessage {
        conversation,
        message: three,
    });
    alice
        .wait_until("the deletion", move |core| find_row(core, three).is_none())
        .await;

    // Bob comes back: his cache still shows the old messages, and replay corrects them. No
    // snapshot is needed, and no history is downloaded again.
    let bob = Client::start("bob", &world.server.base_url, &bob_profile, bob_secrets);
    bob.wait_until_ready().await;
    bob.wait_until("replay of the edit and the deletion", |core| {
        core.selected_conversation().is_some_and(|c| {
            core.timeline(c)
                .iter()
                .map(|i| i.document.plain_text())
                .eq(["one".to_owned(), "two (fixed)".to_owned()])
        })
    })
    .await;
    assert_eq!(
        bob.subscribe_positions(),
        vec![cached_cursor],
        "resumed from the cached cursor"
    );
    assert_eq!(bob.history_fetches(), 0);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_permalink_opens_an_old_message_and_respects_access(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;

    // One message, then more than a page of newer ones, so it is out of view for a newcomer.
    alice.send("the needle");
    alice.wait_confirmed("the needle").await;
    let (general, needle) = row(&alice, "the needle");
    for n in 0..60 {
        alice.send(&format!("filler {n}"));
    }
    alice.wait_confirmed("filler 59").await;
    let workspace = alice.read(|core| core.primary_workspace().unwrap().workspace.id);
    let link = Permalink {
        workspace_id: workspace,
        conversation_id: general,
        message_id: needle,
    };

    let bob = start(&world, dir.path(), "bob");
    bob.register("bob").await;
    assert!(
        bob.read(|core| find_row(core, needle)).is_none(),
        "the newest page does not reach the needle"
    );
    let fetches = bob.history_fetches();
    bob.act(Input::OpenPermalink(link));
    bob.wait_until("the jump target", move |core| {
        core.jump_target()
            .is_some_and(|t| t.conversation == general && t.key == ItemKey::Message(needle))
    })
    .await;
    assert!(bob.read(|core| find_row(core, needle)).is_some());
    assert!(bob.history_fetches() > fetches, "older history was loaded");
    bob.act(Input::ClearJumpTarget);
    assert!(bob.read(|core| core.jump_target()).is_none());

    // A message in a private channel Bob is not in is not revealed.
    alice.act(Input::CreateChannel {
        name: "secret".to_owned(),
        visibility: ChannelVisibility::Private,
        topic: String::new(),
        description: String::new(),
    });
    alice
        .wait_until("the private channel", |core| {
            core.conversations()
                .iter()
                .any(|c| c.name.as_ref().is_some_and(|n| n.as_str() == "secret"))
        })
        .await;
    let secret = alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == "secret"))
                .map(|c| c.id)
        })
        .unwrap();
    alice
        .wait_until("secret selected", move |core| {
            core.selected_conversation() == Some(secret)
        })
        .await;
    alice
        .wait_until("secret history", move |core| {
            matches!(
                core.history_status(secret),
                lazy_client_core::HistoryStatus::Ready { .. }
            )
        })
        .await;
    alice.send("members only");
    alice.wait_confirmed("members only").await;
    let (_, hidden) = row(&alice, "members only");
    let selected = bob.read(|core| core.selected_conversation());
    bob.act(Input::OpenPermalink(Permalink {
        workspace_id: workspace,
        conversation_id: secret,
        message_id: hidden,
    }));
    bob.wait_until("a notice about the link", |core| core.notice().is_some())
        .await;
    assert!(bob.read(|core| core.jump_target()).is_none());
    assert_eq!(bob.read(|core| core.selected_conversation()), selected);
    assert!(
        bob.read(
            |core| core.notice().unwrap().to_lowercase().contains("secret")
                || !core.notice().unwrap().contains("members only")
        ),
        "the notice does not reveal the message"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
