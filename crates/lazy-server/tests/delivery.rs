//! Who receives which events, evaluated against current membership: removal, revocation while
//! connected, late joiners, and personal state. These are the 2.7 hardening cases.

mod common;

use common::*;
use lazy_domain::{ConversationId, EventSeq, UserId};
use lazy_protocol::{ConversationUserState, ErrorCode, Event, EventEnvelope, ServerFrame};
use sqlx::PgPool;

async fn denied(response: reqwest::Response, code: ErrorCode) {
    let (status, error) = api_error(response).await;
    assert_error(status, &error, code);
}

/// Connects a socket and subscribes after `cursor`, returning it with the replay it received.
async fn resume(app: &App, who: &Account, cursor: EventSeq) -> (Ws, Vec<EventEnvelope>, EventSeq) {
    let mut ws = app.connect_ws(who).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let (events, caught_up) = ws.replay().await;
    (ws, events, caught_up)
}

fn about(event: &Event) -> Option<ConversationId> {
    match event {
        Event::MessageCreated { message, .. } => Some(message.conversation_id),
        Event::ConversationCreated { conversation }
        | Event::ConversationUpdated { conversation }
        | Event::MemberAdded { conversation, .. } => Some(conversation.id),
        Event::MemberRemoved {
            conversation_id, ..
        } => Some(*conversation_id),
        Event::UserStateChanged { state } => Some(state.conversation_id),
        Event::MemberJoined { .. } => None,
    }
}

/// The events before `marker` joined. A replay that starts after the marker registered already
/// contains the marker's own arrival, so it is cut there rather than waited for.
fn before_marker(events: Vec<EventEnvelope>, marker: UserId) -> Vec<EventEnvelope> {
    events
        .into_iter()
        .take_while(|e| !matches!(&e.event, Event::MemberJoined { user } if user.id == marker))
        .collect()
}

fn texts(events: &[EventEnvelope]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.event {
            Event::MessageCreated { message, .. } => Some(message.document.plain_text()),
            _ => None,
        })
        .collect()
}

fn is_removal_of(event: &Event, conversation: ConversationId, user: UserId) -> bool {
    matches!(event, Event::MemberRemoved { conversation_id, user_id }
        if *conversation_id == conversation && *user_id == user)
}

/// ada (owner) and a private channel with grace and hopper in it.
struct Room {
    app: App,
    ada: Account,
    grace: Account,
    hopper: Account,
    secret: ConversationId,
}

async fn room(pool: PgPool) -> Room {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let hopper = app.register("hopper").await;
    let secret = app.create_channel(&ada, "secret", "private").await.id;
    let response = app.add_members(&ada, secret, &[&grace, &hopper]).await;
    assert!(response.status().is_success());
    Room {
        app,
        ada,
        grace,
        hopper,
        secret,
    }
}

/// Someone removed while disconnected comes back with the cursor they left with. They learn of
/// the removal and nothing about the channel from before or after it; another member's device
/// still receives everything.
#[sqlx::test]
async fn a_removed_person_replaying_an_old_cursor_gets_only_their_removal(pool: PgPool) {
    let Room {
        app,
        ada,
        grace,
        hopper,
        secret,
    } = room(pool).await;

    // Grace takes everything so far and disconnects; hopper stays connected on another device.
    let (grace_ws, _, cursor) = resume(&app, &grace, EventSeq::ZERO).await;
    drop(grace_ws);
    let mut hopper_live = {
        let (ws, _, _) = resume(&app, &hopper, cursor).await;
        ws
    };

    // While grace is away: an event she did receive before (a message), then her removal, then
    // more messages in the channel, and one in #general that she still belongs to.
    app.send(&ada, secret, "while she was still in").await;
    assert!(
        app.remove_member(&ada, secret, &grace)
            .await
            .status()
            .is_success()
    );
    for text in ["after one", "after two", "after three"] {
        app.send(&ada, secret, text).await;
    }
    app.send(&ada, app.general, "public news").await;
    let marker = app.register("marker").await;

    // Grace reconnects with the OLD cursor.
    let (_grace_back, replay, _) = resume(&app, &grace, cursor).await;
    let hers = before_marker(replay, marker.user.id);
    let in_secret: Vec<&EventEnvelope> = hers
        .iter()
        .filter(|e| about(&e.event) == Some(secret))
        .collect();
    assert_eq!(
        in_secret.len(),
        1,
        "only her removal concerns the secret channel"
    );
    assert!(is_removal_of(&in_secret[0].event, secret, grace.user.id));
    assert!(
        texts(&hers).contains(&"public news".to_owned()),
        "#general is still hers"
    );
    assert!(
        !texts(&hers)
            .iter()
            .any(|t| t.contains("after") || t.contains("still in"))
    );
    // Its history is not hers to read either.
    denied(app.history(&grace, secret, &[]).await, ErrorCode::NotFound).await;
    assert!(
        !app.conversations_of(&grace)
            .await
            .iter()
            .any(|c| c.id == secret)
    );

    // Hopper's live device received everything, in order; a fresh device replaying the same
    // cursor receives exactly the same.
    let live = hopper_live.events_until_marker(marker.user.id).await;
    let (_hopper_fresh, fresh, _) = resume(&app, &hopper, cursor).await;
    let fresh = before_marker(fresh, marker.user.id);
    assert_eq!(
        live.iter().map(|e| e.seq).collect::<Vec<_>>(),
        fresh.iter().map(|e| e.seq).collect::<Vec<_>>()
    );
    assert_eq!(
        texts(&live),
        vec![
            "while she was still in",
            "after one",
            "after two",
            "after three",
            "public news"
        ]
    );
    assert!(
        live.iter()
            .any(|e| is_removal_of(&e.event, secret, grace.user.id))
    );
    app.server.shutdown().await.unwrap();
}

/// Removal while connected stops delivery at once, including events committed after the removal
/// that are still on their way.
#[sqlx::test]
async fn removal_while_connected_stops_delivery_immediately(pool: PgPool) {
    let Room {
        app,
        ada,
        grace,
        secret,
        ..
    } = room(pool).await;
    let (mut grace_ws, _, _) = resume(&app, &grace, app.head().await).await;

    // Delivery is live: she receives a message in the channel.
    app.send(&ada, secret, "visible").await;
    match grace_ws.next().await {
        ServerFrame::Event(envelope) => assert_eq!(texts(&[envelope]), vec!["visible"]),
        other => panic!("unexpected frame {other:?}"),
    }

    assert!(
        app.remove_member(&ada, secret, &grace)
            .await
            .status()
            .is_success()
    );
    app.send(&ada, secret, "hidden one").await;
    app.send(&ada, secret, "hidden two").await;
    app.send(&ada, app.general, "still visible").await;
    let marker = app.register("marker").await;
    let events = grace_ws.events_until_marker(marker.user.id).await;

    assert!(
        events
            .iter()
            .any(|e| is_removal_of(&e.event, secret, grace.user.id))
    );
    assert!(texts(&events).contains(&"still visible".to_owned()));
    assert!(
        !texts(&events).iter().any(|t| t.starts_with("hidden")),
        "nothing from the channel after her removal: {:?}",
        texts(&events)
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| about(&e.event) == Some(secret))
            .count(),
        1
    );
    app.server.shutdown().await.unwrap();
}

/// Leaving works the same way as being removed.
#[sqlx::test]
async fn leaving_a_channel_ends_delivery_for_the_leaver(pool: PgPool) {
    let Room {
        app,
        ada,
        grace,
        secret,
        ..
    } = room(pool).await;
    let (mut grace_ws, _, _) = resume(&app, &grace, app.head().await).await;
    assert!(app.act(&grace, secret, "leave").await.status().is_success());
    app.send(&ada, secret, "you left").await;
    let marker = app.register("marker").await;
    let events = grace_ws.events_until_marker(marker.user.id).await;
    assert_eq!(events.len(), 1);
    assert!(is_removal_of(&events[0].event, secret, grace.user.id));
    app.server.shutdown().await.unwrap();
}

/// Membership is evaluated when events are delivered, so someone who joins later receives the
/// channel's older events when they replay past them.
#[sqlx::test]
async fn a_late_joiner_replays_a_channels_older_events(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let hopper = app.register("hopper").await;
    let before_channel = app.head().await;
    let team = app.create_channel(&ada, "team", "public").await;
    for text in ["first", "second", "third"] {
        app.send(&ada, team.id, text).await;
    }

    // Not a member yet: a stream from before the channel existed shows none of it.
    let (mut outside_live, outside_replay, _) = resume(&app, &hopper, before_channel).await;
    assert!(
        outside_replay
            .iter()
            .all(|e| about(&e.event) != Some(team.id))
    );

    app.act(&hopper, team.id, "join").await;
    let marker = app.register("marker").await;
    // The device that was already connected hears the join, and none of the history it was
    // already past (the client fetches history on MemberAdded).
    let live = outside_live.events_until_marker(marker.user.id).await;
    let live_kinds: Vec<_> = live
        .iter()
        .filter(|e| about(&e.event) == Some(team.id))
        .collect();
    assert_eq!(live_kinds.len(), 1);
    assert!(matches!(live_kinds[0].event, Event::MemberAdded { .. }));

    // A device replaying from before the channel existed receives the older events too, in order.
    let (_replaying, replay, _) = resume(&app, &hopper, before_channel).await;
    let replay = before_marker(replay, marker.user.id);
    let team_events: Vec<&EventEnvelope> = replay
        .iter()
        .filter(|e| about(&e.event) == Some(team.id))
        .collect();
    let kinds: Vec<&str> = team_events
        .iter()
        .map(|e| match e.event {
            Event::ConversationCreated { .. } => "created",
            Event::MessageCreated { .. } => "message",
            Event::MemberAdded { .. } => "added",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec!["created", "message", "message", "message", "added"]
    );
    assert!(team_events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    app.server.shutdown().await.unwrap();
}

/// Personal state reaches its owner, and stops reaching them once they leave the conversation.
#[sqlx::test]
async fn personal_state_events_follow_the_owners_membership(pool: PgPool) {
    let Room {
        app,
        ada,
        grace,
        secret,
        ..
    } = room(pool).await;
    let cursor = app.head().await;
    app.send(&ada, secret, "read me").await;
    let newest = app
        .history_ok(&grace, secret, &[])
        .await
        .messages
        .last()
        .unwrap()
        .seq;
    assert!(
        app.mark_read(&grace, secret, newest.get())
            .await
            .status()
            .is_success()
    );
    assert!(
        app.favorite(&grace, secret, true)
            .await
            .status()
            .is_success()
    );

    // While she is a member, replaying from before shows her two state changes, and ada none.
    let state_changes = |events: &[EventEnvelope]| -> Vec<ConversationUserState> {
        events
            .iter()
            .filter_map(|e| match &e.event {
                Event::UserStateChanged { state } => Some(state.clone()),
                _ => None,
            })
            .collect()
    };
    let (_, hers, _) = resume(&app, &grace, cursor).await;
    assert_eq!(state_changes(&hers).len(), 2);
    let (_, ada_replay, _) = resume(&app, &ada, cursor).await;
    assert!(state_changes(&ada_replay).is_empty());

    // After removal she no longer receives state for the conversation, even old state.
    assert!(
        app.remove_member(&ada, secret, &grace)
            .await
            .status()
            .is_success()
    );
    let (_, after, _) = resume(&app, &grace, cursor).await;
    assert!(
        state_changes(&after).is_empty(),
        "no state for a conversation she is not in"
    );
    assert_eq!(
        after
            .iter()
            .filter(|e| about(&e.event) == Some(secret))
            .count(),
        1
    );
    app.server.shutdown().await.unwrap();
}

/// A person removed and added back receives the channel's full history again, in order.
#[sqlx::test]
async fn being_added_back_restores_everything_in_order(pool: PgPool) {
    let Room {
        app,
        ada,
        grace,
        secret,
        ..
    } = room(pool).await;
    let cursor = app.head().await;
    app.send(&ada, secret, "one").await;
    assert!(
        app.remove_member(&ada, secret, &grace)
            .await
            .status()
            .is_success()
    );
    app.send(&ada, secret, "two").await;
    assert!(
        app.add_members(&ada, secret, &[&grace])
            .await
            .status()
            .is_success()
    );
    app.send(&ada, secret, "three").await;
    let marker = app.register("marker").await;

    let (_ws, events, _) = resume(&app, &grace, cursor).await;
    let events = before_marker(events, marker.user.id);
    // She is a member again, so she receives all of it, her own removal included, in order.
    assert_eq!(texts(&events), vec!["one", "two", "three"]);
    assert!(events.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    assert!(
        events
            .iter()
            .any(|e| is_removal_of(&e.event, secret, grace.user.id))
    );
    app.server.shutdown().await.unwrap();
}

/// User-audience events are pruned like any other: a prefix goes, the kept range still delivers
/// to the right person, and an overtaken cursor is told to take a snapshot.
#[sqlx::test]
async fn pruning_handles_personal_events(pool: PgPool) {
    use lazy_server::pruner::prune_all;
    use tokio_util::sync::CancellationToken;

    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let first = app.send(&ada, app.general, "m1").await;
    assert!(
        app.mark_read(&grace, app.general, first.seq.get())
            .await
            .status()
            .is_success()
    );
    for n in 0..6 {
        app.send(&ada, app.general, &format!("later {n}")).await;
    }
    assert!(
        app.favorite(&grace, app.general, true)
            .await
            .status()
            .is_success()
    );
    let head = app.head().await;
    app.backdate_events(head, 40).await;

    prune_all(&app.pool, retention(4), &CancellationToken::new())
        .await
        .unwrap();
    let floor = app.event_floor().await;
    assert_eq!(floor.get(), head.get() - 4);
    let personal: i64 = sqlx::query_scalar("SELECT count(*) FROM events WHERE audience = 'user'")
        .fetch_one(&app.pool)
        .await
        .unwrap();
    assert_eq!(
        personal, 1,
        "the old personal event went with the prefix; the newer one stayed"
    );

    // From the floor, grace receives her surviving personal event and ada does not.
    let (_, hers, _) = resume(&app, &grace, floor).await;
    assert!(
        hers.iter()
            .any(|e| matches!(e.event, Event::UserStateChanged { .. }))
    );
    let (_, ada_events, _) = resume(&app, &ada, floor).await;
    assert!(
        ada_events
            .iter()
            .all(|e| !matches!(e.event, Event::UserStateChanged { .. }))
    );

    // From before the floor, both are told to start again.
    let mut stale = app.connect_ws(&grace).await;
    stale.subscribe(app.workspace_id, EventSeq::ZERO).await;
    assert!(matches!(
        stale.next().await,
        ServerFrame::SnapshotRequired { .. }
    ));
    app.server.shutdown().await.unwrap();
}
