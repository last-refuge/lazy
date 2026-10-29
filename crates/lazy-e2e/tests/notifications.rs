//! Phase 3D end to end: notification rules, settings and status across a person's devices, and the
//! thread inbox, with real client stacks, a real server, and real Postgres.

mod support;

use lazy_client_core::{ClientCore, Effect, Input, SessionPhase};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::notification::{
    NotificationReason, NotificationSettings, NotifyLevel, UserStatus,
};
use lazy_domain::{ConversationId, MessageId, ReactionEmoji, Timestamp};
use sqlx::PgPool;
use support::{Client, PASSWORD, World};
use time::{Duration, OffsetDateTime};

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

/// The desktop notifications this client has asked for whose body contains `text`.
fn notified(client: &Client, text: &str) -> Vec<(NotificationReason, String, u32)> {
    client
        .effects()
        .iter()
        .filter_map(|effect| match effect {
            Effect::Notify(n) if n.body.contains(text) => {
                Some((n.reason, n.title.clone(), n.count))
            }
            _ => None,
        })
        .collect()
}

fn hours_from_now(hours: i64) -> Timestamp {
    Timestamp::from(OffsetDateTime::now_utc() + Duration::hours(hours))
}

async fn sign_in_second_device(client: &Client, username: &str) {
    client
        .wait_until("the sign-in form", |core| {
            matches!(core.phase(), SessionPhase::SignedOut { .. })
        })
        .await;
    client.act(Input::SignIn {
        username: username.to_owned(),
        password: PASSWORD.to_owned(),
    });
    client.wait_until_ready().await;
}

fn unread(core: &ClientCore, conversation: ConversationId) -> u32 {
    core.user_state(conversation).map_or(0, |s| s.unread_count)
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn live_messages_notify_by_the_rules_and_nothing_else_does(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    // Bob is away from the window, so nothing is suppressed for looking at it.
    bob.act(Input::WindowActive(false));
    let bob_id = bob.read(|core| core.me().unwrap().id);

    // A direct message notifies by default; its title is the sender and it counts as attention.
    alice.act(Input::OpenConversation {
        participants: vec![bob_id],
    });
    alice
        .wait_until("the direct conversation", |core| {
            core.conversations()
                .iter()
                .any(|c| c.kind != lazy_domain::ConversationKind::Channel)
        })
        .await;
    alice.send("lunch today?");
    bob.wait_until("the notification", |core| core.attention_count() == 1)
        .await;
    let found = notified(&bob, "lunch today?");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, NotificationReason::Direct);
    assert!(found[0].1.contains("alice"), "{found:?}");
    assert!(matches!(
        bob.effects().iter().rev().find_map(|e| match e {
            Effect::SetBadge(n) => Some(*n),
            _ => None,
        }),
        Some(1)
    ));

    // A channel message that names nobody does not notify at the default level; a mention does.
    alice.act(Input::SelectConversation(general));
    alice
        .wait_until("Bob in the member list", move |core| {
            core.conversation_members(general)
                .is_some_and(|members| members.contains(&bob_id))
        })
        .await;
    alice.send("a plain channel message");
    alice.wait_confirmed("a plain channel message").await;
    alice.send("@bob please look at this");
    alice.wait_confirmed("@bob please look at this").await;
    bob.wait_until("the mention", move |core| unread(core, general) == 2)
        .await;
    assert!(notified(&bob, "a plain channel message").is_empty());
    let mention = notified(&bob, "please look at this");
    assert_eq!(mention.len(), 1);
    assert_eq!(mention[0].0, NotificationReason::Mention);

    // Muting silences even a mention; unmuting and asking for everything notifies for all.
    bob.act(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: None,
        muted: true,
    });
    bob.wait_until("the mute", move |core| core.is_muted(general))
        .await;
    alice.send("@bob are you there?");
    alice.wait_confirmed("@bob are you there?").await;
    bob.wait_until("the message", move |core| unread(core, general) == 3)
        .await;
    assert!(notified(&bob, "are you there?").is_empty(), "muted");
    bob.act(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: Some(NotifyLevel::All),
        muted: false,
    });
    bob.wait_until("the new level", move |core| {
        !core.is_muted(general) && core.effective_level(general) == NotifyLevel::All
    })
    .await;
    alice.send("hello everyone");
    alice.wait_confirmed("hello everyone").await;
    bob.wait_until("the message", move |core| unread(core, general) == 4)
        .await;
    assert_eq!(
        notified(&bob, "hello everyone")[0].0,
        NotificationReason::AllMessages
    );

    // A pause keeps the message and the count but shows nothing.
    bob.act(Input::PauseNotifications {
        until: Some(hours_from_now(2)),
    });
    bob.wait_until("the pause", |core| {
        core.notification_settings().dnd_until.is_some()
    })
    .await;
    alice.send("during the pause");
    alice.wait_confirmed("during the pause").await;
    bob.wait_until("the message", move |core| unread(core, general) == 5)
        .await;
    assert!(notified(&bob, "during the pause").is_empty());
    bob.act(Input::PauseNotifications { until: None });
    bob.wait_until("resumed", |core| {
        core.notification_settings().dnd_until.is_none()
    })
    .await;
    alice.send("after the pause");
    bob.wait_until("the notification", |_| {
        !notified(&bob, "after the pause").is_empty()
    })
    .await;

    // Looking at the conversation suppresses it: window active, conversation selected.
    bob.act(Input::WindowActive(true));
    bob.act(Input::SelectConversation(general));
    alice.send("while you are looking");
    bob.wait_for_text("while you are looking").await;
    assert!(notified(&bob, "while you are looking").is_empty());

    // Your own messages never notify you, and an edit is not a new message.
    bob.act(Input::WindowActive(false));
    bob.send("my own message");
    bob.wait_confirmed("my own message").await;
    assert!(notified(&bob, "my own message").is_empty());

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_restarted_client_does_not_notify_for_history_it_replays(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    let bob_profile = dir.path().join("bob");
    let secrets = MemoryStorage::new();
    let bob = Client::start("bob", &world.server.base_url, &bob_profile, secrets.clone());
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);
    bob.wait_until("a cache", |core| core.selected_conversation().is_some())
        .await;
    // Alice's client has to know Bob is in the channel before `@bob` can become a mention.
    let bob_id = bob.read(|core| core.me().unwrap().id);
    alice
        .wait_until("Bob in the member list", move |core| {
            core.conversation_members(general)
                .is_some_and(|members| members.contains(&bob_id))
        })
        .await;
    bob.stop().await;

    // Bob is away while two people-directed messages arrive.
    alice.send("@bob did you see this?");
    alice.wait_confirmed("@bob did you see this?").await;
    alice.send("@bob ping again");
    alice.wait_confirmed("@bob ping again").await;

    // Back online they arrive by replay: counted, never announced as new.
    let bob = Client::start("bob", &world.server.base_url, &bob_profile, secrets);
    bob.wait_until_ready().await;
    bob.act(Input::WindowActive(false));
    bob.wait_until("the replayed mentions", move |core| {
        core.user_state(general)
            .is_some_and(|s| s.mention_count == 2)
    })
    .await;
    assert!(notified(&bob, "did you see this?").is_empty());
    assert!(notified(&bob, "ping again").is_empty());
    // A new live one still notifies.
    alice.send("@bob this one is live");
    bob.wait_until("the live notification", |_| {
        !notified(&bob, "this one is live").is_empty()
    })
    .await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn settings_preferences_and_status_follow_the_person_across_devices(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (phone, bob) = (
        start(&world, dir.path(), "alice-phone"),
        start(&world, dir.path(), "bob"),
    );
    phone.register("alice").await;
    bob.register("bob").await;
    let general = general(&phone);
    let desk = start(&world, dir.path(), "alice-desk");
    sign_in_second_device(&desk, "alice").await;

    // Settings and a conversation preference reach the other device, and not another person.
    let settings = NotificationSettings {
        channel_level: NotifyLevel::All,
        show_content: false,
        thread_replies: false,
        ..Default::default()
    };
    phone.act(Input::SetNotificationSettings(settings));
    desk.wait_until("the settings on the other device", move |core| {
        *core.notification_settings() == settings
    })
    .await;
    phone.act(Input::SetConversationNotificationPrefs {
        conversation: general,
        level: Some(NotifyLevel::Nothing),
        muted: true,
    });
    desk.wait_until("the mute on the other device", move |core| {
        core.is_muted(general)
    })
    .await;
    assert_eq!(
        bob.read(|core| *core.notification_settings()),
        NotificationSettings::default()
    );
    assert!(bob.read(|core| !core.is_muted(general)));

    // A status is shown to everyone, hidden when it ends, and cleared when asked.
    let alice_id = phone.read(|core| core.me().unwrap().id);
    let status = UserStatus::new(
        "In a meeting",
        ReactionEmoji::parse("📅").ok(),
        Some(hours_from_now(1)),
    )
    .unwrap();
    phone.act(Input::SetStatus(Some(status.clone())));
    bob.wait_until("the status", move |core| core.status_of(alice_id).is_some())
        .await;
    assert_eq!(
        bob.read(|core| core.status_of(alice_id).map(|s| s.text().to_owned())),
        Some("In a meeting".to_owned())
    );
    // Two hours later, by the core's clock, it has ended and is not shown.
    bob.act(Input::Clock(hours_from_now(2)));
    assert!(bob.read(|core| core.status_of(alice_id).is_none()));
    bob.act(Input::Clock(Timestamp::now()));
    assert!(bob.read(|core| core.status_of(alice_id).is_some()));
    phone.act(Input::SetStatus(None));
    bob.wait_until("the status to clear", move |core| {
        core.status_of(alice_id).is_none()
    })
    .await;
    // An already-ended status is refused rather than stored.
    let past = UserStatus::new("Too late", None, Some(hours_from_now(-1))).unwrap();
    phone.act(Input::SetStatus(Some(past)));
    phone
        .wait_until("a refusal", |core| core.notice().is_some())
        .await;
    assert!(bob.read(|core| core.status_of(alice_id).is_none()));

    phone.stop().await;
    desk.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn the_thread_inbox_lists_followed_threads_and_marks_them_read(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    let general = general(&alice);

    // Two threads of Alice's; Bob replies in both, so Alice follows both and has unread replies.
    let mut roots: Vec<MessageId> = Vec::new();
    for text in ["first topic", "second topic"] {
        alice.send(text);
        alice.wait_confirmed(text).await;
        bob.wait_for_text(text).await;
        let root = alice.read(|core| {
            core.timeline(general)
                .iter()
                .find(|i| i.document.plain_text() == text)
                .and_then(|i| i.message_id())
                .unwrap()
        });
        bob.act(Input::OpenThread {
            conversation: general,
            root,
        });
        bob.wait_until("the thread", move |core| {
            core.open_thread().is_some_and(|t| t.root == root)
        })
        .await;
        bob.act(Input::SendReply {
            conversation: general,
            root,
            text: format!("a reply to the {text}"),
            also_in_channel: false,
        });
        let wanted = format!("a reply to the {text}");
        bob.wait_until("the reply", move |core| {
            core.thread(root).is_some_and(|t| {
                t.replies
                    .iter()
                    .any(|r| r.document.plain_text() == wanted && r.is_confirmed())
            })
        })
        .await;
        roots.push(root);
    }
    alice
        .wait_until("both threads unread", |core| core.inbox_unread_total() == 2)
        .await;

    // The inbox lists them, newest activity first, with the latest reply.
    alice.act(Input::OpenInbox);
    alice
        .wait_until("the inbox", |core| {
            core.inbox().is_some_and(|v| {
                v.entries.len() == 2 && matches!(v.status, lazy_client_core::InboxStatus::Ready)
            })
        })
        .await;
    let entries = alice.read(|core| core.inbox().unwrap().entries.clone());
    assert_eq!(entries[0].root.id, roots[1], "the newest activity first");
    assert_eq!(
        entries[0]
            .last_reply
            .as_ref()
            .map(|m| m.document.plain_text()),
        Some("a reply to the second topic".to_owned())
    );
    assert_eq!(entries[0].state.unread_count, 1);

    // Reading one thread leaves the other unread; "mark all read" clears the rest.
    alice.act(Input::OpenThread {
        conversation: general,
        root: roots[1],
    });
    alice
        .wait_until("the pane", move |core| core.open_thread().is_some())
        .await;
    alice.act(Input::ThreadAtBottom {
        root: roots[1],
        at_bottom: true,
    });
    alice
        .wait_until("one thread read", |core| core.inbox_unread_total() == 1)
        .await;
    alice.act(Input::LoadInbox { unread_only: true });
    alice
        .wait_until("only the unread thread", |core| {
            core.inbox().is_some_and(|v| {
                v.unread_only
                    && v.entries.len() == 1
                    && matches!(v.status, lazy_client_core::InboxStatus::Ready)
            })
        })
        .await;
    alice.act(Input::MarkInboxRead);
    alice
        .wait_until("everything read", |core| core.inbox_unread_total() == 0)
        .await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
