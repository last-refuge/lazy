//! Phase 2C-1 end to end: reactions, emoji preferences, and shortcodes across real clients, a real
//! server, and real Postgres.

mod support;

use lazy_client_core::{ClientCore, Input};
use lazy_client_runtime::MemoryStorage;
use lazy_domain::{ConversationId, MessageId, ReactionEmoji, emoji};
use lazy_protocol::ReactionSummary;
use sqlx::PgPool;
use support::{Client, World};

fn emoji_of(glyph: &str) -> ReactionEmoji {
    ReactionEmoji::parse(glyph).expect("an emoji")
}

fn start(world: &World, dir: &std::path::Path, name: &'static str) -> Client {
    Client::start(
        name,
        &world.server.base_url,
        &dir.join(name),
        MemoryStorage::new(),
    )
}

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

fn reactions_of(core: &ClientCore, id: MessageId) -> Vec<ReactionSummary> {
    core.selected_conversation()
        .and_then(|c| {
            core.timeline(c)
                .iter()
                .find(|item| item.message_id() == Some(id))
                .map(|item| item.reactions.clone())
        })
        .unwrap_or_default()
}

/// The reactions as (glyph, number of people), in the order shown.
fn summary(core: &ClientCore, id: MessageId) -> Vec<(String, usize)> {
    reactions_of(core, id)
        .iter()
        .map(|r| (r.emoji.to_string(), r.users.len()))
        .collect()
}

async fn stored_reactions(world: &World, id: MessageId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM message_reactions WHERE message_id = $1")
        .bind(id.as_uuid())
        .fetch_one(&world.pool)
        .await
        .expect("count reactions")
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn reactions_reach_everyone_and_keep_their_order(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    alice.send("ship it");
    alice.wait_confirmed("ship it").await;
    bob.wait_for_text("ship it").await;
    let (conversation, id) = row(&alice, "ship it");
    let (party, thumbs) = (emoji_of("🎉"), emoji_of("👍"));

    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: party,
    });
    // The person's own reaction shows at once, before the server has answered.
    assert!(alice.read(|core| core.reacted_by_me(conversation, id, party)));
    bob.wait_until("alice's reaction", move |core| {
        summary(core, id) == [("🎉".to_owned(), 1)]
    })
    .await;
    assert!(bob.read(|core| !core.reacted_by_me(conversation, id, party)));

    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: party,
    });
    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: thumbs,
    });
    for (client, who) in [(&alice, "alice"), (&bob, "bob")] {
        client
            .wait_until(&format!("both reactions on {who}'s screen"), move |core| {
                summary(core, id) == [("🎉".to_owned(), 2), ("👍".to_owned(), 1)]
            })
            .await;
    }
    // Emoji in the order first used; people in the order they reacted.
    let users = alice.read(|core| reactions_of(core, id));
    let (alice_id, bob_id) = (
        alice.read(|core| core.me().unwrap().id),
        bob.read(|core| core.me().unwrap().id),
    );
    assert_eq!(users[0].users, [alice_id, bob_id]);
    assert_eq!(users[1].users, [bob_id]);
    assert_eq!(stored_reactions(&world, id).await, 3);

    // A latecomer reads them from history.
    let carol = start(&world, dir.path(), "carol");
    carol.register("carol").await;
    carol
        .wait_until("reactions in history", move |core| {
            summary(core, id) == [("🎉".to_owned(), 2), ("👍".to_owned(), 1)]
        })
        .await;

    // Withdrawing keeps the others; the last withdrawal removes the emoji from the list.
    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: party,
    });
    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: party,
    });
    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: thumbs,
    });
    for client in [&alice, &bob, &carol] {
        client
            .wait_until("every reaction withdrawn", move |core| {
                summary(core, id).is_empty()
            })
            .await;
    }
    assert_eq!(stored_reactions(&world, id).await, 0);

    alice.stop().await;
    bob.stop().await;
    carol.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_reaction_made_while_a_client_was_away_arrives_by_replay(pool: PgPool) {
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
    alice.send("react to me");
    alice.wait_confirmed("react to me").await;
    bob.wait_for_text("react to me").await;
    let cached_cursor = bob.last_cached_cursor().expect("bob saved his cache");
    bob.stop().await;

    let (conversation, id) = row(&alice, "react to me");
    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: emoji_of("🚀"),
    });
    alice
        .wait_until("the reaction to commit", move |core| {
            core.selected_conversation().is_some_and(|c| {
                core.timeline(c)
                    .iter()
                    .any(|i| i.message_id() == Some(id) && i.reactions_as_of.get() > 0)
            })
        })
        .await;

    let bob = Client::start("bob", &world.server.base_url, &bob_profile, bob_secrets);
    bob.wait_until_ready().await;
    bob.wait_until("the reaction from replay", move |core| {
        summary(core, id) == [("🚀".to_owned(), 1)]
    })
    .await;
    assert_eq!(bob.subscribe_positions(), vec![cached_cursor]);
    assert_eq!(bob.history_fetches(), 0);

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn a_message_holds_a_bounded_number_of_different_reactions(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let alice = start(&world, dir.path(), "alice");
    alice.register("alice").await;
    alice.send("so many");
    alice.wait_confirmed("so many").await;
    let (conversation, id) = row(&alice, "so many");

    let kinds: Vec<ReactionEmoji> = emoji::in_group(emoji::Group::SmileysAndEmotion)
        .take(lazy_domain::MAX_REACTION_KINDS + 1)
        .map(|e| emoji_of(e.as_str()))
        .collect();
    assert_eq!(kinds.len(), lazy_domain::MAX_REACTION_KINDS + 1);
    for kind in &kinds[..lazy_domain::MAX_REACTION_KINDS] {
        alice.act(Input::ToggleReaction {
            conversation,
            message: id,
            emoji: *kind,
        });
    }
    alice
        .wait_until("the limit of reactions to be saved", move |core| {
            core.pending_reaction_changes() == 0
                && reactions_of(core, id).len() == lazy_domain::MAX_REACTION_KINDS
        })
        .await;
    assert_eq!(
        stored_reactions(&world, id).await,
        lazy_domain::MAX_REACTION_KINDS as i64
    );

    // One more kind is refused with an explanation, and nothing changes.
    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: kinds[lazy_domain::MAX_REACTION_KINDS],
    });
    alice
        .wait_until("a notice about the limit", |core| core.notice().is_some())
        .await;
    alice
        .wait_until("the extra reaction to settle", move |core| {
            core.pending_reaction_changes() == 0
        })
        .await;
    assert_eq!(
        alice.read(|core| reactions_of(core, id).len()),
        lazy_domain::MAX_REACTION_KINDS
    );
    assert_eq!(
        stored_reactions(&world, id).await,
        lazy_domain::MAX_REACTION_KINDS as i64
    );

    // Withdrawing one makes room.
    alice.act(Input::DismissNotice);
    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: kinds[0],
    });
    alice.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: kinds[lazy_domain::MAX_REACTION_KINDS],
    });
    alice
        .wait_until("room to be made", move |core| {
            core.pending_reaction_changes() == 0
                && reactions_of(core, id)
                    .iter()
                    .any(|r| r.emoji == kinds[lazy_domain::MAX_REACTION_KINDS])
        })
        .await;
    assert_eq!(
        stored_reactions(&world, id).await,
        lazy_domain::MAX_REACTION_KINDS as i64
    );

    alice.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn deleting_a_message_takes_its_reactions_with_it(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;
    alice.send("short lived");
    alice.wait_confirmed("short lived").await;
    bob.wait_for_text("short lived").await;
    let (conversation, id) = row(&alice, "short lived");
    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: emoji_of("😂"),
    });
    alice
        .wait_until("bob's reaction", move |core| {
            summary(core, id) == [("😂".to_owned(), 1)]
        })
        .await;

    alice.act(Input::DeleteMessage {
        conversation,
        message: id,
    });
    for client in [&alice, &bob] {
        client
            .wait_until("the message to go", move |core| {
                core.selected_conversation()
                    .is_some_and(|c| core.timeline(c).iter().all(|i| i.message_id() != Some(id)))
            })
            .await;
    }
    assert_eq!(stored_reactions(&world, id).await, 0);

    // A reaction that races the deletion cannot bring the message back.
    bob.act(Input::ToggleReaction {
        conversation,
        message: id,
        emoji: emoji_of("😂"),
    });
    bob.wait_until("the late reaction to settle", |core| {
        core.pending_reaction_changes() == 0
    })
    .await;
    assert_eq!(stored_reactions(&world, id).await, 0);
    assert!(bob.read(|core| reactions_of(core, id).is_empty()));

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn shortcodes_in_a_sent_message_become_emoji_except_in_code(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let (alice, bob) = (
        start(&world, dir.path(), "alice"),
        start(&world, dir.path(), "bob"),
    );
    alice.register("alice").await;
    bob.register("bob").await;

    alice.send("great job :tada: at 10:30:45 and `:tada:` too");
    bob.wait_for_text("great job 🎉 at 10:30:45 and :tada: too")
        .await;

    // The editor offers the emoji back, not the shortcode; saving another shortcode converts it.
    alice
        .wait_confirmed("great job 🎉 at 10:30:45 and :tada: too")
        .await;
    let (conversation, id) = row(&alice, "great job 🎉 at 10:30:45 and :tada: too");
    assert_eq!(
        alice.read(|core| core.edit_text(conversation, id)),
        Some("great job 🎉 at 10:30:45 and `:tada:` too".to_owned())
    );
    alice.act(Input::EditMessage {
        conversation,
        message: id,
        text: "now :+1: instead".to_owned(),
    });
    bob.wait_for_text("now 👍 instead").await;

    alice.stop().await;
    bob.stop().await;
    world.server.shutdown().await.unwrap();
}

#[sqlx::test(migrations = "../lazy-server/migrations")]
async fn emoji_preferences_survive_a_restart_and_a_damaged_file_is_ignored(pool: PgPool) {
    let world = World::start(pool).await;
    let dir = tempfile::tempdir().unwrap();
    let profile = dir.path().join("alice");
    let secrets = MemoryStorage::new();
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets.clone());
    alice.register("alice").await;
    alice.act(Input::RecordEmojiUse(emoji_of("🎉")));
    alice.act(Input::RecordEmojiUse(emoji_of("👍")));
    alice.act(Input::RecordEmojiUse(emoji_of("🎉")));
    alice.act(Input::SetSkinTone(3));
    alice.stop().await;

    let alice = Client::start("alice", &world.server.base_url, &profile, secrets.clone());
    alice.wait_until_ready().await;
    let prefs = alice.read(|core| core.emoji_prefs().clone());
    assert_eq!(prefs.skin_tone, 3);
    assert_eq!(prefs.recent, [emoji_of("🎉"), emoji_of("👍")]);
    alice.stop().await;

    // A damaged file reads as defaults without a complaint, and the next save repairs it.
    std::fs::write(profile.join("emoji-prefs.json"), b"{ not json").unwrap();
    let alice = Client::start("alice", &world.server.base_url, &profile, secrets);
    alice.wait_until_ready().await;
    assert_eq!(alice.read(|core| core.emoji_prefs().skin_tone), 0);
    assert!(alice.read(|core| core.emoji_prefs().recent.is_empty()));
    assert!(alice.read(|core| core.notice().is_none()));
    alice.act(Input::RecordEmojiUse(emoji_of("🚀")));
    alice.stop().await;
    let repaired = std::fs::read_to_string(profile.join("emoji-prefs.json")).unwrap();
    assert!(repaired.contains("🚀"), "{repaired}");

    world.server.shutdown().await.unwrap();
}
