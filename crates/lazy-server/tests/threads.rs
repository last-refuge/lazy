//! Threads: replies and the summary a root carries, where replies are listed, counting for the
//! conversation and for the thread, following and reading, deletion of replies and roots, delivery
//! and replay, membership, and the races between repliers, deletions, and edits.

mod common;

use common::*;
use futures_util::future::join_all;
use lazy_domain::mention::MentionTarget;
use lazy_domain::{ConversationId, EventSeq, MessageId, OperationId, UserId};
use lazy_protocol::{
    ErrorCode, Event, EventEnvelope, HistoryPage, Mentions, Message, OperationStatus,
    PROTOCOL_VERSION, ThreadSummary, ThreadUserState,
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

/// Everything `who` is sent from `cursor` on, as a replay (then dropped).
async fn replay_from(app: &App, who: &Account, cursor: EventSeq) -> Vec<EventEnvelope> {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, cursor).await;
    ws.replay().await.0
}

fn user(account: &Account) -> MentionTarget {
    MentionTarget::User {
        id: account.user.id,
    }
}

fn ids(accounts: &[&Account]) -> Vec<UserId> {
    accounts.iter().map(|a| a.user.id).collect()
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

/// The `Message` events about a message, in order, as (kind, message).
fn message_events_about(events: &[EventEnvelope], id: MessageId) -> Vec<(&'static str, Message)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. } if message.id == id => {
                Some(("created", message.clone()))
            }
            Event::MessageEdited { message, .. } if message.id == id => {
                Some(("edited", message.clone()))
            }
            Event::MessageDeleted { message, .. } if message.id == id => {
                Some(("deleted", message.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The summaries announced for a root, in order, with the position they were announced at.
fn summaries_of(
    events: &[EventEnvelope],
    root: MessageId,
) -> Vec<(EventSeq, Option<ThreadSummary>)> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ThreadChanged {
                root_id, summary, ..
            } if *root_id == root => Some((e.seq, summary.clone())),
            _ => None,
        })
        .collect()
}

fn thread_states_in(events: &[EventEnvelope]) -> Vec<ThreadUserState> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::ThreadStateChanged { state } => Some(state.clone()),
            _ => None,
        })
        .collect()
}

fn kinds(events: &[EventEnvelope]) -> Vec<&'static str> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { .. } => Some("message_created"),
            Event::MessageEdited { .. } => Some("message_edited"),
            Event::MessageDeleted { .. } => Some("message_deleted"),
            Event::ThreadChanged { .. } => Some("thread_changed"),
            Event::ThreadStateChanged { .. } => Some("thread_state_changed"),
            _ => None,
        })
        .collect()
}

fn texts(page: &HistoryPage) -> Vec<String> {
    page.messages
        .iter()
        .map(|m| m.document.plain_text())
        .collect()
}

async fn history(app: &App, who: &Account, conversation: ConversationId) -> HistoryPage {
    app.history_ok(who, conversation, &[]).await
}

async fn mention_count(app: &App, who: &Account, conversation: ConversationId) -> u32 {
    app.state_of(who, conversation).await.mention_count
}

async fn unread_count(app: &App, who: &Account, conversation: ConversationId) -> u32 {
    app.state_of(who, conversation).await.unread_count
}

// --- Replies and the summary --------------------------------------------------------------

#[sqlx::test]
async fn a_reply_carries_its_root_and_the_root_carries_a_summary(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "what do we think?").await;
    assert_eq!(
        (root.thread_root, root.in_channel, root.thread.is_none()),
        (None, false, true)
    );
    let cursor = app.head().await;

    let reply = app.reply(&c.bob, &root, "I think yes").await;
    assert_eq!(reply.thread_root, Some(root.id));
    assert!(!reply.in_channel && reply.thread.is_none());
    assert_eq!(reply.conversation_id, app.general);

    // The root now carries the summary, wherever it is read.
    let summary = app
        .get_message(&c.carol, root.id)
        .await
        .thread
        .expect("summary");
    assert_eq!(summary.reply_count, 1);
    assert_eq!(summary.last_reply_seq, reply.seq);
    assert_eq!(summary.last_reply_at, reply.created_at);
    assert_eq!(summary.recent_repliers, ids(&[&c.bob]));
    // The summary is announced right after the reply, and its version is that position.
    assert_eq!(summary.as_of.get(), reply.seq.get() + 1);
    let listed = history(app, &c.carol, app.general).await;
    assert_eq!(listed.messages.len(), 1, "the reply stays in its thread");
    assert_eq!(listed.messages[0].id, root.id);
    assert_eq!(listed.messages[0].thread.as_ref(), Some(&summary));
    // The reply is a message like any other.
    let fetched = app.get_message(&c.carol, reply.id).await;
    assert_eq!(fetched, reply);
    let page = app.replies_ok(&c.carol, root.id, &[]).await;
    assert_eq!(page.messages, vec![reply.clone()]);
    assert!(!page.has_more);

    // The events, in order: the reply, the root's new summary, then the people who started
    // following (the replier, then the root's author), each told only about themselves.
    let alice_events = replay_from(app, &c.alice, cursor).await;
    assert_eq!(
        kinds(&alice_events),
        ["message_created", "thread_changed", "thread_state_changed"]
    );
    let positions: Vec<i64> = alice_events.iter().map(|e| e.seq.get()).collect();
    assert_eq!(
        positions,
        [reply.seq.get(), reply.seq.get() + 1, reply.seq.get() + 3]
    );
    assert_eq!(
        summaries_of(&alice_events, root.id),
        vec![(summary.as_of, Some(summary.clone()))]
    );
    let alice_state = &thread_states_in(&alice_events)[0];
    assert_eq!(
        alice_state,
        &ThreadUserState {
            root_id: root.id,
            conversation_id: app.general,
            following: true,
            last_read: EventSeq::ZERO,
            unread_count: 1,
            mention_count: 0,
        }
    );
    let bob_events = replay_from(app, &c.bob, cursor).await;
    let bob_state = &thread_states_in(&bob_events)[0];
    assert_eq!(
        (
            bob_state.following,
            bob_state.last_read,
            bob_state.unread_count
        ),
        (true, reply.seq, 0)
    );
    assert_eq!(
        bob_events
            .iter()
            .find(|e| matches!(e.event, Event::ThreadStateChanged { .. }))
            .unwrap()
            .seq
            .get(),
        reply.seq.get() + 2
    );
    // Someone not involved sees the messages and the summary and no state.
    let carol_events = replay_from(app, &c.carol, cursor).await;
    assert_eq!(kinds(&carol_events), ["message_created", "thread_changed"]);
    finish(c).await;
}

#[sqlx::test]
async fn the_summary_lists_five_recent_repliers_and_its_version_only_grows(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let people = [&c.alice, &c.bob, &c.carol, &c.dave, &c.owner, &c.boss];
    let extra = app.register("erin").await;
    let root = app.send(&c.alice, app.general, "root").await;
    let mut ws = subscribed(app, &c.dave).await;

    // Seven repliers in turn: alice, bob, carol, dave, owner, boss, erin; then bob and alice again.
    let mut order: Vec<&Account> = people.to_vec();
    order.push(&extra);
    order.push(&c.bob);
    order.push(&c.alice);
    let mut last = None;
    for (n, replier) in order.iter().enumerate() {
        last = Some(app.reply(replier, &root, &format!("reply {n}")).await);
    }
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    let summaries = summaries_of(&events, root.id);
    assert_eq!(summaries.len(), 9);
    // Each announcement's version is its own position, so they only grow.
    for (seq, summary) in &summaries {
        assert_eq!(summary.as_ref().unwrap().as_of, *seq);
    }
    assert!(summaries.windows(2).all(|w| w[0].0 < w[1].0));
    let counts: Vec<u32> = summaries
        .iter()
        .map(|(_, s)| s.as_ref().unwrap().reply_count)
        .collect();
    assert_eq!(counts, (1..=9).collect::<Vec<u32>>());

    let summary = app.get_message(&c.carol, root.id).await.thread.unwrap();
    assert_eq!(summary, summaries.last().unwrap().1.clone().unwrap());
    // Most recent first, each person once, at most five: alice, bob, then erin, boss, owner.
    assert_eq!(
        summary.recent_repliers,
        ids(&[&c.alice, &c.bob, &extra, &c.boss, &c.owner])
    );
    assert_eq!(summary.last_reply_seq, last.unwrap().seq);

    // The first announcement lists one person; the third lists three, newest first.
    let third = summaries[2].1.clone().unwrap();
    assert_eq!(third.recent_repliers, ids(&[&c.carol, &c.bob, &c.alice]));
    finish(c).await;
}

#[sqlx::test]
async fn a_retried_reply_is_the_same_reply_and_a_changed_one_is_a_conflict(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let other = app.send(&c.alice, app.general, "another root").await;
    let operation = OperationId::generate();
    let document = || lazy_domain::MessageDocument::from_plain_text("once").unwrap();

    let first = app
        .reply_raw(&c.bob, &root, operation, document(), false)
        .await;
    let first = first
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message;
    let head = app.head().await;
    let again = app
        .reply_raw(&c.bob, &root, operation, document(), false)
        .await;
    assert!(again.status().is_success());
    assert_eq!(
        again
            .json::<lazy_protocol::SendMessageResponse>()
            .await
            .unwrap()
            .message,
        first
    );
    assert_eq!(app.head().await, head, "no second reply, no second event");
    assert_eq!(
        app.get_message(&c.alice, root.id)
            .await
            .thread
            .unwrap()
            .reply_count,
        1
    );
    match app.operation(&c.bob, operation).await {
        OperationStatus::Committed { message } => assert_eq!(message, first),
        other => panic!("unexpected status {other:?}"),
    }

    // The same id for the same words in another thread, in the channel, or not as a reply: conflict.
    denied(
        app.reply_raw(&c.bob, &other, operation, document(), false)
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.reply_raw(&c.bob, &root, operation, document(), true)
            .await,
        ErrorCode::Conflict,
    )
    .await;
    denied(
        app.send_raw(&c.bob, app.general, operation, "once").await,
        ErrorCode::Conflict,
    )
    .await;

    // A retry still returns the reply after the root was deleted, even though a new one could not.
    app.delete(&c.alice, root.id).await;
    let after = app
        .reply_raw(&c.bob, &root, operation, document(), false)
        .await;
    assert!(after.status().is_success());
    assert_eq!(
        after
            .json::<lazy_protocol::SendMessageResponse>()
            .await
            .unwrap()
            .message
            .id,
        first.id
    );
    denied(
        app.reply_raw(&c.bob, &root, OperationId::generate(), document(), false)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn what_can_and_cannot_be_replied_to(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let document = || lazy_domain::MessageDocument::from_plain_text("hi").unwrap();
    let root = app.send(&c.alice, app.general, "root").await;
    let reply = app.reply(&c.bob, &root, "a reply").await;

    // A reply is not a root: reply in the thread of its root instead.
    denied(
        app.reply_raw(&c.carol, &reply, OperationId::generate(), document(), false)
            .await,
        ErrorCode::InvalidRequest,
    )
    .await;
    // Nothing such, or a message of another conversation: not found (and not revealed).
    denied(
        app.reply_to_raw(
            &c.carol,
            app.general,
            MessageId::generate(),
            OperationId::generate(),
            document(),
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(
        app.act(&c.carol, team.id, "join")
            .await
            .status()
            .is_success()
    );
    denied(
        app.reply_to_raw(
            &c.carol,
            team.id,
            root.id,
            OperationId::generate(),
            document(),
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    // The flag belongs to replies.
    let response = app
        .call(
            reqwest::Method::POST,
            &lazy_protocol::api::paths::messages(app.general),
            &c.carol,
            Some(serde_json::json!({
                "operation_id": OperationId::generate(),
                "document": serde_json::to_value(document()).unwrap(),
                "also_in_channel": true,
            })),
        )
        .await;
    denied(response, ErrorCode::InvalidRequest).await;

    // A person who cannot post cannot reply: reading a public channel is not enough.
    let outsider_root = app.send(&c.alice, team.id, "root in team").await;
    denied(
        app.reply_raw(
            &c.dave,
            &outsider_root,
            OperationId::generate(),
            document(),
            false,
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    // Nor can anyone in an archived channel.
    assert!(
        app.act(&c.alice, team.id, "archive")
            .await
            .status()
            .is_success()
    );
    denied(
        app.reply_raw(
            &c.carol,
            &outsider_root,
            OperationId::generate(),
            document(),
            false,
        )
        .await,
        ErrorCode::Forbidden,
    )
    .await;
    // A private channel's threads do not exist for outsiders.
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    let secret = app.send(&c.alice, hush.id, "secret root").await;
    denied(
        app.reply_raw(
            &c.carol,
            &secret,
            OperationId::generate(),
            document(),
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.replies_raw(&c.carol, secret.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.follow_raw(&c.carol, secret.id, true).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.read_thread_raw(&c.carol, secret.id, EventSeq::new(1).unwrap())
            .await,
        ErrorCode::NotFound,
    )
    .await;
    // Nothing was written by any of the refusals.
    let replies: i64 =
        sqlx::query_scalar("SELECT count(*) FROM messages WHERE thread_root_id IS NOT NULL")
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(replies, 1);
    finish(c).await;
}

// --- Where replies are listed ---------------------------------------------------------------

#[sqlx::test]
async fn history_lists_roots_and_in_channel_replies_and_replies_page_by_thread(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let a = app.send(&c.alice, app.general, "a").await;
    let r1 = app.reply(&c.bob, &a, "r1 thread only").await;
    let r2 = app.reply_in_channel(&c.carol, &a, "r2 also channel").await;
    let b = app.send(&c.alice, app.general, "b").await;
    let r3 = app.reply(&c.bob, &a, "r3 thread only").await;
    let r4 = app.reply_in_channel(&c.bob, &b, "r4 also channel").await;
    assert!(!r1.in_channel && r2.in_channel && !r3.in_channel && r4.in_channel);

    // The channel: roots and in-channel replies, in position order.
    let page = history(app, &c.dave, app.general).await;
    assert_eq!(
        texts(&page),
        ["a", "r2 also channel", "b", "r4 also channel"]
    );
    assert!(!page.has_more);
    assert_eq!(page.messages[1].thread_root, Some(a.id));
    assert!(page.messages[1].in_channel);
    // Roots carry their summaries; replies carry none.
    assert_eq!(page.messages[0].thread.as_ref().unwrap().reply_count, 3);
    assert_eq!(page.messages[2].thread.as_ref().unwrap().reply_count, 1);
    assert!(page.messages[1].thread.is_none());

    // Paging the channel by two cuts the listed messages, not the replies in between.
    let newest = app
        .history_ok(&c.dave, app.general, &[("limit", "2".into())])
        .await;
    assert_eq!(texts(&newest), ["b", "r4 also channel"]);
    assert!(newest.has_more);
    let older = app
        .history_ok(
            &c.dave,
            app.general,
            &[
                ("limit", "2".into()),
                ("before", newest.messages[0].seq.get().to_string()),
            ],
        )
        .await;
    assert_eq!(texts(&older), ["a", "r2 also channel"]);
    assert!(!older.has_more);

    // The thread: its replies, oldest first, in-channel or not; a page never mixes in other threads.
    let thread = app.replies_ok(&c.dave, a.id, &[]).await;
    assert_eq!(
        texts(&thread),
        ["r1 thread only", "r2 also channel", "r3 thread only"]
    );
    assert!(thread.messages.iter().all(|m| m.thread_root == Some(a.id)));
    // Paging a thread: newest page first, older replies behind it.
    let newest = app
        .replies_ok(&c.dave, a.id, &[("limit", "2".into())])
        .await;
    assert_eq!(texts(&newest), ["r2 also channel", "r3 thread only"]);
    assert!(newest.has_more);
    let older = app
        .replies_ok(
            &c.dave,
            a.id,
            &[
                ("limit", "2".into()),
                ("before", newest.messages[0].seq.get().to_string()),
            ],
        )
        .await;
    assert_eq!(texts(&older), ["r1 thread only"]);
    assert!(!older.has_more);
    // A top-level message nobody replied to has an empty thread.
    let lonely = app.send(&c.alice, app.general, "lonely").await;
    assert!(
        app.replies_ok(&c.dave, lonely.id, &[])
            .await
            .messages
            .is_empty()
    );
    // A reply is not a thread, nor is something that does not exist.
    denied(
        app.replies_raw(&c.dave, r1.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.replies_raw(&c.dave, MessageId::generate(), &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    // Reading a reply by id gives its root.
    assert_eq!(
        app.get_message(&c.dave, r3.id).await.thread_root,
        Some(a.id)
    );
    finish(c).await;
}

#[sqlx::test]
async fn a_conversation_counts_only_what_its_history_lists(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let everyone = [("channel", MentionTarget::Channel), ("dave", user(&c.dave))];
    let root = app.send(&c.alice, app.general, "root").await;
    assert_eq!(unread_count(app, &c.dave, app.general).await, 1);

    // Replies that stay in their thread are not unread in the channel, and their mentions are not
    // the channel's mentions.
    app.reply(&c.bob, &root, "thread only").await;
    app.reply_mentioning(&c.bob, &root, "@dave look", &everyone, false)
        .await;
    app.reply_mentioning(&c.boss, &root, "@channel everyone", &everyone, false)
        .await;
    assert_eq!(unread_count(app, &c.dave, app.general).await, 1);
    assert_eq!(mention_count(app, &c.dave, app.general).await, 0);

    // The same replies sent also to the channel count there, once each.
    let loud = app
        .reply_mentioning(&c.boss, &root, "@dave @channel both", &everyone, true)
        .await;
    let named = app
        .reply_mentioning(&c.bob, &root, "@dave again", &everyone, true)
        .await;
    let plain = app
        .reply_in_channel(&c.carol, &root, "just in channel")
        .await;
    let state = app.state_of(&c.dave, app.general).await;
    assert_eq!((state.unread_count, state.mention_count), (4, 2));
    // The author is not counted for their own, and reading the channel clears the lot.
    assert_eq!(unread_count(app, &c.bob, app.general).await, 3);
    let read = app.mark_read(&c.dave, app.general, plain.seq.get()).await;
    assert!(read.status().is_success());
    let state = app.state_of(&c.dave, app.general).await;
    assert_eq!((state.unread_count, state.mention_count), (0, 0));
    assert!(loud.seq < named.seq && named.seq < plain.seq);

    // A reply never moves its author's read position in the channel: someone who answers in a
    // thread has not read what the channel says meanwhile.
    let before = app.state_of(&c.carol, app.general).await;
    app.send(&c.alice, app.general, "newer").await;
    app.reply(&c.carol, &root, "from a thread pane").await;
    let after = app.state_of(&c.carol, app.general).await;
    assert_eq!(after.last_read, before.last_read);
    assert_eq!(after.unread_count, before.unread_count + 1);
    finish(c).await;
}

// --- A person's thread: counts, following, reading -----------------------------------------

#[sqlx::test]
async fn thread_counts_follow_the_replies_a_person_has_not_read(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let everyone = [
        ("channel", MentionTarget::Channel),
        ("alice", user(&c.alice)),
    ];
    let root = app.send(&c.alice, app.general, "root").await;
    let r1 = app.reply(&c.bob, &root, "one").await;
    let r2 = app
        .reply_mentioning(&c.carol, &root, "@alice two", &everyone, false)
        .await;
    let r3 = app
        .reply_mentioning(&c.boss, &root, "@channel three", &everyone, false)
        .await;
    let r4 = app.reply(&c.alice, &root, "my own").await;
    let r5 = app.reply(&c.bob, &root, "five").await;

    // Alice started it and replied: after her own reply (r4) she has one unread (r5) of the two
    // others' replies after it; read position moves with her own reply.
    let alice = app.thread_state(&c.alice, root.id).await.expect("listed");
    assert_eq!(alice.last_read, r4.seq);
    assert_eq!((alice.unread_count, alice.mention_count), (1, 0));
    // Bob replied first and again last: his own never count, the others' after r1 do.
    let bob = app.thread_state(&c.bob, root.id).await.expect("listed");
    assert_eq!(bob.last_read, r5.seq);
    assert_eq!((bob.unread_count, bob.mention_count), (0, 0));
    // A person who follows from the start with nothing read counts everything from others.
    app.follow(&c.dave, root.id, true).await;
    app.read_thread(&c.dave, root.id, EventSeq::ZERO).await;
    let dave = app.thread_state(&c.dave, root.id).await.expect("listed");
    assert_eq!(
        (
            dave.following,
            dave.last_read,
            dave.unread_count,
            dave.mention_count
        ),
        (true, r5.seq, 0, 0)
    );
    // Replies before alice's own were unread for her until she wrote: rewind to see them counted,
    // naming her (r2) and @channel (r3) as mentions.
    sqlx::query("UPDATE thread_user_state SET last_read = 0 WHERE user_id = $1 AND root_id = $2")
        .bind(c.alice.user.id.as_uuid())
        .bind(root.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    let alice = app.thread_state(&c.alice, root.id).await.expect("listed");
    assert_eq!((alice.unread_count, alice.mention_count), (4, 2));
    // The commands count the same way as the bootstrap: her own reply (r4) is not unread, the
    // reply naming her and the one using @channel are mentions.
    assert_eq!(
        app.read_thread(&c.alice, root.id, EventSeq::ZERO).await,
        alice
    );
    // Nothing of this is in the conversation's counts.
    assert_eq!(mention_count(app, &c.alice, app.general).await, 0);
    assert!(r1.seq < r2.seq && r2.seq < r3.seq);

    // Editing a reply moves its mention; deleting one takes it out of the counts.
    let edited = app.edit_mentioning(&c.carol, &r2, "no one now", &[]).await;
    assert!(edited.mentions.is_empty());
    let alice = app.thread_state(&c.alice, root.id).await.expect("listed");
    assert_eq!((alice.unread_count, alice.mention_count), (4, 1));
    app.delete(&c.boss, r3.id).await;
    let alice = app.thread_state(&c.alice, root.id).await.expect("listed");
    assert_eq!((alice.unread_count, alice.mention_count), (3, 0));
    finish(c).await;
}

#[sqlx::test]
async fn marking_a_thread_read_moves_forward_clamps_and_a_no_op_commits_nothing(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let r1 = app.reply(&c.bob, &root, "one").await;
    let r2 = app.reply(&c.bob, &root, "two").await;
    let r3 = app.reply(&c.bob, &root, "three").await;
    // Alice started the thread, so she follows with nothing read.
    let mut ws = subscribed(app, &c.alice).await;
    assert_eq!(
        app.thread_state(&c.alice, root.id)
            .await
            .unwrap()
            .unread_count,
        3
    );

    let state = app.read_thread(&c.alice, root.id, r1.seq).await;
    assert_eq!((state.last_read, state.unread_count), (r1.seq, 2));
    // Only forward.
    let head = app.head().await;
    let state = app.read_thread(&c.alice, root.id, EventSeq::ZERO).await;
    assert_eq!((state.last_read, state.unread_count), (r1.seq, 2));
    let state = app.read_thread(&c.alice, root.id, r1.seq).await;
    assert_eq!(state.last_read, r1.seq);
    assert_eq!(
        app.head().await,
        head,
        "a read that moves nothing commits nothing"
    );
    // Clamped to the newest reply, however far ahead it points.
    let state = app
        .read_thread(
            &c.alice,
            root.id,
            EventSeq::new(r3.seq.get() + 1000).unwrap(),
        )
        .await;
    assert_eq!((state.last_read, state.unread_count), (r3.seq, 0));
    let head = app.head().await;
    let again = app
        .read_thread(&c.alice, root.id, EventSeq::new(r3.seq.get() + 5).unwrap())
        .await;
    assert_eq!(again, state);
    assert_eq!(app.head().await, head);
    assert!(r2.seq < r3.seq);

    // Alice's other devices were told of the two moves, and only those.
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    let states = thread_states_in(&events);
    assert_eq!(states.len(), 2);
    assert_eq!(
        states
            .iter()
            .map(|s| (s.last_read, s.unread_count))
            .collect::<Vec<_>>(),
        [(r1.seq, 2), (r3.seq, 0)]
    );

    // A person with no state for the thread has nothing to move, and nothing is stored or sent.
    let head = app.head().await;
    let dave = app.read_thread(&c.dave, root.id, r3.seq).await;
    assert_eq!(
        (dave.following, dave.last_read, dave.unread_count),
        (false, r3.seq, 0)
    );
    assert_eq!(app.head().await, head);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM thread_user_state WHERE user_id = $1")
        .bind(c.dave.user.id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // The root must be a root the caller can see.
    denied(
        app.read_thread_raw(&c.alice, r1.id, r1.seq).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.read_thread_raw(&c.alice, MessageId::generate(), r1.seq)
            .await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

async fn state(app: &App, who: &Account, root: MessageId) -> Option<ThreadUserState> {
    app.thread_state(who, root).await
}

#[sqlx::test]
async fn following_is_idempotent_and_replies_start_it_by_the_rules(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mentions = [
        ("carol", user(&c.carol)),
        ("dave", user(&c.dave)),
        ("alice", user(&c.alice)),
        ("boss", user(&c.boss)),
    ];
    let root = app.send(&c.alice, app.general, "root").await;

    // Nobody follows before anyone replies, and following by choice is idempotent.
    assert!(state(app, &c.carol, root.id).await.is_none());
    let head = app.head().await;
    let chosen = app.follow(&c.owner, root.id, true).await;
    assert!(chosen.following);
    let after_follow = app.head().await;
    assert!(after_follow > head);
    assert_eq!(app.follow(&c.owner, root.id, true).await, chosen);
    assert_eq!(
        app.head().await,
        after_follow,
        "following again appends nothing"
    );
    app.follow(&c.owner, root.id, false).await;
    let after_unfollow = app.head().await;
    assert!(after_unfollow > after_follow);
    app.follow(&c.owner, root.id, false).await;
    assert_eq!(app.head().await, after_unfollow);

    // Someone who unfollows without ever having had state is remembered, silently.
    let unfollowed = app.follow(&c.boss, root.id, false).await;
    assert!(!unfollowed.following);
    assert_eq!(app.head().await, after_unfollow, "nothing visible changed");

    // The first reply, by bob, mentioning carol and boss: bob (replier), alice (the root's author)
    // and carol (named) follow. Boss chose not to. Dave, not named, does not.
    let first = app
        .reply_mentioning(&c.bob, &root, "@carol @boss see this", &mentions, false)
        .await;
    for who in [&c.alice, &c.bob, &c.carol] {
        assert!(state(app, who, root.id).await.expect("has state").following);
    }
    assert!(state(app, &c.dave, root.id).await.is_none());
    assert!(
        state(app, &c.boss, root.id)
            .await
            .is_none_or(|s| !s.following)
    );
    // The root's author starts with the reply unread; a person named starts with only this reply
    // unread; the replier has read to their reply.
    assert_eq!(state(app, &c.alice, root.id).await.unwrap().unread_count, 1);
    let carol = state(app, &c.carol, root.id).await.unwrap();
    assert_eq!(
        (carol.last_read, carol.unread_count, carol.mention_count),
        (EventSeq::ZERO, 1, 1)
    );
    assert_eq!(
        state(app, &c.bob, root.id).await.unwrap().last_read,
        first.seq
    );

    // A long thread, and then dave is named: only the naming reply is unread for him.
    app.reply(&c.bob, &root, "two").await;
    let before_dave = app.reply(&c.bob, &root, "three").await;
    let naming = app
        .reply_mentioning(&c.bob, &root, "@dave join us", &mentions, false)
        .await;
    let dave = state(app, &c.dave, root.id)
        .await
        .expect("named, so following");
    assert_eq!(
        (
            dave.following,
            dave.last_read,
            dave.unread_count,
            dave.mention_count
        ),
        (true, before_dave.seq, 1, 1)
    );
    assert!(naming.seq > before_dave.seq);

    // Choosing to stop sticks against everything but replying: alice (the author) and dave (named)
    // unfollow; further replies and mentions do not bring them back.
    app.follow(&c.alice, root.id, false).await;
    app.follow(&c.dave, root.id, false).await;
    app.reply_mentioning(&c.bob, &root, "@alice @dave @boss again", &mentions, false)
        .await;
    assert!(!state(app, &c.alice, root.id).await.unwrap().following);
    assert!(!state(app, &c.dave, root.id).await.unwrap().following);
    assert!(
        state(app, &c.boss, root.id)
            .await
            .is_none_or(|s| !s.following)
    );
    // They are still listed, because replies are unread, but not followed.
    assert!(state(app, &c.alice, root.id).await.unwrap().unread_count > 0);
    // Replying does: alice answers her own thread and follows again.
    app.reply(&c.alice, &root, "I am back").await;
    assert!(state(app, &c.alice, root.id).await.unwrap().following);
    // And dave, who was named but never replied, can choose to follow again.
    assert!(app.follow(&c.dave, root.id, true).await.following);

    // Only the people whose following started were told, and only each of themselves.
    let marker = app.register("marker").await;
    let _ = marker;
    for (who, expected) in [
        (&c.alice, 3),
        (&c.bob, 1),
        (&c.carol, 1),
        (&c.dave, 3),
        (&c.boss, 0),
        (&c.owner, 2),
    ] {
        let told = thread_states_in(&replay_from(app, who, EventSeq::ZERO).await);
        assert!(told.iter().all(|s| s.root_id == root.id));
        assert_eq!(told.len(), expected, "{}", who.user.username);
    }
    finish(c).await;
}

// --- Deleting replies and roots -------------------------------------------------------------

/// Replays `events` for one thread and checks that every announced summary is exactly what the
/// messages before it add up to, and that its version is its own position.
fn check_thread_log(events: &[EventEnvelope], root: MessageId) {
    let mut count: u32 = 0;
    let mut last_version = EventSeq::ZERO;
    for envelope in events {
        match &envelope.event {
            Event::MessageCreated { message, .. } if message.thread_root == Some(root) => {
                count += 1
            }
            Event::MessageDeleted { message, .. } if message.thread_root == Some(root) => {
                count -= 1
            }
            Event::ThreadChanged {
                root_id, summary, ..
            } if *root_id == root => match summary {
                Some(summary) => {
                    assert_eq!(summary.reply_count, count, "at {}", envelope.seq);
                    assert_eq!(summary.as_of, envelope.seq);
                    assert!(summary.as_of > last_version);
                    last_version = summary.as_of;
                }
                None => assert_eq!(count, 0, "at {}", envelope.seq),
            },
            _ => {}
        }
    }
}

async fn stored_replies(app: &App, root: MessageId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM messages WHERE thread_root_id = $1 AND NOT deleted")
        .bind(root.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn deleting_a_reply_takes_it_out_of_its_thread_its_summary_and_the_channel(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let r1 = app.reply(&c.bob, &root, "r1").await;
    let r2 = app.reply_in_channel(&c.carol, &root, "r2 in channel").await;
    let r3 = app.reply(&c.bob, &root, "r3").await;
    let r4 = app.reply(&c.dave, &root, "r4").await;
    let cursor = app.head().await;
    assert_eq!(unread_count(app, &c.alice, app.general).await, 1);

    // The newest reply goes: the summary steps back to the one before it.
    let gone = app.delete(&c.dave, r4.id).await;
    assert!(gone.deleted && gone.thread_root == Some(root.id) && gone.thread.is_none());
    let summary = app.get_message(&c.alice, root.id).await.thread.unwrap();
    assert_eq!(summary.reply_count, 3);
    assert_eq!(summary.last_reply_seq, r3.seq);
    assert_eq!(summary.recent_repliers, ids(&[&c.bob, &c.carol]));
    // The reply that was also in the channel goes: it leaves the channel and the thread.
    app.delete(&c.carol, r2.id).await;
    assert_eq!(texts(&history(app, &c.alice, app.general).await), ["root"]);
    assert_eq!(
        texts(&app.replies_ok(&c.alice, root.id, &[]).await),
        ["r1", "r3"]
    );
    assert_eq!(
        unread_count(app, &c.dave, app.general).await,
        1,
        "only the root is left to read"
    );
    let summary = app.get_message(&c.alice, root.id).await.thread.unwrap();
    assert_eq!(
        (summary.reply_count, summary.recent_repliers.clone()),
        (2, ids(&[&c.bob]))
    );
    // Deleting twice announces nothing the second time.
    let head = app.head().await;
    assert!(app.delete(&c.carol, r2.id).await.deleted);
    assert_eq!(app.head().await, head);

    // The events say so, in order: the deletion, then the new summary at the next position.
    let events = replay_from(app, &c.alice, cursor).await;
    assert_eq!(
        kinds(&events),
        [
            "message_deleted",
            "thread_changed",
            "message_deleted",
            "thread_changed"
        ]
    );
    let announced = summaries_of(&events, root.id);
    assert_eq!(announced.len(), 2);
    for (seq, summary) in &announced {
        assert_eq!(summary.as_ref().unwrap().as_of, *seq);
    }
    check_thread_log(&replay_from(app, &c.alice, EventSeq::ZERO).await, root.id);

    // The last replies go: the summary disappears, and the root is an ordinary message again.
    app.delete(&c.bob, r1.id).await;
    app.delete(&c.bob, r3.id).await;
    let events = replay_from(app, &c.alice, cursor).await;
    assert_eq!(summaries_of(&events, root.id).last().unwrap().1, None);
    assert!(app.get_message(&c.alice, root.id).await.thread.is_none());
    assert_eq!(texts(&history(app, &c.alice, app.general).await), ["root"]);
    assert!(
        app.replies_ok(&c.alice, root.id, &[])
            .await
            .messages
            .is_empty()
    );
    let threads: i64 = sqlx::query_scalar("SELECT count(*) FROM threads")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(threads, 0);
    check_thread_log(&replay_from(app, &c.alice, EventSeq::ZERO).await, root.id);
    // A thread can start again.
    let again = app.reply(&c.carol, &root, "back").await;
    let summary = app.get_message(&c.alice, root.id).await.thread.unwrap();
    assert_eq!(
        (summary.reply_count, summary.last_reply_seq),
        (1, again.seq)
    );
    assert!(summary.as_of > r4.seq);
    check_thread_log(&replay_from(app, &c.alice, EventSeq::ZERO).await, root.id);
    finish(c).await;
}

#[sqlx::test]
async fn deleting_a_root_with_replies_leaves_a_tombstone_until_the_last_reply_goes(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "secret words").await;
    let r1 = app.reply(&c.bob, &root, "r1").await;
    let r2 = app.reply_in_channel(&c.carol, &root, "r2 in channel").await;
    let cursor = app.head().await;
    assert_eq!(unread_count(app, &c.dave, app.general).await, 2);
    let mut ws = subscribed(app, &c.dave).await;

    // Deleting the root keeps its place and its thread: the tombstone carries the summary.
    let tombstone = app.delete(&c.alice, root.id).await;
    assert!(tombstone.deleted);
    let summary = tombstone
        .thread
        .clone()
        .expect("the tombstone keeps its summary");
    assert_eq!(summary.reply_count, 2);
    assert_eq!(summary.last_reply_seq, r2.seq);
    assert_eq!(
        tombstone.document,
        lazy_domain::message::deleted_placeholder()
    );
    assert_eq!(app.get_message(&c.dave, root.id).await, tombstone);
    // History lists the tombstone (and the reply that was also in the channel); it never counts.
    let page = history(app, &c.dave, app.general).await;
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[0].id, root.id);
    assert!(page.messages[0].deleted);
    assert_eq!(page.messages[0].thread.as_ref(), Some(&summary));
    assert_eq!(page.messages[1].id, r2.id);
    assert_eq!(unread_count(app, &c.dave, app.general).await, 1);
    // The replies stay readable, and the thread still has its state.
    assert_eq!(
        texts(&app.replies_ok(&c.dave, root.id, &[]).await),
        ["r1", "r2 in channel"]
    );
    assert!(app.thread_state(&c.bob, root.id).await.is_some());
    // Nothing more can be said under it.
    denied(
        app.reply_raw(
            &c.dave,
            &tombstone,
            OperationId::generate(),
            lazy_domain::MessageDocument::from_plain_text("late").unwrap(),
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.react_raw(&c.dave, root.id, "👍", true).await,
        ErrorCode::NotFound,
    )
    .await;
    // The deletion event carries it too, and the events that announced the root are tombstones
    // without a summary of their own.
    let marker = app.register("marker").await;
    let live = ws.events_until_marker(marker.user.id).await;
    let deleted = message_events_about(&live, root.id);
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].1.thread.as_ref(), Some(&summary));
    let earlier = message_events_about(&replay_from(app, &c.dave, EventSeq::ZERO).await, root.id);
    assert_eq!(
        earlier.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        ["created", "deleted"]
    );
    assert!(earlier[0].1.deleted && earlier[0].1.thread.is_none());
    let stored: String = sqlx::query_scalar(
        "SELECT payload::text FROM events WHERE message_id = $1 AND kind = 'message_created'",
    )
    .bind(root.id.as_uuid())
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert!(!stored.contains("secret words"));

    // Replies keep going away; the tombstone follows the count.
    app.delete(&c.bob, r1.id).await;
    let page = history(app, &c.dave, app.general).await;
    assert_eq!(page.messages[0].thread.as_ref().unwrap().reply_count, 1);
    assert!(page.messages[0].deleted);
    // The last one takes the tombstone out of history, and the thread's state with it.
    app.delete(&c.carol, r2.id).await;
    assert!(history(app, &c.dave, app.general).await.messages.is_empty());
    denied(
        app.replies_raw(&c.dave, root.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.follow_raw(&c.bob, root.id, true).await,
        ErrorCode::NotFound,
    )
    .await;
    let gone = app.get_message(&c.dave, root.id).await;
    assert!(gone.deleted && gone.thread.is_none());
    let states: i64 = sqlx::query_scalar("SELECT count(*) FROM thread_user_state")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(states, 0);
    let events = replay_from(app, &c.dave, cursor).await;
    assert_eq!(summaries_of(&events, root.id).last().unwrap().1, None);
    check_thread_log(&replay_from(app, &c.dave, EventSeq::ZERO).await, root.id);

    // A root without replies is removed as before.
    let plain = app.send(&c.alice, app.general, "plain").await;
    app.delete(&c.alice, plain.id).await;
    assert!(history(app, &c.dave, app.general).await.messages.is_empty());
    denied(
        app.replies_raw(&c.dave, plain.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    finish(c).await;
}

#[sqlx::test]
async fn deleting_rewrites_earlier_events_and_keeps_what_edits_replaced(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let bob = [("bob", user(&c.bob))];
    let root = app.send(&c.alice, app.general, "root").await;
    let cursor = app.head().await;
    let reply = app
        .reply_mentioning(&c.carol, &root, "hello @bob", &bob, false)
        .await;
    assert_eq!(reply.mentions.users, ids(&[&c.bob]));
    let edited = app
        .edit_mentioning(&c.carol, &reply, "hello nobody", &[])
        .await;
    assert!(edited.mentions.is_empty());
    app.delete(&c.carol, reply.id).await;

    // The reply's events replay as tombstones that still know their thread, the edit still says
    // whom it replaced, and the deletion reports the last version's (empty) mentions.
    let events = replay_from(app, &c.dave, cursor).await;
    let about = message_events_about(&events, reply.id);
    assert_eq!(
        about.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        ["created", "edited", "deleted"]
    );
    for (kind, message) in &about {
        assert!(message.deleted && message.mentions.is_empty(), "{kind}");
        assert_eq!(message.thread_root, Some(root.id), "{kind}");
    }
    let previous = events.iter().find_map(|e| match &e.event {
        Event::MessageEdited {
            previous_mentions, ..
        } => Some(previous_mentions.clone()),
        _ => None,
    });
    assert_eq!(
        previous,
        Some(Mentions {
            users: ids(&[&c.bob]),
            channel: false
        })
    );
    let had = events.iter().find_map(|e| match &e.event {
        Event::MessageDeleted { had_mentions, .. } => Some(had_mentions.clone()),
        _ => None,
    });
    assert_eq!(had, Some(Mentions::default()));
    // No text is left anywhere.
    let words: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM events WHERE payload::text LIKE '%hello%') \
              + (SELECT count(*) FROM messages WHERE document::text LIKE '%hello%' OR plain_text LIKE '%hello%') \
              + (SELECT count(*) FROM message_revisions)",
    )
    .fetch_one(&app.pool)
    .await
    .unwrap();
    assert_eq!(words, 0);
    check_thread_log(&replay_from(app, &c.dave, EventSeq::ZERO).await, root.id);
    finish(c).await;
}

#[sqlx::test]
async fn reactions_edits_and_mentions_work_on_replies_and_roots_keep_their_summary(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = [("bob", user(&c.bob)), ("channel", MentionTarget::Channel)];
    let root = app.send(&c.alice, app.general, "root").await;
    let reply = app.reply(&c.bob, &root, "a reply").await;
    let in_channel = app.reply_in_channel(&c.carol, &root, "in channel").await;
    let mut ws = subscribed(app, &c.dave).await;

    // Reacting to a reply, and to the root: each answer is the message as it stands.
    let reacted = app.react(&c.dave, reply.id, "👍", true).await;
    assert_eq!(reacted.thread_root, Some(root.id));
    assert_eq!(reacted.reactions.len(), 1);
    let reacted_root = app.react(&c.dave, root.id, "🎉", true).await;
    let summary = reacted_root
        .thread
        .clone()
        .expect("the root's answer carries its summary");
    assert_eq!(summary.reply_count, 2);
    assert_eq!(reacted_root.reactions.len(), 1);

    // Editing the root announces it with the thread summary; editing a reply keeps its place.
    let edited_root = app.edit(&c.alice, &root, "root, edited").await;
    assert_eq!(edited_root.thread.as_ref(), Some(&summary));
    let edited_reply = app
        .edit_mentioning(&c.bob, &reply, "a reply for @bob", &team)
        .await;
    assert_eq!(
        (edited_reply.thread_root, edited_reply.seq),
        (Some(root.id), reply.seq)
    );
    let marker = app.register("marker").await;
    let events = ws.events_until_marker(marker.user.id).await;
    let edits = message_events_about(&events, root.id);
    assert_eq!(
        edits.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
        ["edited"]
    );
    assert_eq!(edits[0].1.thread.as_ref(), Some(&summary));

    // A mention in a reply is stored as the thread's, not the conversation's, unless it was also
    // sent to the channel.
    let thread_only = app
        .reply_mentioning(&c.boss, &root, "@bob only here", &team, false)
        .await;
    let also = app
        .reply_mentioning(&c.boss, &root, "@bob everywhere", &team, true)
        .await;
    let flags: Vec<(bool, MessageId)> = sqlx::query_as(
        "SELECT mm.listed, mm.message_id FROM message_mentions mm WHERE mm.message_id = ANY($1) ORDER BY mm.seq",
    )
    .bind(vec![thread_only.id.as_uuid(), also.id.as_uuid()])
    .fetch_all(&app.pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(listed, id): (bool, uuid::Uuid)| (listed, MessageId::from_uuid(id)))
    .collect();
    assert_eq!(flags, vec![(false, thread_only.id), (true, also.id)]);
    assert_eq!(mention_count(app, &c.bob, app.general).await, 1);
    // An in-channel reply is edited and the conversation's count follows it.
    app.edit_mentioning(&c.boss, &also, "nobody", &[]).await;
    assert_eq!(mention_count(app, &c.bob, app.general).await, 0);
    assert!(in_channel.in_channel);
    finish(c).await;
}

// --- Delivery, replay, membership ------------------------------------------------------------

#[sqlx::test]
async fn a_subscriber_who_was_away_replays_what_a_connected_one_saw(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let mentions = [("carol", user(&c.carol))];
    let cursor = app.head().await;
    let mut alice_live = subscribed(app, &c.alice).await;
    let mut carol_live = subscribed(app, &c.carol).await;
    let mut dave_live = subscribed(app, &c.dave).await;

    let root = app.send(&c.alice, app.general, "root").await;
    let r1 = app
        .reply_mentioning(&c.bob, &root, "@carol look", &mentions, false)
        .await;
    app.reply_in_channel(&c.carol, &root, "seen").await;
    app.read_thread(&c.carol, root.id, r1.seq).await;
    app.follow(&c.dave, root.id, true).await;
    let marker = app.register("marker").await;

    for (who, live) in [
        (&c.alice, &mut alice_live),
        (&c.carol, &mut carol_live),
        (&c.dave, &mut dave_live),
    ] {
        let seen = live.events_until_marker(marker.user.id).await;
        let mut replayed = replay_from(app, who, cursor).await;
        let cut = replayed
            .iter()
            .position(
                |e| matches!(&e.event, Event::MemberJoined { user } if user.id == marker.user.id),
            )
            .unwrap_or(replayed.len());
        replayed.truncate(cut);
        assert_eq!(seen, replayed, "{}", who.user.username);
        assert!(replayed.windows(2).all(|w| w[0].seq < w[1].seq));
        // Only their own thread states, never anyone else's.
        let states = thread_states_in(&replayed);
        assert!(states.iter().all(|s| s.root_id == root.id));
    }
    // Dave sees no state but his own choice; carol sees hers (started by the mention, moved by
    // reading, and started again as a replier).
    assert_eq!(
        thread_states_in(&replay_from(app, &c.dave, cursor).await).len(),
        1
    );
    let carol_states = thread_states_in(&replay_from(app, &c.carol, cursor).await);
    assert!(
        !carol_states.is_empty()
            && carol_states
                .iter()
                .all(|s| s.following || s.last_read > EventSeq::ZERO)
    );
    check_thread_log(&replay_from(app, &c.dave, cursor).await, root.id);
    finish(c).await;
}

#[sqlx::test]
async fn a_member_removed_while_away_gets_nothing_more_about_a_thread(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let hush = app.create_channel(&c.alice, "hush", "private").await;
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.bob, &c.carol])
            .await
            .status()
            .is_success()
    );
    let root = app.send(&c.alice, hush.id, "private root").await;
    let first = app
        .reply_mentioning(
            &c.bob,
            &root,
            "@carol first",
            &[("carol", user(&c.carol))],
            false,
        )
        .await;
    assert!(app.thread_state(&c.carol, root.id).await.is_some());
    let cursor = app.head().await;

    // Carol is away; she is removed; the thread carries on.
    assert!(
        app.remove_member(&c.alice, hush.id, &c.carol)
            .await
            .status()
            .is_success()
    );
    app.reply(&c.bob, &root, "second").await;
    app.reply_mentioning(
        &c.alice,
        &root,
        "third @carol",
        &[("carol", user(&c.carol))],
        false,
    )
    .await;

    let replayed = replay_from(app, &c.carol, cursor).await;
    assert!(
        replayed
            .iter()
            .all(|e| matches!(&e.event, Event::MemberRemoved { .. })),
        "only her own removal: {:?}",
        kinds(&replayed)
    );
    // Everything about the thread is closed to her, and her state for it is gone.
    denied(
        app.replies_raw(&c.carol, root.id, &[]).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.follow_raw(&c.carol, root.id, true).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.read_thread_raw(&c.carol, root.id, first.seq).await,
        ErrorCode::NotFound,
    )
    .await;
    denied(
        app.reply_raw(
            &c.carol,
            &root,
            OperationId::generate(),
            lazy_domain::MessageDocument::from_plain_text("x").unwrap(),
            false,
        )
        .await,
        ErrorCode::NotFound,
    )
    .await;
    assert!(app.threads_of(&c.carol).await.is_empty());
    let states: i64 =
        sqlx::query_scalar("SELECT count(*) FROM thread_user_state WHERE user_id = $1")
            .bind(c.carol.user.id.as_uuid())
            .fetch_one(&app.pool)
            .await
            .unwrap();
    assert_eq!(states, 0);
    // Someone still in it hears all of it.
    let bob_events = replay_from(app, &c.bob, cursor).await;
    assert_eq!(
        bob_events
            .iter()
            .filter(|e| matches!(e.event, Event::ThreadChanged { .. }))
            .count(),
        2
    );

    // Adding her back starts her fresh: no state, and the mention that named her is not unread.
    assert!(
        app.add_members(&c.alice, hush.id, &[&c.carol])
            .await
            .status()
            .is_success()
    );
    assert!(app.thread_state(&c.carol, root.id).await.is_none());
    assert_eq!(
        app.replies_ok(&c.carol, root.id, &[]).await.messages.len(),
        3
    );

    // In a public channel someone who leaves can still read the thread but not change their state.
    let team = app.create_channel(&c.alice, "team", "public").await;
    assert!(app.act(&c.bob, team.id, "join").await.status().is_success());
    let team_root = app.send(&c.alice, team.id, "public root").await;
    app.reply(&c.bob, &team_root, "hello").await;
    assert!(
        app.act(&c.bob, team.id, "leave")
            .await
            .status()
            .is_success()
    );
    assert_eq!(
        app.replies_ok(&c.bob, team_root.id, &[])
            .await
            .messages
            .len(),
        1
    );
    denied(
        app.follow_raw(&c.bob, team_root.id, true).await,
        ErrorCode::Forbidden,
    )
    .await;
    assert!(app.thread_state(&c.bob, team_root.id).await.is_none());
    finish(c).await;
}

#[sqlx::test]
async fn the_bootstrap_lists_followed_and_unread_threads_newest_first_up_to_five_hundred(
    pool: PgPool,
) {
    let c = chat(pool).await;
    let app = &c.app;
    let team = app.create_channel(&c.alice, "team", "public").await;
    // 600 threads, each one reply by alice, all followed by dave with nothing read; seeded directly,
    // at positions no real event will reach.
    let base: i64 = 1_000_000;
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at) \
         SELECT ('20000000-0000-7000-8000-' || lpad(to_hex(n), 12, '0'))::uuid, $1, $2, $3::jsonb, 'x', 1, $4 + n * 2, now() \
         FROM generate_series(1, 600) n",
    )
    .bind(app.general.as_uuid())
    .bind(c.alice.user.id.as_uuid())
    .bind(serde_json::json!({"version": 2, "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": "x"}]}]}))
    .bind(base)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO messages (id, conversation_id, author_id, document, plain_text, revision, seq, created_at, \
                               thread_root_id, listed) \
         SELECT ('30000000-0000-7000-8000-' || lpad(to_hex(n), 12, '0'))::uuid, $1, $2, $3::jsonb, 'x', 1, $4 + n * 2 + 1, now(), \
                ('20000000-0000-7000-8000-' || lpad(to_hex(n), 12, '0'))::uuid, false \
         FROM generate_series(1, 600) n",
    )
    .bind(app.general.as_uuid())
    .bind(c.alice.user.id.as_uuid())
    .bind(serde_json::json!({"version": 2, "blocks": [{"type": "paragraph", "inlines": [{"type": "text", "text": "x"}]}]}))
    .bind(base)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO threads (root_id, conversation_id, reply_count, last_reply_seq, last_reply_at, recent_repliers, as_of) \
         SELECT ('20000000-0000-7000-8000-' || lpad(to_hex(n), 12, '0'))::uuid, $1, 1, $3 + n * 2 + 1, now(), ARRAY[$2::uuid], $3 + n * 2 + 1 \
         FROM generate_series(1, 600) n",
    )
    .bind(app.general.as_uuid())
    .bind(c.alice.user.id.as_uuid())
    .bind(base)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, last_read) \
         SELECT $2, ('20000000-0000-7000-8000-' || lpad(to_hex(n), 12, '0'))::uuid, $1, true, 0 FROM generate_series(1, 600) n",
    )
    .bind(app.general.as_uuid())
    .bind(c.dave.user.id.as_uuid())
    .execute(&app.pool)
    .await
    .unwrap();

    let listed = app.threads_of(&c.dave).await;
    assert_eq!(listed.len(), 500);
    // Newest activity first: roots 600 down to 101.
    let expected: Vec<MessageId> = (101..=600u64)
        .rev()
        .map(|n| format!("20000000-0000-7000-8000-{n:012x}").parse().unwrap())
        .collect();
    assert_eq!(
        listed.iter().map(|s| s.root_id).collect::<Vec<_>>(),
        expected
    );
    assert!(listed.iter().all(|s| s.following
        && s.unread_count == 1
        && s.mention_count == 0
        && s.conversation_id == app.general));

    // A thread that is not followed is listed while it has unread replies and not once it has none;
    // one in a conversation the person is not in is not listed at all.
    let loose: MessageId = format!("20000000-0000-7000-8000-{:012x}", 50)
        .parse()
        .unwrap();
    let rows = |following: bool, last_read: i64| {
        let (pool, dave, loose) = (app.pool.clone(), c.dave.user.id, loose);
        async move {
            sqlx::query("DELETE FROM thread_user_state WHERE user_id = $1 AND root_id = $2")
                .bind(dave.as_uuid())
                .bind(loose.as_uuid())
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO thread_user_state (user_id, root_id, conversation_id, following, unfollowed, last_read) VALUES ($1, $2, (SELECT conversation_id FROM messages WHERE id = $2), $3, NOT $3, $4)")
                .bind(dave.as_uuid()).bind(loose.as_uuid()).bind(following).bind(last_read).execute(&pool).await.unwrap();
        }
    };
    // Not followed, nothing read: listed (it ranks below the 500 newest only by activity, so it
    // competes for a place; make it newest to see it).
    sqlx::query("UPDATE threads SET last_reply_seq = $2, as_of = $2 WHERE root_id = $1")
        .bind(loose.as_uuid())
        .bind(base + 5000)
        .execute(&app.pool)
        .await
        .unwrap();
    rows(false, 0).await;
    let listed = app.threads_of(&c.dave).await;
    assert_eq!(listed.len(), 500);
    let state = listed
        .iter()
        .find(|s| s.root_id == loose)
        .expect("unread replies, so listed");
    assert!(!state.following && state.unread_count == 1);
    rows(false, base + 101).await;
    assert!(
        app.thread_state(&c.dave, loose).await.is_none(),
        "nothing unread and not followed"
    );
    assert!(team.id != app.general);
    finish(c).await;
}

// --- Races -------------------------------------------------------------------------------------

#[sqlx::test]
async fn many_repliers_at_once_leave_an_exact_summary_and_a_gapless_log(pool: PgPool) {
    const PEOPLE: usize = 12;
    const EACH: usize = 4;
    let c = chat(pool).await;
    let app = &c.app;
    let mut people = vec![
        c.alice.clone(),
        c.bob.clone(),
        c.carol.clone(),
        c.dave.clone(),
    ];
    for n in 0..PEOPLE - 4 {
        people.push(app.register(&format!("person{n}")).await);
    }
    let root = app.send(&c.owner, app.general, "root").await;
    let cursor = app.head().await;

    let replies = join_all(people.iter().map(|person| {
        let (app, person, root) = (&c.app, person.clone(), root.clone());
        async move {
            let mut mine = Vec::new();
            for n in 0..EACH {
                mine.push(
                    app.reply(&person, &root, &format!("{}-{n}", person.user.username))
                        .await,
                );
            }
            mine
        }
    }))
    .await;
    let total = PEOPLE * EACH;
    let all: Vec<&Message> = replies.iter().flatten().collect();
    assert_eq!(all.len(), total);

    // The summary is exactly right, and agrees with the table of replies.
    let summary = app.get_message(&c.owner, root.id).await.thread.unwrap();
    assert_eq!(summary.reply_count as usize, total);
    assert_eq!(stored_replies(app, root.id).await as usize, total);
    let newest = all.iter().max_by_key(|m| m.seq).unwrap();
    assert_eq!(summary.last_reply_seq, newest.seq);
    let mut by_seq = all.clone();
    by_seq.sort_by_key(|m| std::cmp::Reverse(m.seq));
    let mut expected: Vec<UserId> = Vec::new();
    for message in by_seq {
        if !expected.contains(&message.author_id) && expected.len() < 5 {
            expected.push(message.author_id);
        }
    }
    assert_eq!(summary.recent_repliers, expected);
    // The log has no hole, one reply and one summary per reply, the summary right behind it, and
    // every announced summary is the count so far.
    let positions = app.log_positions_after(cursor).await;
    assert!(positions.windows(2).all(|w| w[1] == w[0] + 1), "gapless");
    let events = replay_from(app, &c.owner, cursor).await;
    let created: Vec<&EventEnvelope> = events
        .iter()
        .filter(|e| matches!(&e.event, Event::MessageCreated { .. }))
        .collect();
    let changed: Vec<&EventEnvelope> = events
        .iter()
        .filter(|e| matches!(&e.event, Event::ThreadChanged { .. }))
        .collect();
    assert_eq!((created.len(), changed.len()), (total, total));
    for reply in created {
        assert!(
            changed.iter().any(|e| e.seq.get() > reply.seq.get()
                && summaries_of(&[(*e).clone()], root.id).len() == 1),
            "{}",
            reply.seq
        );
        let right_behind = events
            .iter()
            .find(|e| {
                e.seq.get() > reply.seq.get() && matches!(e.event, Event::ThreadChanged { .. })
            })
            .unwrap();
        assert!(right_behind.seq.get() <= reply.seq.get() + 1 + people.len() as i64 + 1);
    }
    check_thread_log(&events, root.id);
    // Each person follows once, and each reply's author has read to their own newest reply.
    for person in &people {
        let state = app
            .thread_state(person, root.id)
            .await
            .expect("every replier follows");
        let mine = replies[people
            .iter()
            .position(|p| p.user.id == person.user.id)
            .unwrap()]
        .iter()
        .map(|m| m.seq)
        .max()
        .unwrap();
        assert!(
            state.following && state.last_read == mine,
            "{}",
            person.user.username
        );
    }

    // The same operation id fired many times at once makes one reply.
    let operation = OperationId::generate();
    let document = || lazy_domain::MessageDocument::from_plain_text("exactly once").unwrap();
    let results = join_all((0..8).map(|_| {
        let (app, root) = (&c.app, root.clone());
        let bob = c.bob.clone();
        async move {
            app.reply_raw(&bob, &root, operation, document(), false)
                .await
        }
    }))
    .await;
    let mut seen = std::collections::HashSet::new();
    for response in results {
        assert!(response.status().is_success());
        seen.insert(
            response
                .json::<lazy_protocol::SendMessageResponse>()
                .await
                .unwrap()
                .message
                .id,
        );
    }
    assert_eq!(seen.len(), 1);
    assert_eq!(stored_replies(app, root.id).await as usize, total + 1);
    assert_eq!(
        app.get_message(&c.owner, root.id)
            .await
            .thread
            .unwrap()
            .reply_count as usize,
        total + 1
    );
    finish(c).await;
}

#[sqlx::test]
async fn replies_racing_a_root_deletion_either_land_before_it_or_are_refused(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..10 {
        let root = app
            .send(&c.alice, app.general, &format!("root {round}"))
            .await;
        let cursor = app.head().await;
        let repliers = [
            &c.bob, &c.carol, &c.dave, &c.boss, &c.owner, &c.bob, &c.carol, &c.dave,
        ];
        let (deleted, results) = tokio::join!(
            async { app.delete_raw(&c.alice, root.id).await },
            join_all(repliers.iter().enumerate().map(|(n, who)| {
                let (app, who, root) = (&c.app, (*who).clone(), root.clone());
                async move {
                    let document =
                        lazy_domain::MessageDocument::from_plain_text(&format!("r{n}")).unwrap();
                    app.reply_raw(&who, &root, OperationId::generate(), document, n % 2 == 0)
                        .await
                }
            }))
        );
        assert!(deleted.status().is_success());
        let mut landed = 0usize;
        for response in results {
            match response.status().as_u16() {
                200 => landed += 1,
                404 => {}
                other => panic!("round {round}: {other}"),
            }
        }
        // What landed is all there, summarized exactly, and the tombstone stands while it does.
        assert_eq!(
            stored_replies(app, root.id).await as usize,
            landed,
            "round {round}"
        );
        let tombstone = app.get_message(&c.alice, root.id).await;
        assert!(tombstone.deleted);
        assert_eq!(
            tombstone.thread.map_or(0, |t| t.reply_count as usize),
            landed
        );
        let listed = history(app, &c.carol, app.general)
            .await
            .messages
            .iter()
            .any(|m| m.id == root.id);
        assert_eq!(listed, landed > 0, "round {round}");
        check_thread_log(&replay_from(app, &c.alice, cursor).await, root.id);
        let log = app.log_positions_after(cursor).await;
        assert!(log.windows(2).all(|w| w[1] == w[0] + 1));
    }
    finish(c).await;
}

#[sqlx::test]
async fn replies_racing_the_deletion_of_the_last_reply_keep_the_summary_exact(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    for round in 0..10 {
        let root = app
            .send(&c.alice, app.general, &format!("root {round}"))
            .await;
        let only = app.reply(&c.bob, &root, "the only reply").await;
        // A tombstoned root too, in half the rounds: then the last deletion also removes it.
        if round % 2 == 1 {
            app.delete(&c.alice, root.id).await;
        }
        let cursor = app.head().await;
        let newcomers = [&c.carol, &c.dave, &c.boss];
        let (deleted, results) = tokio::join!(
            async { app.delete_raw(&c.bob, only.id).await },
            join_all(newcomers.iter().map(|who| {
                let (app, who, root) = (&c.app, (*who).clone(), root.clone());
                async move {
                    let document = lazy_domain::MessageDocument::from_plain_text("new").unwrap();
                    app.reply_raw(&who, &root, OperationId::generate(), document, false)
                        .await
                }
            }))
        );
        assert!(deleted.status().is_success());
        let landed = results.iter().filter(|r| r.status().is_success()).count();
        if round % 2 == 0 {
            assert_eq!(landed, 3, "a live root takes every reply");
        }
        let now = app.get_message(&c.alice, root.id).await;
        assert_eq!(
            stored_replies(app, root.id).await as usize,
            landed,
            "round {round}"
        );
        assert_eq!(
            now.thread.as_ref().map_or(0, |t| t.reply_count as usize),
            landed,
            "round {round}"
        );
        let listed = history(app, &c.carol, app.general)
            .await
            .messages
            .iter()
            .any(|m| m.id == root.id);
        assert_eq!(listed, !now.deleted || landed > 0, "round {round}");
        let all = replay_from(app, &c.alice, cursor).await;
        // The log from the race alone starts from one reply, so check it against that start.
        let mut count = 1u32;
        for envelope in &all {
            match &envelope.event {
                Event::MessageCreated { message, .. } if message.thread_root == Some(root.id) => {
                    count += 1
                }
                Event::MessageDeleted { message, .. } if message.thread_root == Some(root.id) => {
                    count -= 1
                }
                Event::ThreadChanged {
                    root_id, summary, ..
                } if *root_id == root.id => {
                    assert_eq!(
                        summary.as_ref().map_or(0, |s| s.reply_count),
                        count,
                        "round {round} at {}",
                        envelope.seq
                    );
                }
                _ => {}
            }
        }
        assert_eq!(count as usize, landed);
    }
    finish(c).await;
}

/// Holds a thread's root row the way a transaction in the middle of an edit or deletion of it
/// would, so that the others can be shown to wait for it.
async fn hold_root(app: &App, root: MessageId) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut holder = app.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM messages WHERE id = $1 FOR NO KEY UPDATE")
        .bind(root.as_uuid())
        .fetch_one(&mut *holder)
        .await
        .unwrap();
    holder
}

/// The commands that touch a thread take its root's row before anything is written, so they take
/// turns with whoever holds it. The test holds the row itself, so the outcome never depends on
/// timing: each command must stay blocked, and then see what the holder committed.
#[sqlx::test]
async fn a_reply_waits_for_its_root_and_then_sees_what_the_holder_did(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let holder = hold_root(app, root.id).await;

    let (http, url, token) = (
        app.http.clone(),
        app.url(&lazy_protocol::api::paths::messages(app.general)),
        c.bob.token.clone(),
    );
    let body = serde_json::json!({
        "operation_id": OperationId::generate(),
        "document": lazy_domain::MessageDocument::from_plain_text("late").unwrap(),
        "thread_root": root.id,
    });
    let mut reply = tokio::spawn(async move {
        http.post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(600), &mut reply)
            .await
            .is_err(),
        "a reply should wait for its root's row"
    );
    // The holder deletes the root and commits; the reply that was waiting finds it gone.
    let mut holder = holder;
    sqlx::query("UPDATE messages SET deleted = true, listed = false WHERE id = $1")
        .bind(root.id.as_uuid())
        .execute(&mut *holder)
        .await
        .unwrap();
    holder.commit().await.unwrap();
    let response = tokio::time::timeout(DEADLINE, reply)
        .await
        .expect("it finishes")
        .unwrap();
    denied(response, ErrorCode::NotFound).await;
    assert_eq!(stored_replies(app, root.id).await, 0);
    finish(c).await;
}

#[sqlx::test]
async fn deleting_a_reply_waits_for_its_root(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let reply = app.reply(&c.bob, &root, "mine").await;
    let holder = hold_root(app, root.id).await;

    let (http, url, token) = (
        app.http.clone(),
        app.url(&lazy_protocol::api::paths::message(reply.id)),
        c.bob.token.clone(),
    );
    let mut deletion =
        tokio::spawn(async move { http.delete(url).bearer_auth(token).send().await.unwrap() });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(600), &mut deletion)
            .await
            .is_err(),
        "deleting a reply should wait for its root's row"
    );
    holder.commit().await.unwrap();
    let response = tokio::time::timeout(DEADLINE, deletion)
        .await
        .expect("it finishes")
        .unwrap();
    assert!(response.status().is_success());
    assert!(app.get_message(&c.alice, root.id).await.thread.is_none());
    finish(c).await;
}

#[sqlx::test]
async fn following_and_reading_wait_for_the_thread_root(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    let root = app.send(&c.alice, app.general, "root").await;
    let reply = app.reply(&c.bob, &root, "one").await;
    let holder = hold_root(app, root.id).await;

    let follow = {
        let (http, url, token) = (
            app.http.clone(),
            app.url(&lazy_protocol::api::paths::thread_action(root.id, "follow")),
            c.carol.token.clone(),
        );
        tokio::spawn(async move {
            http.put(url)
                .bearer_auth(token)
                .json(&serde_json::json!({"following": true}))
                .send()
                .await
                .unwrap()
        })
    };
    let read = {
        let (http, url, token) = (
            app.http.clone(),
            app.url(&lazy_protocol::api::paths::thread_action(root.id, "read")),
            c.alice.token.clone(),
        );
        let body = serde_json::json!({"up_to": reply.seq});
        tokio::spawn(async move {
            http.post(url)
                .bearer_auth(token)
                .json(&body)
                .send()
                .await
                .unwrap()
        })
    };
    let (mut follow, mut read) = (follow, read);
    for (name, task) in [("follow", &mut follow), ("read", &mut read)] {
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(600), task)
                .await
                .is_err(),
            "{name} should wait for the root's row"
        );
    }
    holder.commit().await.unwrap();
    for task in [follow, read] {
        let response = tokio::time::timeout(DEADLINE, task)
            .await
            .expect("it finishes")
            .unwrap();
        assert!(response.status().is_success());
    }
    finish(c).await;
}

#[sqlx::test]
async fn the_server_reports_a_protocol_version_that_has_threads(pool: PgPool) {
    let c = chat(pool).await;
    let app = &c.app;
    const { assert!(PROTOCOL_VERSION >= 6) };
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
