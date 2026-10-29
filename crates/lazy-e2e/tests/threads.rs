//! Phase 2D end to end: threads across real clients, a real server, and real Postgres.

mod support;

use lazy_client_core::{ClientCore, Input, ItemKey, TimelineItem};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId, MessageId, Permalink};
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

fn root_of(client: &Client, conversation: ConversationId, text: &str) -> MessageId {
    client.read(|core| {
        core.timeline(conversation)
            .iter()
            .find(|i| i.document.plain_text() == text)
            .and_then(|i| i.message_id())
            .unwrap_or_else(|| panic!("no confirmed row {text:?}"))
    })
}

fn channel_row(
    core: &ClientCore,
    conversation: ConversationId,
    id: MessageId,
) -> Option<TimelineItem> {
    core.timeline(conversation)
        .iter()
        .find(|i| i.message_id() == Some(id))
        .cloned()
}

fn channel_texts(core: &ClientCore, conversation: ConversationId) -> Vec<String> {
    core.timeline(conversation)
        .iter()
        .map(|i| i.document.plain_text())
        .collect()
}

fn reply_texts(core: &ClientCore, root: MessageId) -> Vec<String> {
    core.thread(root)
        .map(|t| t.replies.iter().map(|r| r.document.plain_text()).collect())
        .unwrap_or_default()
}

fn reply_count(core: &ClientCore, conversation: ConversationId, root: MessageId) -> u32 {
    channel_row(core, conversation, root)
        .and_then(|r| r.thread)
        .map_or(0, |t| t.reply_count)
}

async fn open_thread(client: &Client, conversation: ConversationId, root: MessageId) {
    client.act(Input::OpenThread { conversation, root });
    client
        .wait_until("the thread to open", move |core| {
            core.open_thread().is_some_and(|t| t.root == root)
                && core.thread(root).is_some_and(|t| {
                    t.root.is_some()
                        && matches!(t.history, lazy_client_core::HistoryStatus::Ready { .. })
                })
        })
        .await;
}

async fn reply(
    client: &Client,
    conversation: ConversationId,
    root: MessageId,
    text: &str,
    also: bool,
) {
    client.act(Input::SendReply {
        conversation,
        root,
        text: text.to_owned(),
        also_in_channel: also,
    });
    let wanted = text.to_owned();
    client
        .wait_until("the reply to be confirmed", move |core| {
            core.thread(root).is_some_and(|t| {
                t.replies
                    .iter()
                    .any(|r| r.document.plain_text() == wanted && r.is_confirmed())
            })
        })
        .await;
}

async fn stored_thread(world: &World, root: MessageId) -> (i64, i64) {
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM messages WHERE thread_root_id = $1 AND NOT deleted), \
                (SELECT count(*) FROM messages WHERE thread_root_id = $1 AND in_channel AND NOT deleted)",
    )
    .bind(root.as_uuid())
    .fetch_one(&world.pool)
    .await
    .expect("stored thread")
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn replies_form_a_thread_that_stays_out_of_the_channel_unless_sent_there(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    alice.send("the root");
    alice.wait_confirmed("the root").await;
    bob.wait_for_text("the root").await;
    let root = root_of(&alice, general, "the root");

    // Bob replies from the thread pane; the channel timelines gain a summary and no new row.
    open_thread(&bob, general, root).await;
    reply(&bob, general, root, "first reply", false).await;
    for client in [&alice, &bob] {
        client
            .wait_until("the summary on the root", move |core| {
                reply_count(core, general, root) == 1
            })
            .await;
        assert_eq!(
            client.read(|core| channel_texts(core, general)),
            ["the root"]
        );
    }
    let summary = alice.read(|core| channel_row(core, general, root).unwrap().thread.unwrap());
    let bob_id = bob.read(|core| core.me().unwrap().id);
    assert_eq!(summary.recent_repliers, [bob_id]);

    // Alice opens the thread, sees the reply, and answers; this one is also sent to the channel.
    open_thread(&alice, general, root).await;
    assert_eq!(alice.read(|core| reply_texts(core, root)), ["first reply"]);
    reply(&alice, general, root, "second reply", true).await;
    for client in [&alice, &bob] {
        client
            .wait_until("the second reply in the channel", move |core| {
                channel_texts(core, general) == ["the root", "second reply"]
                    && reply_count(core, general, root) == 2
            })
            .await;
    }
    let in_channel = bob.read(|core| {
        core.timeline(general)
            .iter()
            .find(|i| i.document.plain_text() == "second reply")
            .cloned()
            .unwrap()
    });
    assert_eq!(in_channel.thread_root, Some(root));
    assert!(in_channel.in_channel);
    // It is one message: the thread holds the same one, not a copy.
    bob.wait_until("the second reply in the thread", move |core| {
        reply_texts(core, root) == ["first reply", "second reply"]
    })
    .await;
    assert_eq!(stored_thread(&world, root).await, (2, 1));

    // The most recent replier comes first.
    let alice_id = alice.read(|core| core.me().unwrap().id);
    let summary = bob.read(|core| channel_row(core, general, root).unwrap().thread.unwrap());
    assert_eq!(summary.recent_repliers, [alice_id, bob_id]);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn only_what_the_channel_lists_counts_as_unread_in_the_channel(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob, carol) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
        start(&world, dir.path(), "carol"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    carol.register("carol").await;
    let general = general(&alice);

    // Carol reads another channel, so #general counts up for her.
    alice.act(Input::CreateChannel {
        name: "design".to_owned(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    alice
        .wait_until("the new channel", |core| {
            core.conversations().iter().any(|c| !c.is_default)
        })
        .await;
    let design = alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| !c.is_default)
                .map(|c| c.id)
        })
        .unwrap();
    carol.act(Input::BrowseChannels);
    carol
        .wait_until("the directory", move |core| {
            core.browse()
                .channels
                .iter()
                .any(|b| b.conversation.id == design)
        })
        .await;
    carol.act(Input::JoinChannel(design));
    carol
        .wait_until("the join", move |core| core.conversation(design).is_some())
        .await;
    carol.act(Input::SelectConversation(design));
    carol
        .wait_until("design selected", move |core| {
            core.selected_conversation() == Some(design)
        })
        .await;

    alice.act(Input::SelectConversation(general));
    alice.send("a root");
    alice.wait_confirmed("a root").await;
    bob.wait_for_text("a root").await;
    let root = root_of(&alice, general, "a root");
    open_thread(&bob, general, root).await;
    reply(&bob, general, root, "thread only", false).await;
    reply(&bob, general, root, "also in channel", true).await;
    carol
        .wait_until("what the channel lists", move |core| {
            core.user_state(general)
                .is_some_and(|s| s.unread_count == 2)
        })
        .await;
    // The thread-only reply never counted: the root and the reply sent to the channel did.
    assert_eq!(
        carol.read(|core| core.user_state(general).unwrap().unread_count),
        2
    );

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_thread_has_its_own_follow_and_read_state(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    alice.send("start here");
    alice.wait_confirmed("start here").await;
    bob.wait_for_text("start here").await;
    let root = root_of(&alice, general, "start here");

    // Bob replies twice; Alice started the thread, so she follows it and has two unread.
    open_thread(&bob, general, root).await;
    reply(&bob, general, root, "one", false).await;
    reply(&bob, general, root, "two", false).await;
    alice
        .wait_until("Alice to follow with two unread", move |core| {
            core.thread_state(root)
                .is_some_and(|s| s.following && s.unread_count == 2)
        })
        .await;
    assert_eq!(alice.read(|core| core.followed_thread_unread_total()), 2);
    // Bob replied, so he follows it too, and his own replies are not unread.
    bob.wait_until("Bob to follow", move |core| {
        core.thread_state(root)
            .is_some_and(|s| s.following && s.unread_count == 0)
    })
    .await;

    // Reading the thread: the pane is open and its newest reply is on screen.
    open_thread(&alice, general, root).await;
    alice.act(Input::ThreadAtBottom {
        root,
        at_bottom: true,
    });
    alice
        .wait_until("the thread to be read", move |core| {
            core.thread_state(root).is_some_and(|s| s.unread_count == 0)
        })
        .await;
    let read: i64 = sqlx::query_scalar(
        "SELECT last_read FROM thread_user_state WHERE root_id = $1 \
         AND user_id = (SELECT id FROM users WHERE username = 'alice')",
    )
    .bind(root.as_uuid())
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(read > 0);

    // Unfollowing sticks: a new reply no longer counts for Bob.
    bob.act(Input::SetThreadFollow {
        root,
        following: false,
    });
    bob.wait_until("the unfollow", move |core| {
        core.thread_state(root).is_some_and(|s| !s.following)
    })
    .await;
    reply(&alice, general, root, "three", false).await;
    bob.wait_until("the reply in the thread", move |core| {
        reply_texts(core, root).contains(&"three".to_owned())
    })
    .await;
    assert!(bob.read(|core| core.thread_state(root).is_some_and(|s| !s.following)));

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn deleting_replies_and_roots_leaves_a_tombstone_only_while_replies_remain(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    alice.send("doomed root");
    alice.wait_confirmed("doomed root").await;
    bob.wait_for_text("doomed root").await;
    let root = root_of(&alice, general, "doomed root");
    open_thread(&bob, general, root).await;
    reply(&bob, general, root, "r1", false).await;
    reply(&bob, general, root, "r2", false).await;
    alice
        .wait_until("two replies", move |core| {
            reply_count(core, general, root) == 2
        })
        .await;

    // Deleting the root keeps a tombstone with its summary, and the replies stay readable.
    alice.act(Input::DeleteMessage {
        conversation: general,
        message: root,
    });
    for client in [&alice, &bob] {
        client
            .wait_until("the root to become a tombstone", move |core| {
                channel_row(core, general, root).is_some_and(|r| {
                    r.deleted && r.thread.as_ref().is_some_and(|t| t.reply_count == 2)
                })
            })
            .await;
    }
    assert_eq!(bob.read(|core| reply_texts(core, root)), ["r1", "r2"]);
    // Nobody can reply to it any more.
    bob.act(Input::SendReply {
        conversation: general,
        root,
        text: "too late".to_owned(),
        also_in_channel: false,
    });
    bob.wait_until("a refusal", |core| core.notice().is_some())
        .await;
    assert_eq!(stored_thread(&world, root).await.0, 2);

    // Deleting one reply leaves the tombstone with one; deleting the last takes the root away.
    let r1 = bob.read(|core| {
        core.thread(root)
            .and_then(|t| {
                t.replies
                    .iter()
                    .find(|r| r.document.plain_text() == "r1")
                    .and_then(|r| r.message_id())
            })
            .unwrap()
    });
    bob.act(Input::DeleteMessage {
        conversation: general,
        message: r1,
    });
    alice
        .wait_until("one reply left", move |core| {
            channel_row(core, general, root)
                .is_some_and(|r| r.thread.as_ref().is_some_and(|t| t.reply_count == 1))
        })
        .await;
    let r2 = bob.read(|core| {
        core.thread(root)
            .and_then(|t| {
                t.replies
                    .iter()
                    .find(|r| r.document.plain_text() == "r2")
                    .and_then(|r| r.message_id())
            })
            .unwrap()
    });
    bob.act(Input::DeleteMessage {
        conversation: general,
        message: r2,
    });
    for client in [&alice, &bob] {
        client
            .wait_until("the root to go with its last reply", move |core| {
                channel_row(core, general, root).is_none()
            })
            .await;
    }
    assert_eq!(stored_thread(&world, root).await, (0, 0));

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_reply_made_while_a_client_was_away_arrives_by_replay(pool: PgPool) {
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
    let general = general(&alice);
    alice.send("while you were out");
    alice.wait_confirmed("while you were out").await;
    bob.wait_for_text("while you were out").await;
    let root = root_of(&alice, general, "while you were out");
    let cached_cursor = bob.last_cached_cursor().expect("bob saved his cache");
    bob.stop().await;

    open_thread(&alice, general, root).await;
    reply(&alice, general, root, "a reply", false).await;

    let bob = Client::start("bob", &world.server.base_url, &bob_profile, bob_secrets);
    bob.wait_until_ready().await;
    bob.wait_until("the summary from replay", move |core| {
        reply_count(core, general, root) == 1
    })
    .await;
    assert!(bob.subscribe_positions().contains(&cached_cursor));
    // And the thread itself is there when Bob opens it.
    open_thread(&bob, general, root).await;
    assert_eq!(bob.read(|core| reply_texts(core, root)), ["a reply"]);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_private_thread_is_gone_for_someone_who_is_removed(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    alice.act(Input::CreateChannel {
        name: "secret".to_owned(),
        visibility: ChannelVisibility::Private,
        topic: String::new(),
        description: String::new(),
    });
    alice
        .wait_until("the channel", |core| {
            core.conversations().iter().any(|c| !c.is_default)
        })
        .await;
    let secret = alice
        .read(|core| {
            core.conversations()
                .iter()
                .find(|c| !c.is_default)
                .map(|c| c.id)
        })
        .unwrap();
    let bob_id = bob.read(|core| core.me().unwrap().id);
    alice.act(Input::AddMembers {
        id: secret,
        users: vec![bob_id],
    });
    bob.wait_until("to be added", move |core| {
        core.conversation(secret).is_some()
    })
    .await;
    alice
        .wait_until("secret selected", move |core| {
            core.selected_conversation() == Some(secret)
        })
        .await;
    alice.send("members only root");
    alice.wait_confirmed("members only root").await;
    let root = root_of(&alice, secret, "members only root");
    bob.act(Input::SelectConversation(secret));
    bob.wait_until("the root", move |core| {
        channel_row(core, secret, root).is_some()
    })
    .await;
    open_thread(&bob, secret, root).await;

    // Bob is removed: the pane closes and the thread is unreachable.
    alice.act(Input::RemoveMember {
        id: secret,
        user: bob_id,
    });
    bob.wait_until("the removal", move |core| {
        core.conversation(secret).is_none() && core.open_thread().is_none()
    })
    .await;
    let readable: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_members WHERE conversation_id = $1 AND user_id = $2)",
    )
    .bind(secret.as_uuid())
    .bind(bob_id.as_uuid())
    .fetch_one(&world.pool)
    .await
    .unwrap();
    assert!(!readable);
    bob.act(Input::OpenThread {
        conversation: secret,
        root,
    });
    bob.wait_until("nothing to open", |core| core.open_thread().is_none())
        .await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_permalink_to_a_reply_opens_its_thread(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    alice.send("the root");
    alice.wait_confirmed("the root").await;
    let root = root_of(&alice, general, "the root");
    open_thread(&alice, general, root).await;
    reply(&alice, general, root, "an early reply", false).await;
    reply(&alice, general, root, "a late reply", false).await;
    let early = alice.read(|core| {
        core.thread(root)
            .and_then(|t| {
                t.replies
                    .iter()
                    .find(|r| r.document.plain_text() == "an early reply")
                    .and_then(|r| r.message_id())
            })
            .unwrap()
    });
    let workspace = alice.read(|core| core.primary_workspace().unwrap().workspace.id);

    bob.act(Input::OpenPermalink(Permalink {
        workspace_id: workspace,
        conversation_id: general,
        message_id: early,
    }));
    bob.wait_until("the thread and the jump target", move |core| {
        core.open_thread().is_some_and(|t| t.root == root)
            && core
                .jump_target()
                .is_some_and(|t| t.key == ItemKey::Message(early))
    })
    .await;
    assert_eq!(
        bob.read(|core| reply_texts(core, root)),
        ["an early reply", "a late reply"]
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
