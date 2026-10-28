//! The MVP's reliability claims, exercised end to end: real clients, a real server, real Postgres.

mod support;

use lazy_client_core::{ConnectionState, Effect, SessionPhase};
use lazy_client_runtime::MemoryStorage;
use sqlx::PgPool;
use support::proxy::{Mode, Proxy};
use support::{Client, World, count_of};

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn two_clients_exchange_messages(pool: PgPool) {
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

    alice.send("hello bob");
    alice.wait_confirmed("hello bob").await;
    assert_eq!(world.count_messages("hello bob").await, 1);
    bob.wait_for_text("hello bob").await;

    bob.send("hi alice 👋 — café\nsecond line");
    alice.wait_for_text("hi alice 👋 — café\nsecond line").await;
    bob.wait_confirmed("hi alice 👋 — café\nsecond line").await;

    // Both see the same history in the same order, each message exactly once.
    let (a, b) = (alice.texts(), bob.texts());
    assert_eq!(a, b);
    assert_eq!(a, ["hello bob", "hi alice 👋 — café\nsecond line"]);
    // Bob can tell who wrote what.
    let first_author = bob.items()[0].author_id;
    let author = bob.read(|core| core.display_name(first_author).to_owned());
    assert_eq!(author, "User alice");

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_restarted_client_resumes_its_session_and_sees_history(pool: PgPool) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice_profile, bob_profile) = (dir.path().join("alice"), dir.path().join("bob"));
    let (alice_secrets, bob_secrets) = (MemoryStorage::new(), MemoryStorage::new());
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets.clone(),
    );
    // Bob reaches the server through the proxy so the test can take his network away.
    let bob = Client::start("bob", &proxy.url, &bob_profile, bob_secrets.clone());
    alice.register("alice").await;
    bob.register("bob").await;
    alice.send("before restart");
    bob.wait_for_text("before restart").await;
    let cached_cursor = bob
        .last_cached_cursor()
        .expect("bob's client saved its content cache");

    // Bob quits; the conversation moves on while he is away.
    bob.stop().await;
    alice.send("while bob was away");
    alice.wait_confirmed("while bob was away").await;

    // He reopens the app with no network at all. No sign-in, and his history is on screen at
    // once, from the cache, while the connection is still down.
    proxy.set_mode(Mode::Down);
    let bob = Client::start("bob", &proxy.url, &bob_profile, bob_secrets);
    bob.wait_until("saved history on screen without any network", |core| {
        matches!(core.phase(), SessionPhase::Ready)
            && core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .map(|i| i.document.plain_text())
                    .eq(["before restart".to_owned()])
            })
    })
    .await;
    assert_ne!(
        bob.read(|core| core.connection()),
        ConnectionState::Live,
        "the history appeared before any connection to the server"
    );

    // The network returns: the gap is filled by replay from the saved cursor.
    proxy.set_mode(Mode::Pass);
    bob.wait_until_ready().await;
    bob.wait_for_text("while bob was away").await;
    assert_eq!(bob.texts(), ["before restart", "while bob was away"]);
    assert_eq!(
        bob.subscribe_positions(),
        vec![cached_cursor],
        "resumed from the cached cursor, not from a fresh snapshot"
    );
    assert_eq!(
        bob.history_fetches(),
        0,
        "the newest page is not downloaded again for cached history"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_pruned_event_log_sends_a_restarted_client_back_to_a_fresh_start(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice_profile, bob_profile) = (dir.path().join("alice"), dir.path().join("bob"));
    let (alice_secrets, bob_secrets) = (MemoryStorage::new(), MemoryStorage::new());
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &alice_profile,
        alice_secrets,
    );
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &bob_profile,
        bob_secrets.clone(),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    alice.send("one");
    bob.wait_for_text("one").await;
    let cached_cursor = bob.last_cached_cursor().expect("a saved cache");
    bob.stop().await;

    // While bob is away the log moves on and then ages out entirely.
    alice.send("two");
    alice.send("three");
    alice.wait_confirmed("three").await;
    world.prune_event_log().await;
    let head = world.event_head().await;
    assert!(head > cached_cursor);

    // Bob's saved cursor is now older than anything the server still has. The server says so,
    // and he falls back to a clean first run without losing or doubling anything.
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &bob_profile,
        bob_secrets.clone(),
    );
    bob.wait_until_ready().await;
    bob.wait_for_text("three").await;
    assert_eq!(bob.texts(), ["one", "two", "three"]);
    assert_eq!(
        bob.subscribe_positions(),
        vec![cached_cursor, head],
        "tried the saved cursor, then restarted from the fresh snapshot"
    );
    assert!(
        bob.effects().contains(&Effect::DiscardCache),
        "the stale cache was discarded"
    );
    assert_eq!(
        bob.history_fetches(),
        1,
        "the newest page was fetched once, fresh"
    );

    // The cache written afterwards starts from the new snapshot, and it is what reaches disk.
    assert_eq!(bob.last_cached_cursor(), Some(head));

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_lost_acknowledgment_resolves_without_a_duplicate(pool: PgPool) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &proxy.url,
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

    // The server commits alice's message but she never receives the acknowledgment.
    proxy.set_mode(Mode::LoseSendAcks);
    alice.send("important");
    world.wait_for_message_count("important", 1).await;
    proxy.set_mode(Mode::Pass);

    // She reconciles by asking the server, rather than sending it again.
    alice.wait_confirmed("important").await;
    bob.wait_for_text("important").await;
    assert!(
        alice.saw_unknown_outcome(),
        "the lost acknowledgment should have left the message in doubt"
    );
    assert_eq!(count_of(&alice.texts(), "important"), 1);
    assert_eq!(count_of(&bob.texts(), "important"), 1);
    assert_eq!(
        world.count_messages("important").await,
        1,
        "the message must not be duplicated"
    );
    alice
        .wait_until("all operations settled", |core| {
            core.unsettled_operations() == 0
        })
        .await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_client_killed_mid_send_reconciles_after_restart(pool: PgPool) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let alice_profile = dir.path().join("alice");
    let alice_secrets = MemoryStorage::new();
    let alice = Client::start("alice", &proxy.url, &alice_profile, alice_secrets.clone());
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    // The message reaches the server and commits, but alice's app goes away before she hears.
    proxy.set_mode(Mode::LoseSendAcks);
    alice.send("sent just before the crash");
    world
        .wait_for_message_count("sent just before the crash", 1)
        .await;
    alice.stop().await;
    proxy.set_mode(Mode::Pass);

    // On relaunch the journal brings the message back as "in doubt" and the server settles it.
    let alice = Client::start("alice", &proxy.url, &alice_profile, alice_secrets);
    alice.wait_until_ready().await;
    alice.wait_confirmed("sent just before the crash").await;
    bob.wait_for_text("sent just before the crash").await;
    assert_eq!(count_of(&alice.texts(), "sent just before the crash"), 1);
    assert_eq!(count_of(&bob.texts(), "sent just before the crash"), 1);
    assert_eq!(world.count_messages("sent just before the crash").await, 1);
    alice
        .wait_until("all operations settled", |core| {
            core.unsettled_operations() == 0
        })
        .await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_dropped_connection_catches_up_without_gaps_or_duplicates(pool: PgPool) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    let bob = Client::start(
        "bob",
        &proxy.url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    alice.send("one");
    bob.wait_for_text("one").await;

    // Bob's network disappears. Alice keeps talking.
    proxy.set_mode(Mode::Down);
    proxy.sever_all();
    bob.wait_until("a reconnecting banner", |core| {
        matches!(core.connection(), ConnectionState::Reconnecting { .. })
    })
    .await;
    for text in ["two", "three", "four"] {
        alice.send(text);
        alice.wait_confirmed(text).await;
    }

    // The network returns; Bob replays what he missed, once each, in order.
    proxy.set_mode(Mode::Pass);
    bob.wait_for_text("four").await;
    bob.wait_until("a live connection", |core| {
        core.connection() == ConnectionState::Live
    })
    .await;
    assert_eq!(bob.texts(), ["one", "two", "three", "four"]);
    assert_eq!(alice.texts(), bob.texts());

    // And he can still send.
    bob.send("back online");
    alice.wait_for_text("back online").await;
    bob.wait_confirmed("back online").await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn older_history_pages_in_order_while_new_messages_arrive(pool: PgPool) {
    use lazy_client_core::{HistoryStatus, Input};

    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &world.server.base_url,
        &dir.path().join("alice"),
        MemoryStorage::new(),
    );
    alice.register("alice").await;

    let label = |n: usize| format!("message {n:03}");
    for n in 1..=130 {
        alice.send(&label(n));
    }
    alice.wait_confirmed(&label(130)).await;

    // Bob joins late and starts with only the newest page.
    let bob = Client::start(
        "bob",
        &world.server.base_url,
        &dir.path().join("bob"),
        MemoryStorage::new(),
    );
    bob.register("bob").await;
    assert_eq!(bob.items().len(), 50, "the first page is the newest 50");
    assert_eq!(bob.texts().last().map(String::as_str), Some("message 130"));

    // New messages keep arriving while he scrolls back, as the timeline view requests older pages.
    let conversation = bob.read(|core| core.selected_conversation()).unwrap();
    for newer in 131..=133 {
        alice.send(&label(newer));
        alice.wait_confirmed(&label(newer)).await;
        bob.wait_for_text(&label(newer)).await;
        bob.act(Input::LoadOlder { conversation });
    }
    while bob.read(|core| {
        matches!(
            core.history_status(conversation),
            HistoryStatus::Ready { has_more: true }
        )
    }) {
        let before = bob.items().len();
        bob.act(Input::LoadOlder { conversation });
        bob.wait_until("another page", move |core| {
            core.timeline(conversation).len() > before
        })
        .await;
    }

    let expected: Vec<String> = (1..=133).map(label).collect();
    assert_eq!(
        bob.texts(),
        expected,
        "every message exactly once, in order"
    );

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_message_sent_while_the_server_is_unreachable_is_delivered_once_after_it_returns(
    pool: PgPool,
) {
    let world = World::start(pool).await;
    let proxy = Proxy::start(world.server.addr).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = Client::start(
        "alice",
        &proxy.url,
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

    // Alice's connection to the server goes away and she keeps typing.
    proxy.set_mode(Mode::Down);
    proxy.sever_all();
    alice
        .wait_until("a reconnecting banner", |core| {
            matches!(core.connection(), ConnectionState::Reconnecting { .. })
        })
        .await;
    alice.send("typed while offline");
    alice
        .wait_until("the message to be in doubt or failed", |core| {
            core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .any(|i| !matches!(i.delivery, lazy_client_core::Delivery::Pending))
            })
        })
        .await;
    assert_eq!(
        world.count_messages("typed while offline").await,
        0,
        "nothing reached the server"
    );

    // The server becomes reachable again. Without any further action from Alice it is delivered.
    proxy.set_mode(Mode::Pass);
    alice.wait_confirmed("typed while offline").await;
    bob.wait_for_text("typed while offline").await;
    assert_eq!(count_of(&alice.texts(), "typed while offline"), 1);
    assert_eq!(count_of(&bob.texts(), "typed while offline"), 1);
    assert_eq!(world.count_messages("typed while offline").await, 1);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}
