//! Mentions: who a message notifies, how that is decided, stored and reported, and how it feeds
//! the unread mention count through sends, edits, deletions, and reading.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::mention::MentionTarget;
use lazy_domain::{GroupId, MessageId, OperationId, UserId};
use lazy_protocol::{
    ErrorCode, Event, EventEnvelope, Mentions, Message, MessageResponse, UserStateResponse,
};
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

async fn subscribed(app: &App, who: &Account) -> Ws {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, app.head().await).await;
    ws.replay().await;
    ws
}

fn user(account: &Account) -> MentionTarget {
    MentionTarget::User {
        id: account.user.id,
    }
}

fn group(id: lazy_domain::GroupId) -> MentionTarget {
    MentionTarget::Group { id }
}

fn sorted(mut users: Vec<UserId>) -> Vec<UserId> {
    users.sort();
    users
}

fn people(accounts: &[&Account]) -> Mentions {
    Mentions {
        users: sorted(accounts.iter().map(|a| a.user.id).collect()),
        channel: false,
    }
}

/// An owner and an admin, and four ordinary members, all in #general.
struct Chat {
    app: App,
    owner: Account,
    boss: Account,
    alice: Account,
    bob: Account,
    carol: Account,
    dave: Account,
}

async fn chat(pool: PgPool) -> Chat {
    let app = App::start(pool).await;
    let owner = app.register("owner").await;
    let boss = app.register("boss").await;
    app.set_role(&boss, "admin").await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let carol = app.register("carol").await;
    let dave = app.register("dave").await;
    Chat {
        app,
        owner,
        boss,
        alice,
        bob,
        carol,
        dave,
    }
}

async fn finish(chat: Chat) {
    chat.app.server.shutdown().await.unwrap();
}

async fn mention_count(app: &App, who: &Account, conversation: lazy_domain::ConversationId) -> u32 {
    app.state_of(who, conversation).await.mention_count
}

fn mention_events(events: &[EventEnvelope]) -> Vec<&Event> {
    events
        .iter()
        .map(|e| &e.event)
        .filter(|e| {
            matches!(
                e,
                Event::MessageCreated { .. }
                    | Event::MessageEdited { .. }
                    | Event::MessageDeleted { .. }
            )
        })
        .collect()
}

#[sqlx::test]
async fn naming_people_notifies_them_and_the_decision_is_stored_with_the_message(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut ws = subscribed(app, &c.carol).await;

    let sent = app
        .send_mentioning(
            &c.alice,
            app.general,
            "hello @bob and @carol",
            &[("bob", user(&c.bob)), ("carol", user(&c.carol))],
        )
        .await;
    assert_eq!(sent.mentions, people(&[&c.bob, &c.carol]));
    assert_eq!(sent.document.mentions().len(), 2);

    // The same set everywhere the message appears.
    assert_eq!(
        app.get_message(&c.dave, sent.id).await.mentions,
        sent.mentions
    );
    let page = app.history_ok(&c.dave, app.general, &[]).await;
    assert_eq!(page.messages[0].mentions, sent.mentions);
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    match mention_events(&events)[..] {
        [Event::MessageCreated { message, .. }] => assert_eq!(message.mentions, sent.mentions),
        ref other => panic!("unexpected events {other:?}"),
    }

    // Stored exactly as returned, with one row per person for counting, and the document is
    // written as version 3 because it holds a mention.
    let stored: serde_json::Value =
        sqlx::query_scalar("SELECT mentions FROM messages WHERE id = $1")
            .bind(sent.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(stored, serde_json::to_value(&sent.mentions).unwrap());
    assert_eq!(app.mention_rows(sent.id).await, sent.mentions.users);
    let version: serde_json::Value =
        sqlx::query_scalar("SELECT document -> 'version' FROM messages WHERE id = $1")
            .bind(sent.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(version, 3);
    finish(c).await;
}

#[sqlx::test]
async fn a_message_without_mentions_is_exactly_what_it_was_before(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let plain = app.send(&c.alice, app.general, "nothing to see").await;
    assert!(plain.mentions.is_empty());

    // No `mentions` key on the wire, a version 2 document in storage, no rows, no flag.
    let raw: serde_json::Value = app
        .call(
            reqwest::Method::GET,
            &lazy_protocol::api::paths::message(plain.id),
            &c.bob,
            None,
        )
        .await
        .json::<serde_json::Value>()
        .await
        .unwrap();
    assert!(raw["message"].get("mentions").is_none());
    let (version, channel): (serde_json::Value, bool) = sqlx::query_as(
        "SELECT document -> 'version', mentions_channel FROM messages WHERE id = $1",
    )
    .bind(plain.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!((version, channel), (serde_json::json!(2), false));
    assert!(app.mention_rows(plain.id).await.is_empty());
    assert_eq!(mention_count(app, &c.bob, app.general).await, 0);
    finish(c).await;
}

#[sqlx::test]
async fn the_author_is_never_notified_and_each_person_is_listed_once(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app
        .create_group(&c.boss, "design", &[&c.alice, &c.bob, &c.carol])
        .await;

    // Alice names herself, Bob twice, and a group that holds both of them and Carol.
    let sent = app
        .send_mentioning(
            &c.alice,
            app.general,
            "@alice @bob @bob @design",
            &[
                ("alice", user(&c.alice)),
                ("bob", user(&c.bob)),
                ("design", group(team.id)),
            ],
        )
        .await;
    assert_eq!(sent.mentions, people(&[&c.bob, &c.carol]));
    assert_eq!(app.mention_rows(sent.id).await, sent.mentions.users);
    assert_eq!(
        mention_count(app, &c.alice, app.general).await,
        0,
        "not even by name"
    );
    finish(c).await;
}

#[sqlx::test]
async fn naming_someone_who_is_not_in_the_conversation_notifies_no_one(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.bob])
            .await
            .status()
            .is_success()
    );
    let team = app
        .create_group(&c.boss, "everyone-but", &[&c.bob, &c.carol, &c.dave])
        .await;

    // Carol is in the workspace but not the channel: allowed, and she is simply not notified.
    let sent = app
        .send_mentioning(
            &c.alice,
            hush.id,
            "@bob @carol",
            &[("bob", user(&c.bob)), ("carol", user(&c.carol))],
        )
        .await;
    assert_eq!(sent.mentions, people(&[&c.bob]));
    // A group is cut down to the people who are in the conversation.
    let via_group = app
        .send_mentioning(
            &c.alice,
            hush.id,
            "@everyone-but",
            &[("everyone-but", group(team.id))],
        )
        .await;
    assert_eq!(via_group.mentions, people(&[&c.bob]));
    assert_eq!(mention_count(app, &c.bob, hush.id).await, 2);
    // Carol has no state for a channel she is not in, and the other conversations are untouched.
    assert_eq!(mention_count(app, &c.carol, app.general).await, 0);
    finish(c).await;
}

#[sqlx::test]
async fn a_mention_that_cannot_be_resolved_is_an_invalid_request(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let gone = app.register("gone").await;
    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(gone.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let elsewhere = app.other_workspace(&[(&c.boss, "admin")]).await;
    let other_group = GroupId::generate();
    sqlx::query(
        "INSERT INTO user_groups (id, workspace_id, handle, name) VALUES ($1, $2, 'away', 'Away')",
    )
    .bind(other_group.as_uuid())
    .bind(elsewhere.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();

    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    for (text, entries) in [
        ("@gone", vec![("gone", user(&gone))]),
        (
            "@ghost",
            vec![(
                "ghost",
                MentionTarget::User {
                    id: UserId::generate(),
                },
            )],
        ),
        ("@nowhere", vec![("nowhere", group(GroupId::generate()))]),
        ("@away", vec![("away", group(other_group))]),
    ] {
        let document = App::mention_document(text, &entries);
        denied(
            app.send_doc_raw(&c.alice, app.general, document).await,
            ErrorCode::InvalidRequest,
        )
        .await;
    }
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(after, before, "nothing was stored");
    // And an edit that adds one is refused the same way, leaving the message as it was.
    let message = app.send(&c.alice, app.general, "fine").await;
    let response = app
        .edit_mentioning_raw(
            &c.alice,
            &message,
            OperationId::generate(),
            "@gone",
            &[("gone", user(&gone))],
        )
        .await;
    denied(response, ErrorCode::InvalidRequest).await;
    assert_eq!(app.get_message(&c.alice, message.id).await, message);
    finish(c).await;
}

#[sqlx::test]
async fn channel_mentions_follow_the_permission_rule(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let everyone = [("channel", MentionTarget::Channel)];

    // In the default channel only admins and owners may.
    denied(
        app.send_doc_raw(
            &c.bob,
            app.general,
            App::mention_document("@channel hi", &everyone),
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    let from_admin = app
        .send_mentioning(&c.boss, app.general, "@channel hi", &everyone)
        .await;
    assert_eq!(
        from_admin.mentions,
        Mentions {
            users: vec![],
            channel: true
        }
    );
    let from_owner = app
        .send_mentioning(&c.owner, app.general, "@channel hello", &everyone)
        .await;
    assert!(from_owner.mentions.channel);

    // Anywhere else anyone who can post may.
    let team = app.create_channel(&c.bob, "team", "public").await;
    assert!(
        app.act(&c.carol, team.id, "join")
            .await
            .status()
            .is_success()
    );
    let in_team = app
        .send_mentioning(&c.bob, team.id, "@channel standup", &everyone)
        .await;
    assert!(in_team.mentions.channel);
    let direct = app.open(&c.bob, &[&c.carol]).await;
    assert!(
        app.send_mentioning(&c.bob, direct.id, "@channel psst", &everyone)
            .await
            .mentions
            .channel
    );

    // Editing is held to the same rule, and a refused edit changes nothing.
    let plain = app.send(&c.bob, app.general, "plain").await;
    let refused = app
        .edit_mentioning_raw(
            &c.bob,
            &plain,
            OperationId::generate(),
            "@channel now",
            &everyone,
        )
        .await;
    denied(refused, ErrorCode::Forbidden).await;
    assert_eq!(app.get_message(&c.bob, plain.id).await, plain);
    assert_eq!(app.kept_revisions(plain.id).await, Vec::<i32>::new());
    finish(c).await;
}

#[sqlx::test]
async fn the_mention_count_is_in_bootstrap_and_follows_reading(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let everyone = [("channel", MentionTarget::Channel), ("bob", user(&c.bob))];
    let mut bob_ws = subscribed(app, &c.bob).await;

    let by_name = app
        .send_mentioning(&c.boss, app.general, "@bob look", &everyone)
        .await;
    let to_all = app
        .send_mentioning(&c.boss, app.general, "@channel news", &everyone)
        .await;
    let both = app
        .send_mentioning(&c.boss, app.general, "@channel @bob both", &everyone)
        .await;

    // Bob is named in two and reached by @channel in two: three messages, each counted once.
    let state = app.state_of(&c.bob, app.general).await;
    assert_eq!((state.unread_count, state.mention_count), (3, 3));
    // Carol is only reached by @channel; the author reads his own words.
    let carol = app.state_of(&c.carol, app.general).await;
    assert_eq!((carol.unread_count, carol.mention_count), (3, 2));
    let boss = app.state_of(&c.boss, app.general).await;
    assert_eq!((boss.unread_count, boss.mention_count), (0, 0));
    // Not even with a read position behind his own words: @channel never counts for its author.
    sqlx::query(
        "UPDATE conversation_user_state SET last_read = 0 WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(c.boss.user.id.as_uuid())
    .bind(app.general.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();
    assert_eq!(app.state_of(&c.boss, app.general).await.mention_count, 0);
    sqlx::query(
        "UPDATE conversation_user_state SET last_read = $3 WHERE user_id = $1 AND conversation_id = $2",
    )
    .bind(c.boss.user.id.as_uuid())
    .bind(app.general.as_uuid())
    .bind(both.seq.get())
    .execute(&app.pool)
    .await
    .unwrap();

    // Reading moves both counts, and the state he is sent back says so.
    let response = app.mark_read(&c.bob, app.general, by_name.seq.get()).await;
    let state = response.json::<UserStateResponse>().await.unwrap().state;
    assert_eq!((state.unread_count, state.mention_count), (2, 2));
    let state = app
        .mark_read(&c.bob, app.general, both.seq.get())
        .await
        .json::<UserStateResponse>()
        .await
        .unwrap()
        .state;
    assert_eq!((state.unread_count, state.mention_count), (0, 0));
    assert!(to_all.seq < both.seq);

    // His devices heard each state, with the mention count in it.
    let marker = app.register("marker").await;
    let events = bob_ws.events_until_marker(marker.user.id).await;
    let counts: Vec<(u32, u32)> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::UserStateChanged { state } => Some((state.unread_count, state.mention_count)),
            _ => None,
        })
        .collect();
    assert_eq!(counts, vec![(2, 2), (0, 0)]);

    // Favoriting reports the same count.
    let carol_state = app.favorite(&c.carol, app.general, true).await;
    let carol_state = carol_state.json::<UserStateResponse>().await.unwrap().state;
    assert_eq!(carol_state.mention_count, 2);
    finish(c).await;
}

#[sqlx::test]
async fn mentions_in_direct_conversations_count_for_the_other_person(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let direct = app.open(&c.alice, &[&c.bob]).await;
    app.send_mentioning(&c.alice, direct.id, "@bob ping", &[("bob", user(&c.bob))])
        .await;
    app.send(&c.alice, direct.id, "no mention").await;
    let state = app.state_of(&c.bob, direct.id).await;
    assert_eq!((state.unread_count, state.mention_count), (2, 1));
    assert_eq!(app.state_of(&c.alice, direct.id).await.mention_count, 0);
    finish(c).await;
}

#[sqlx::test]
async fn an_edit_recomputes_who_is_notified_and_reports_who_it_replaced(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mut ws = subscribed(app, &c.dave).await;
    let everyone = [("channel", MentionTarget::Channel)];

    let m1 = app
        .send_mentioning(&c.alice, app.general, "@bob hi", &[("bob", user(&c.bob))])
        .await;
    assert_eq!(mention_count(app, &c.bob, app.general).await, 1);

    // Naming someone else moves the notification.
    let m2 = app
        .edit_mentioning(&c.alice, &m1, "@carol hi", &[("carol", user(&c.carol))])
        .await;
    assert_eq!(m2.mentions, people(&[&c.carol]));
    assert_eq!(app.mention_rows(m1.id).await, vec![c.carol.user.id]);
    assert_eq!(mention_count(app, &c.bob, app.general).await, 0);
    assert_eq!(mention_count(app, &c.carol, app.general).await, 1);

    // Removing every mention clears them.
    let m3 = app.edit_mentioning(&c.alice, &m2, "plain now", &[]).await;
    assert!(m3.mentions.is_empty());
    assert!(app.mention_rows(m1.id).await.is_empty());
    assert_eq!(mention_count(app, &c.carol, app.general).await, 0);
    let flag: bool = sqlx::query_scalar("SELECT mentions_channel FROM messages WHERE id = $1")
        .bind(m1.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert!(!flag);

    // An admin adding and then dropping @channel moves the channel flag.
    let admin_message = app.send(&c.boss, app.general, "quiet").await;
    let loud = app
        .edit_mentioning(&c.boss, &admin_message, "@channel loud", &everyone)
        .await;
    assert!(loud.mentions.channel);
    assert_eq!(mention_count(app, &c.dave, app.general).await, 1);
    let quiet = app
        .edit_mentioning(&c.boss, &loud, "quiet again", &[])
        .await;
    assert!(quiet.mentions.is_empty());
    assert_eq!(mention_count(app, &c.dave, app.general).await, 0);

    // Each edit event names the set it replaced, so a client can correct its count without the
    // earlier message.
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    let replaced: Vec<(Mentions, Mentions)> = events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageEdited {
                message,
                previous_mentions,
                ..
            } => Some((previous_mentions.clone(), message.mentions.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        replaced,
        vec![
            (people(&[&c.bob]), people(&[&c.carol])),
            (people(&[&c.carol]), Mentions::default()),
            (
                Mentions::default(),
                Mentions {
                    users: vec![],
                    channel: true
                }
            ),
            (
                Mentions {
                    users: vec![],
                    channel: true
                },
                Mentions::default()
            ),
        ]
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_repeated_edit_returns_the_words_and_the_people_it_originally_notified(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let message = app.send(&c.alice, app.general, "start").await;
    let operation = OperationId::generate();
    let first = app
        .edit_mentioning_raw(
            &c.alice,
            &message,
            operation,
            "@bob first",
            &[("bob", user(&c.bob))],
        )
        .await
        .json::<MessageResponse>()
        .await
        .unwrap()
        .message;
    let second = app
        .edit_mentioning(
            &c.alice,
            &first,
            "@carol second",
            &[("carol", user(&c.carol))],
        )
        .await;
    assert_eq!(second.mentions, people(&[&c.carol]));

    // The first request again, after the message moved on: its own words with its own people.
    let again = app
        .edit_mentioning_raw(
            &c.alice,
            &message,
            operation,
            "@bob first",
            &[("bob", user(&c.bob))],
        )
        .await
        .json::<MessageResponse>()
        .await
        .unwrap()
        .message;
    assert_eq!(again.document, first.document);
    assert_eq!(again.mentions, people(&[&c.bob]));
    finish(c).await;
}

#[sqlx::test]
async fn deleting_clears_the_mentions_everywhere_and_reports_who_they_were(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let everyone = [
        ("channel", MentionTarget::Channel),
        ("bob", user(&c.bob)),
        ("carol", user(&c.carol)),
    ];
    let mut ws = subscribed(app, &c.dave).await;
    let sent = app
        .send_mentioning(&c.boss, app.general, "@channel @bob", &everyone)
        .await;
    let edited = app
        .edit_mentioning(&c.boss, &sent, "@channel @bob @carol", &everyone)
        .await;
    let had = edited.mentions.clone();
    assert_eq!(
        had,
        Mentions {
            users: sorted(vec![c.bob.user.id, c.carol.user.id]),
            channel: true
        }
    );
    assert_eq!(mention_count(app, &c.bob, app.general).await, 1);
    assert_eq!(mention_count(app, &c.dave, app.general).await, 1);

    let tombstone = app.delete(&c.boss, sent.id).await;
    assert!(tombstone.deleted && tombstone.mentions.is_empty());
    assert_eq!(
        app.get_message(&c.bob, sent.id).await.mentions,
        Mentions::default()
    );

    // Counts drop, and nothing is left to count from.
    for who in [&c.bob, &c.carol, &c.dave] {
        assert_eq!(mention_count(app, who, app.general).await, 0);
    }
    assert!(app.mention_rows(sent.id).await.is_empty());
    let (stored, flag): (serde_json::Value, bool) =
        sqlx::query_as("SELECT mentions, mentions_channel FROM messages WHERE id = $1")
            .bind(sent.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!((stored, flag), (serde_json::json!({}), false));

    // The deletion event says who it notified.
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    let reported = events.iter().find_map(|e| match &e.event {
        Event::MessageDeleted {
            had_mentions,
            message,
        } => Some((had_mentions.clone(), message.mentions.clone())),
        _ => None,
    });
    assert_eq!(reported, Some((had, Mentions::default())));

    // The earlier events about the message no longer carry its content or who it mentioned. What an
    // edit replaced stays as written: it holds user ids only, and a client undoes its count with it.
    let earlier: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT kind, payload FROM events WHERE message_id = $1 AND kind IN ('message_created', 'message_edited')",
    )
    .bind(sent.id.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(earlier.len(), 2);
    for (kind, payload) in &earlier {
        assert!(
            payload["message"].get("mentions").is_none(),
            "{kind} still names people"
        );
        assert_eq!(payload["message"]["deleted"], true, "{kind}");
    }
    let replaced = earlier
        .iter()
        .find(|(kind, _)| kind == "message_edited")
        .map(|(_, payload)| payload["previous_mentions"].clone());
    assert_eq!(
        replaced,
        Some(serde_json::to_value(people_and_channel(&[&c.bob])).unwrap())
    );
    finish(c).await;
}

fn people_and_channel(accounts: &[&Account]) -> Mentions {
    Mentions {
        channel: true,
        ..people(accounts)
    }
}

/// Replays the workspace from `cursor` as a subscriber would, and returns the events about one
/// message: (kind, message, previous_mentions or had_mentions).
async fn history_of(
    app: &App,
    who: &Account,
    cursor: lazy_domain::EventSeq,
    message: MessageId,
) -> Vec<(&'static str, Message, Option<Mentions>)> {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let (events, _) = ws.replay().await;
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message: m, .. } if m.id == message => {
                Some(("created", m.clone(), None))
            }
            Event::MessageEdited {
                message: m,
                previous_mentions,
                ..
            } if m.id == message => Some(("edited", m.clone(), Some(previous_mentions.clone()))),
            Event::MessageDeleted {
                message: m,
                had_mentions,
            } if m.id == message => Some(("deleted", m.clone(), Some(had_mentions.clone()))),
            _ => None,
        })
        .collect()
}

/// A deletion turns the message in its earlier events into a tombstone and nothing else: the ids an
/// edit replaced stay, and the deletion reports the mentions of the last version, the one that was
/// standing when it was deleted. This one's last version mentioned nobody.
#[sqlx::test]
async fn deleting_after_an_edit_that_dropped_the_mentions_keeps_what_the_edit_replaced(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;
    let sent = app
        .send_mentioning(
            &c.alice,
            app.general,
            "hello @bob",
            &[("bob", user(&c.bob))],
        )
        .await;
    assert_eq!(sent.mentions, people(&[&c.bob]));
    let edited = app
        .edit_mentioning(&c.alice, &sent, "hello nobody", &[])
        .await;
    assert!(edited.mentions.is_empty());
    assert_eq!(mention_count(app, &c.bob, app.general).await, 0);
    app.delete(&c.alice, sent.id).await;

    // A subscriber replaying from before the message sees three events about it.
    let history = history_of(app, &c.carol, cursor, sent.id).await;
    let kinds: Vec<&str> = history.iter().map(|(kind, ..)| *kind).collect();
    assert_eq!(kinds, ["created", "edited", "deleted"]);
    for (kind, message, _) in &history {
        assert!(message.deleted, "{kind} still carries the content");
        assert!(message.mentions.is_empty(), "{kind} still names people");
        assert_eq!(message.document, history[2].1.document, "{kind}");
        assert_eq!(message.revision, history[2].1.revision, "{kind}");
    }
    // The edit still says it replaced {bob}; the deletion removes nothing, as the last version
    // mentioned nobody.
    assert_eq!(history[1].2, Some(people(&[&c.bob])));
    assert_eq!(history[2].2, Some(Mentions::default()));
    // And it is the same in the database.
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT payload -> 'previous_mentions' FROM events \
         WHERE message_id = $1 AND kind = 'message_edited'",
    )
    .bind(sent.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(stored, serde_json::to_value(people(&[&c.bob])).unwrap());
    finish(c).await;
}

/// The same, when the last version still mentions the person: the deletion reports them.
#[sqlx::test]
async fn deleting_a_message_whose_last_version_mentions_people_reports_them(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let cursor = app.head().await;
    let both = [("bob", user(&c.bob)), ("carol", user(&c.carol))];
    let sent = app
        .send_mentioning(&c.alice, app.general, "hello @bob", &both)
        .await;
    let edited = app
        .edit_mentioning(&c.alice, &sent, "hello @bob and @carol", &both)
        .await;
    assert_eq!(edited.mentions, people(&[&c.bob, &c.carol]));
    assert_eq!(mention_count(app, &c.bob, app.general).await, 1);
    app.delete(&c.alice, sent.id).await;
    assert_eq!(mention_count(app, &c.bob, app.general).await, 0);

    let history = history_of(app, &c.dave, cursor, sent.id).await;
    let kinds: Vec<&str> = history.iter().map(|(kind, ..)| *kind).collect();
    assert_eq!(kinds, ["created", "edited", "deleted"]);
    for (kind, message, _) in &history {
        assert!(message.deleted && message.mentions.is_empty(), "{kind}");
    }
    // The edit added carol, so it replaced {bob}; the last version was {bob, carol}.
    assert_eq!(history[1].2, Some(people(&[&c.bob])));
    assert_eq!(history[2].2, Some(people(&[&c.bob, &c.carol])));
    finish(c).await;
}

#[sqlx::test]
async fn a_group_notifies_the_people_it_held_when_the_message_was_sent(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app
        .create_group(&c.boss, "design", &[&c.bob, &c.carol])
        .await;
    let sent = app
        .send_mentioning(
            &c.alice,
            app.general,
            "@design please look",
            &[("design", group(team.id))],
        )
        .await;
    assert_eq!(sent.mentions, people(&[&c.bob, &c.carol]));
    assert_eq!(mention_count(app, &c.carol, app.general).await, 1);

    // The group changes, then goes away: the message neither changes nor stops notifying.
    let response = app
        .update_group_raw(
            &c.boss,
            team.id,
            serde_json::json!({"members": [c.dave.user.id], "handle": "ux"}),
        )
        .await;
    assert!(response.status().is_success());
    assert_eq!(
        app.get_message(&c.dave, sent.id).await.mentions,
        sent.mentions
    );
    assert_eq!(mention_count(app, &c.dave, app.general).await, 0);
    assert!(
        app.delete_group_raw(&c.boss, team.id)
            .await
            .status()
            .is_success()
    );
    let after = app.get_message(&c.dave, sent.id).await;
    assert_eq!(after.mentions, sent.mentions);
    assert_eq!(
        after.document.mentions(),
        vec![group(team.id)],
        "still names the group by identity"
    );
    assert_eq!(mention_count(app, &c.bob, app.general).await, 1);

    // A new message cannot name a group that no longer exists.
    let document = App::mention_document("@design", &[("design", group(team.id))]);
    denied(
        app.send_doc_raw(&c.alice, app.general, document).await,
        ErrorCode::InvalidRequest,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn many_mentions_arriving_at_once_are_all_counted(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let senders = [&c.alice, &c.carol, &c.dave, &c.boss, &c.owner];
    let requests = (0..15).map(|n| {
        let sender = senders[n % senders.len()];
        let (http, url, token) = (
            app.http.clone(),
            app.url(&lazy_protocol::api::paths::messages(app.general)),
            sender.token.clone(),
        );
        let body = serde_json::json!({
            "operation_id": OperationId::generate(),
            "document": App::mention_document(&format!("@bob number {n}"), &[("bob", user(&c.bob))]),
        });
        tokio::spawn(async move {
            http.post(url).bearer_auth(token).json(&body).send().await.unwrap().status().as_u16()
        })
    });
    for status in tokio::time::timeout(DEADLINE, join_all(requests))
        .await
        .expect("the sends finish")
    {
        assert_eq!(status.unwrap(), 200);
    }
    let state = app.state_of(&c.bob, app.general).await;
    assert_eq!((state.unread_count, state.mention_count), (15, 15));
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM message_mentions WHERE user_id = $1")
        .bind(c.bob.user.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 15);
    finish(c).await;
}
