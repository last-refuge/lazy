//! Phase 2A end to end: channels, direct and group conversations, membership changes, read state.
//! Real client stacks (core and runtime) against a real server and Postgres.

mod support;

use lazy_client_core::{
    ActionKind, ActionState, ClientCore, ConnectionState, Delivery, Input, SessionPhase,
};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ChannelVisibility, ConversationId, ConversationKind, UserId};
use sqlx::PgPool;
use support::proxy::{Mode, Proxy};
use support::{Client, PASSWORD, World};

fn channel(core: &ClientCore, name: &str) -> Option<ConversationId> {
    core.conversations()
        .iter()
        .find(|c| c.name.as_ref().is_some_and(|n| n.as_str() == name))
        .map(|c| c.id)
}

fn user_id(core: &ClientCore, username: &str) -> Option<UserId> {
    core.members()
        .into_iter()
        .find(|u| u.username.as_str() == username)
        .map(|u| u.id)
}

async fn wait_channel(client: &Client, name: &str) -> ConversationId {
    client
        .wait_until(&format!("channel #{name}"), |core| {
            channel(core, name).is_some()
        })
        .await;
    client
        .read(|core| channel(core, name))
        .expect("channel is known")
}

async fn create_channel(
    client: &Client,
    name: &str,
    visibility: ChannelVisibility,
) -> ConversationId {
    client.act(Input::CreateChannel {
        name: name.to_owned(),
        visibility,
        topic: String::new(),
        description: String::new(),
    });
    let id = wait_channel(client, name).await;
    client
        .wait_until("channel creation to finish", |core| {
            *core.action(ActionKind::CreateChannel) == ActionState::Idle
        })
        .await;
    id
}

async fn select(client: &Client, id: ConversationId) {
    client.act(Input::SelectConversation(id));
    client
        .wait_until("the conversation to be selected", move |core| {
            core.selected_conversation() == Some(id)
        })
        .await;
}

/// Signs in on a fresh profile, as a second device of an existing account would.
async fn sign_in(client: &Client, username: &str) {
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

fn unread(core: &ClientCore, id: ConversationId) -> u32 {
    core.user_state(id).map_or(0, |state| state.unread_count)
}

async fn count_conversations(world: &World, kinds: &[&str]) -> i64 {
    let kinds: Vec<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
    sqlx::query_scalar("SELECT count(*) FROM conversations WHERE kind = ANY($1)")
        .bind(kinds)
        .fetch_one(&world.pool)
        .await
        .expect("count conversations")
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn channels_are_created_browsed_joined_and_left(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    // A new account belongs to the default channel only.
    assert_eq!(bob.read(|core| core.conversations().len()), 1);
    let general = bob
        .read(|core| channel(core, "general"))
        .expect("the default channel");
    assert!(bob.read(|core| !core.can_leave(general) && !core.can_archive(general)));

    // Alice creates a public channel; Bob finds it by browsing and joins.
    let design = create_channel(&alice, "design", ChannelVisibility::Public).await;
    assert!(
        bob.read(|core| channel(core, "design")).is_none(),
        "not in the sidebar until joined"
    );
    bob.act(Input::BrowseChannels);
    bob.wait_until("the directory", |core| {
        core.browse_results()
            .iter()
            .any(|b| b.conversation.id == design)
    })
    .await;
    let joinable = bob.read(|core| {
        let entry = core
            .browse_results()
            .iter()
            .find(|b| b.conversation.id == design)
            .unwrap()
            .clone();
        (entry.joined, core.can_join(&entry))
    });
    assert_eq!(joinable, (false, true));
    bob.act(Input::JoinChannel(design));
    bob.wait_until("the joined channel", |core| {
        core.conversation(design).is_some()
    })
    .await;
    select(&bob, design).await;

    alice.act(Input::SelectConversation(design));
    alice.send("welcome to #design");
    bob.wait_for_text("welcome to #design").await;
    alice
        .wait_until("two members", |core| {
            core.conversation(design)
                .is_some_and(|c| c.member_count == 2)
        })
        .await;

    // Bob leaves; it disappears from his sidebar and the count drops for Alice.
    bob.act(Input::LeaveChannel(design));
    bob.wait_until("the channel to leave his sidebar", |core| {
        core.conversation(design).is_none()
    })
    .await;
    alice
        .wait_until("one member", |core| {
            core.conversation(design)
                .is_some_and(|c| c.member_count == 1)
        })
        .await;
    assert_ne!(
        bob.read(|core| core.selected_conversation()),
        Some(design),
        "selection falls back"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_member_removed_while_offline_never_receives_what_followed(pool: PgPool) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let bob_profile = dir.path().join("bob");
    let bob_secrets = MemoryStorage::new();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let bob = Client::start("bob", &proxy.url, &bob_profile, bob_secrets.clone());
    alice.register("alice").await;
    bob.register("bob").await;

    // A private channel is invisible to Bob until Alice adds him; then he sees its history.
    let secret = create_channel(&alice, "secret", ChannelVisibility::Private).await;
    alice.act(Input::SelectConversation(secret));
    alice.send("before bob joined");
    alice.wait_confirmed("before bob joined").await;
    bob.act(Input::BrowseChannels);
    bob.wait_until("the directory", |core| !core.browse_results().is_empty())
        .await;
    assert!(bob.read(|core| {
        core.browse_results()
            .iter()
            .all(|b| b.conversation.id != secret)
    }));
    alice
        .wait_until("Bob's profile", |core| user_id(core, "bob").is_some())
        .await;
    let bob_id = alice.read(|core| user_id(core, "bob")).unwrap();
    alice.act(Input::AddMembers {
        id: secret,
        users: vec![bob_id],
    });
    bob.wait_until("the private channel to appear", |core| {
        core.conversation(secret).is_some()
    })
    .await;
    select(&bob, secret).await;
    bob.wait_for_text("before bob joined").await;

    // Bob's network drops. Alice removes him and keeps talking.
    proxy.set_mode(Mode::Down);
    proxy.sever_all();
    bob.wait_until("a reconnecting banner", |core| {
        matches!(core.connection(), ConnectionState::Reconnecting { .. })
    })
    .await;
    alice.act(Input::RemoveMember {
        id: secret,
        user: bob_id,
    });
    alice
        .wait_until("the removal", |core| {
            core.conversation(secret)
                .is_some_and(|c| c.member_count == 1)
        })
        .await;
    alice.send("after removal one");
    alice.send("after removal two");
    alice.wait_confirmed("after removal two").await;

    // The network returns. Bob learns he was removed and never sees anything that followed.
    proxy.set_mode(Mode::Pass);
    bob.wait_until("the removal to arrive", |core| {
        core.conversation(secret).is_none()
    })
    .await;
    bob.wait_until("a live connection", |core| {
        core.connection() == ConnectionState::Live
    })
    .await;
    let notice = bob.read(|core| core.notice().map(str::to_owned));
    assert!(
        notice.as_deref().is_some_and(|n| n.contains("secret")),
        "he is told: {notice:?}"
    );
    let leaked = bob.read(|core| {
        core.conversations()
            .iter()
            .flat_map(|c| core.timeline(c.id))
            .any(|item| item.document.plain_text().starts_with("after removal"))
    });
    assert!(!leaked, "no message from after the removal reached Bob");

    // And nothing about the channel comes back from his local cache after a restart.
    bob.stop().await;
    let bob = Client::start("bob", &proxy.url, &bob_profile, bob_secrets);
    bob.wait_until_ready().await;
    assert!(bob.read(|core| channel(core, "secret")).is_none());

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn direct_conversations_are_idempotent_and_adding_people_starts_a_new_one(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    let zoe = Client::start(
        "zoe",
        &world.server.base_url,
        &dir.path().join("zoe"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    zoe.register("zoe").await;
    // Everyone needs to know everyone to name them.
    alice
        .wait_until("everyone known", |core| core.members().len() == 3)
        .await;
    let (bob_id, zoe_id) =
        alice.read(|core| (user_id(core, "bob").unwrap(), user_id(core, "zoe").unwrap()));
    let alice_id = bob.read(|core| user_id(core, "alice")).unwrap();

    // Opening the same direct conversation twice finds the same one.
    alice.act(Input::OpenConversation {
        participants: vec![bob_id],
    });
    alice
        .wait_until("the direct conversation", |core| {
            core.conversations()
                .iter()
                .any(|c| c.kind == ConversationKind::Direct)
        })
        .await;
    let direct = alice.read(|core| core.selected_conversation()).unwrap();
    assert_eq!(
        alice.read(|core| core.conversation(direct).unwrap().kind),
        ConversationKind::Direct
    );
    alice.act(Input::OpenConversation {
        participants: vec![bob_id],
    });
    alice
        .wait_until("the second open to finish", |core| {
            *core.action(ActionKind::OpenConversation) == ActionState::Idle
        })
        .await;
    assert_eq!(
        count_conversations(&world, &["direct"]).await,
        1,
        "same people, same conversation"
    );

    // It reads naturally for each side and carries messages.
    bob.wait_until("the direct conversation", |core| {
        core.conversation(direct).is_some()
    })
    .await;
    assert_eq!(
        bob.read(|core| core.conversation_title(direct)),
        "User alice"
    );
    assert_eq!(
        alice.read(|core| core.conversation_title(direct)),
        "User bob"
    );
    alice.send("just us");
    select(&bob, direct).await;
    bob.wait_for_text("just us").await;

    // Adding Zoe is a *different* set: a new group with no history; the direct one is untouched.
    alice.act(Input::OpenConversation {
        participants: vec![bob_id, zoe_id],
    });
    alice
        .wait_until("the group", |core| {
            core.conversations()
                .iter()
                .any(|c| c.kind == ConversationKind::Group)
        })
        .await;
    let group = alice.read(|core| core.selected_conversation()).unwrap();
    assert_ne!(group, direct);
    assert!(
        alice.read(|core| core.timeline(group).is_empty()),
        "a new group starts empty"
    );
    assert_eq!(count_conversations(&world, &["direct", "group"]).await, 2);
    zoe.wait_until("the group", |core| core.conversation(group).is_some())
        .await;
    assert_eq!(
        zoe.read(|core| core.conversation_title(group)),
        "User alice, User bob"
    );
    assert!(
        zoe.read(|core| core
            .conversations()
            .iter()
            .all(|c| c.kind != ConversationKind::Direct)),
        "Zoe never sees the old direct conversation"
    );

    // Two people opening the conversation with each other at once still make exactly one.
    zoe.act(Input::OpenConversation {
        participants: vec![bob_id],
    });
    bob.act(Input::OpenConversation {
        participants: vec![zoe.read(|core| user_id(core, "zoe")).unwrap()],
    });
    zoe.wait_until("zoe's direct", |core| {
        core.conversations()
            .iter()
            .any(|c| c.kind == ConversationKind::Direct)
    })
    .await;
    bob.wait_until("bob's second direct", |core| {
        core.conversations()
            .iter()
            .filter(|c| c.kind == ConversationKind::Direct)
            .count()
            == 2
    })
    .await;
    assert_eq!(
        count_conversations(&world, &["direct"]).await,
        2,
        "alice-bob and bob-zoe, each once"
    );
    let _ = alice_id;

    for client in [alice, bob, zoe] {
        client.stop().await;
    }
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn unread_counts_and_favorites_follow_a_person_across_devices(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let phone = Client::start(
        "bob-phone",
        &world.server.base_url,
        &dir.path().join("phone"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    phone.register("bob").await;
    let laptop = Client::start(
        "bob-laptop",
        &world.server.base_url,
        &dir.path().join("laptop"),
        MemoryStorage::new(),
    );
    sign_in(&laptop, "bob").await;

    // Bob joins #news on his phone; his laptop learns about it from the event stream.
    let news = create_channel(&alice, "news", ChannelVisibility::Public).await;
    phone.act(Input::BrowseChannels);
    phone
        .wait_until("the directory", |core| {
            core.browse_results()
                .iter()
                .any(|b| b.conversation.id == news)
        })
        .await;
    phone.act(Input::JoinChannel(news));
    phone
        .wait_until("joined", |core| core.conversation(news).is_some())
        .await;
    laptop
        .wait_until("the channel on the laptop", |core| {
            core.conversation(news).is_some()
        })
        .await;
    // Both are looking at #general, so #news accumulates unread messages.
    for device in [&phone, &laptop] {
        let general = device.read(|core| channel(core, "general")).unwrap();
        select(device, general).await;
    }

    alice.act(Input::SelectConversation(news));
    for text in ["one", "two", "three"] {
        alice.send(text);
        alice.wait_confirmed(text).await;
    }
    for device in [&phone, &laptop] {
        device
            .wait_until("three unread", |core| unread(core, news) == 3)
            .await;
    }
    assert_eq!(
        alice.read(|core| unread(core, news)),
        0,
        "your own messages are never unread"
    );

    // Reading on the phone marks it read everywhere.
    select(&phone, news).await;
    phone.act(Input::TimelineAtBottom {
        conversation: news,
        at_bottom: true,
    });
    phone
        .wait_until("read on the phone", |core| unread(core, news) == 0)
        .await;
    laptop
        .wait_until("read on the laptop", |core| unread(core, news) == 0)
        .await;

    // While Bob watches the bottom of the conversation, new messages never pile up as unread.
    alice.send("live");
    phone.wait_for_text("live").await;
    phone
        .wait_until("still read", |core| unread(core, news) == 0)
        .await;
    // Sending moves the read position too, on both devices.
    phone.send("mine");
    phone.wait_confirmed("mine").await;
    laptop
        .wait_until("his own message is not unread", |core| {
            unread(core, news) == 0
        })
        .await;

    // A favorite set on one device appears on the other.
    phone.act(Input::SetFavorite {
        id: news,
        favorite: true,
    });
    laptop
        .wait_until("the favorite", |core| {
            core.user_state(news).is_some_and(|s| s.favorite)
        })
        .await;
    assert!(laptop.read(|core| core.favorites().contains(&news)));

    for client in [alice, phone, laptop] {
        client.stop().await;
    }
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn archiving_closes_a_channel_but_keeps_it_readable_and_taken_names_are_reported(
    pool: PgPool,
) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    let ops = create_channel(&alice, "ops", ChannelVisibility::Public).await;
    bob.act(Input::BrowseChannels);
    bob.wait_until("the directory", |core| {
        core.browse_results()
            .iter()
            .any(|b| b.conversation.id == ops)
    })
    .await;
    bob.act(Input::JoinChannel(ops));
    bob.wait_until("joined", |core| core.conversation(ops).is_some())
        .await;
    select(&bob, ops).await;
    alice.act(Input::SelectConversation(ops));
    alice.send("before archive");
    bob.wait_for_text("before archive").await;

    // Only the creator or an admin archives it; Bob cannot even be offered the action.
    assert!(bob.read(|core| !core.can_archive(ops)));
    assert!(alice.read(|core| core.can_archive(ops)));
    alice.act(Input::ArchiveConversation(ops));
    bob.wait_until("the archive", |core| {
        core.conversation(ops).is_some_and(|c| c.archived)
    })
    .await;
    assert!(
        bob.read(|core| core
            .timeline(ops)
            .iter()
            .any(|i| i.document.plain_text() == "before archive")),
        "history stays readable"
    );

    // A send into an archived channel is refused, and the row says so instead of vanishing.
    bob.send("too late");
    bob.wait_until("a rejected row", |core| {
        core.timeline(ops)
            .iter()
            .any(|i| matches!(i.delivery, Delivery::Failed { .. }))
    })
    .await;
    assert_eq!(world.count_messages("too late").await, 0);

    // Unarchiving reopens it.
    alice.act(Input::UnarchiveConversation(ops));
    bob.wait_until("the channel to reopen", |core| {
        core.conversation(ops).is_some_and(|c| !c.archived)
    })
    .await;

    // A second channel with the same name is refused with an explanation.
    bob.act(Input::CreateChannel {
        name: "ops".into(),
        visibility: ChannelVisibility::Public,
        topic: String::new(),
        description: String::new(),
    });
    bob.wait_until("the name to be refused", |core| {
        matches!(
            core.action(ActionKind::CreateChannel),
            ActionState::Failed(_)
        )
    })
    .await;
    let message = bob.read(|core| match core.action(ActionKind::CreateChannel) {
        ActionState::Failed(message) => message.clone(),
        other => panic!("{other:?}"),
    });
    assert!(
        message.to_lowercase().contains("taken") || message.to_lowercase().contains("already"),
        "{message}"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
