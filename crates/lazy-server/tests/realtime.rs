mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use lazy_domain::{EventSeq, MessageDocument, OperationId, WorkspaceId};
use lazy_protocol::{ErrorCode, Event, SendMessageRequest, ServerFrame, SnapshotReason};
use lazy_server::pruner::prune_all;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

fn message_text(event: &lazy_protocol::EventEnvelope) -> String {
    match &event.event {
        Event::MessageCreated { message, .. } => message.document.plain_text(),
        other => panic!("expected a message event, got {other:?}"),
    }
}

#[sqlx::test]
async fn subscribers_get_replay_then_caught_up_then_live_events(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    for text in ["one", "two", "three"] {
        app.send(&ada, app.general, text).await;
    }
    let grace = app.register("grace").await;

    let mut ws = app.connect_ws(&grace).await;
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (replayed, caught_up) = ws.replay().await;
    // ada joined the workspace (1) and #general (2), three messages (3..=5), then grace did
    // both (6, 7). Grace is a member of #general now, so she receives all of it.
    let positions: Vec<i64> = replayed.iter().map(|e| e.seq.get()).collect();
    assert_eq!(positions, vec![1, 2, 3, 4, 5, 6, 7]);
    assert_eq!(caught_up.get(), 7);
    assert!(matches!(replayed[0].event, Event::MemberJoined { .. }));
    assert!(matches!(replayed[1].event, Event::MemberAdded { .. }));
    assert_eq!(message_text(&replayed[2]), "one");
    assert_eq!(message_text(&replayed[4]), "three");

    // Live: a message, with the sender's operation id so its sender can reconcile.
    let operation = OperationId::generate();
    let sent = app.send_raw(&ada, app.general, operation, "live one").await;
    assert!(sent.status().is_success());
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), 8);
            assert_eq!(envelope.workspace_id, app.workspace_id);
            match envelope.event {
                Event::MessageCreated {
                    message,
                    operation_id,
                } => {
                    assert_eq!(message.document.plain_text(), "live one");
                    assert_eq!(operation_id, Some(operation));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        other => panic!("unexpected frame {other:?}"),
    }

    // Live: workspace-wide events reach members too.
    let hopper = app.register("hopper").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), 9);
            match envelope.event {
                Event::MemberJoined { user } => assert_eq!(user, hopper.user),
                other => panic!("unexpected event {other:?}"),
            }
        }
        other => panic!("unexpected frame {other:?}"),
    }
    // ...followed by their arrival in #general, which grace shares.
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), 10);
            assert!(matches!(
                envelope.event,
                Event::MemberAdded { user_id, .. } if user_id == hopper.user.id
            ));
        }
        other => panic!("unexpected frame {other:?}"),
    }
    drop(ws);
    app.server.shutdown().await.unwrap();
}

/// The central correctness test: whenever a client connects relative to concurrent commits, the
/// replay plus live stream contains every event exactly once, in order, with no gaps.
#[sqlx::test]
async fn nothing_is_lost_or_duplicated_while_clients_connect_mid_stream(pool: PgPool) {
    const SENDERS: usize = 8;
    const PER_SENDER: usize = 50;
    const REGISTRATIONS: usize = 4;
    const CHURN_ROUNDS: usize = 25;
    // The watchers are members of #general only. They see each message, and each registration
    // (two events: joining the workspace, then #general). They must see none of the churn below.
    const EXPECTED: usize = SENDERS * PER_SENDER + 2 * REGISTRATIONS;
    const OLD_EVENTS: usize = 300;
    const KEEP_NEWEST: u32 = 50;

    let app = App::start(pool).await;
    let watcher = app.register("watcher").await;
    let mut senders = Vec::new();
    for index in 0..SENDERS {
        senders.push(app.register(&format!("sender{index}")).await);
    }
    // A private channel whose membership churns for the whole run. The owner stays in it; the
    // churner is added and removed over and over; the watchers are never in it.
    let owner = senders[0].clone();
    let churner = senders[1].clone();
    let churn = app.create_channel(&owner, "churn", "private").await.id;
    // History that has aged out of the retention window, so the pruner has real work to do while
    // everything below happens. Subscribers start at `baseline`, so their cursors stay valid.
    for n in 0..OLD_EVENTS {
        app.send(&watcher, app.general, &format!("old {n}")).await;
    }
    let baseline = app.head().await;
    app.backdate_events(baseline, 40).await;
    let policy = retention(KEEP_NEWEST);

    // Prune continuously for as long as the writers run. Each pass takes the same workspace lock
    // the writers do, so pruning and writing contend and interleave.
    let stop_pruning = CancellationToken::new();
    let pruning = {
        let pool = app.pool.clone();
        let stop = stop_pruning.clone();
        tokio::spawn(async move {
            let mut passes = 0u32;
            while !stop.is_cancelled() {
                prune_all(&pool, policy, &CancellationToken::new())
                    .await
                    .unwrap();
                passes += 1;
                // Pacing between passes, not synchronization.
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            passes
        })
    };

    // The churner's own live connection, which must receive each of its removals exactly once and
    // nothing about the channel after the last one. It stops when the end-of-run marker arrives.
    let churner_stream = {
        let mut ws = app.connect_ws(&churner).await;
        ws.subscribe(app.workspace_id, baseline).await;
        tokio::spawn(async move {
            let mut seen = Vec::new();
            loop {
                match ws.next().await {
                    ServerFrame::Event(envelope) => {
                        let done = matches!(&envelope.event,
                            Event::MemberJoined { user } if user.username.as_str() == "stressmarker");
                        seen.push(envelope);
                        if done {
                            return seen;
                        }
                    }
                    ServerFrame::CaughtUp { .. } => {}
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        })
    };

    let finished_sends = Arc::new(Semaphore::new(0));
    let mut writers = Vec::new();
    for sender in senders {
        let http = app.http.clone();
        let url = app.url(&lazy_protocol::api::paths::messages(app.general));
        let finished = Arc::clone(&finished_sends);
        writers.push(tokio::spawn(async move {
            for n in 0..PER_SENDER {
                let body = SendMessageRequest {
                    operation_id: OperationId::generate(),
                    document: MessageDocument::from_plain_text(&format!(
                        "{}-{n}",
                        sender.user.username
                    ))
                    .unwrap(),
                };
                let response = http
                    .post(&url)
                    .bearer_auth(&sender.token)
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success(), "{}", response.status());
                finished.add_permits(1);
            }
        }));
    }
    for index in 0..REGISTRATIONS {
        let http = app.http.clone();
        let url = app.url("/v1/auth/register");
        writers.push(tokio::spawn(async move {
            let body = serde_json::json!({
                "username": format!("joiner{index}"),
                "display_name": format!("Joiner {index}"),
                "password": PASSWORD,
            });
            let response = http.post(url).json(&body).send().await.unwrap();
            assert!(response.status().is_success());
        }));
    }
    // Membership churn: add, post, remove, post.
    writers.push({
        let http = app.http.clone();
        let base = app.server.base_url.clone();
        let (owner, churner) = (owner.clone(), churner.clone());
        tokio::spawn(async move {
            use lazy_protocol::api::paths;
            for round in 0..CHURN_ROUNDS {
                let added = http
                    .post(format!(
                        "{base}{}",
                        paths::conversation_action(churn, "members")
                    ))
                    .bearer_auth(&owner.token)
                    .json(&serde_json::json!({"user_ids": [churner.user.id]}))
                    .send()
                    .await
                    .unwrap();
                assert!(added.status().is_success(), "add: {}", added.status());
                for phase in ["inside", "outside"] {
                    let body = SendMessageRequest {
                        operation_id: OperationId::generate(),
                        document: MessageDocument::from_plain_text(&format!(
                            "churn {phase} {round}"
                        ))
                        .unwrap(),
                    };
                    let sent = http
                        .post(format!("{base}{}", paths::messages(churn)))
                        .bearer_auth(&owner.token)
                        .json(&body)
                        .send()
                        .await
                        .unwrap();
                    assert!(sent.status().is_success(), "churn send: {}", sent.status());
                    if phase == "inside" {
                        let removed = http
                            .delete(format!("{base}{}", paths::member(churn, churner.user.id)))
                            .bearer_auth(&owner.token)
                            .send()
                            .await
                            .unwrap();
                        assert_eq!(removed.status().as_u16(), 204, "remove");
                    }
                }
            }
        })
    });

    // Subscribers connect after progressively more sends have completed, so they overlap commits
    // in every phase: before, during, and after. The window between a subscriber's replay and its
    // live feed is tiny, so many subscribers are needed to land in it reliably if it were open.
    let mut completed = 0usize;
    let mut readers = Vec::new();
    for threshold in (0..=SENDERS * PER_SENDER - 20).step_by(12) {
        if threshold > completed {
            finished_sends
                .acquire_many((threshold - completed) as u32)
                .await
                .unwrap()
                .forget();
            completed = threshold;
        }
        let mut ws = app.connect_ws(&watcher).await;
        ws.subscribe(app.workspace_id, baseline).await;
        readers.push(tokio::spawn(async move {
            let mut positions = Vec::new();
            while positions.len() < EXPECTED {
                match ws.next().await {
                    ServerFrame::Event(envelope) => positions.push(envelope.seq.get()),
                    ServerFrame::CaughtUp { .. } => {}
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            positions
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    // The end of the run, as far as the churner's stream is concerned.
    app.register("stressmarker").await;

    stop_pruning.cancel();
    let passes = pruning.await.unwrap();
    assert!(passes >= 1, "the pruner ran alongside the writers");
    // One last pass: only the aged-out history was eligible, and all of it is gone.
    prune_all(&app.pool, policy, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(app.event_floor().await, baseline);

    // The log is gapless from the floor to the head, churn and all.
    let log = app.log_positions_after(EventSeq::ZERO).await;
    assert_eq!(
        log,
        (baseline.get() + 1..=app.head().await.get()).collect::<Vec<_>>(),
        "the log itself is gapless"
    );

    // What a #general member who never entered the churn channel may see: workspace events and
    // #general's. Exactly those, once each, in order, to every subscriber.
    let visible_to_watchers: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM events WHERE workspace_id = $1 AND seq > $2 AND (audience = 'workspace' \
         OR (audience = 'members' AND conversation_id = $3)) ORDER BY seq",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(baseline.get())
    .bind(app.general.as_uuid())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    // (The end-of-run marker's two events came after every reader had its EXPECTED events.)
    assert_eq!(visible_to_watchers.len(), EXPECTED + 2);
    for (index, reader) in readers.into_iter().enumerate() {
        let received = reader.await.unwrap();
        assert_eq!(
            received,
            visible_to_watchers[..EXPECTED],
            "subscriber {index} saw a different stream, or a leak from the churn channel"
        );
    }

    // The churner: each of its removals exactly once, in order, and nothing about the channel
    // after the last removal.
    let stream = churner_stream.await.unwrap();
    assert!(
        stream.windows(2).all(|pair| pair[0].seq < pair[1].seq),
        "positions strictly increase"
    );
    let about_churn = |event: &Event| match event {
        Event::MessageCreated { message, .. } => message.conversation_id == churn,
        Event::ConversationCreated { conversation }
        | Event::ConversationUpdated { conversation }
        | Event::MemberAdded { conversation, .. } => conversation.id == churn,
        Event::MemberRemoved {
            conversation_id, ..
        } => *conversation_id == churn,
        _ => false,
    };
    let removals: Vec<i64> = stream
        .iter()
        .filter(|e| matches!(&e.event, Event::MemberRemoved { user_id, .. } if *user_id == churner.user.id))
        .map(|e| e.seq.get())
        .collect();
    let removals_in_log: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM events WHERE kind = 'member_removed' AND conversation_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(churn.as_uuid())
    .bind(baseline.get())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(removals_in_log.len(), CHURN_ROUNDS);
    assert_eq!(
        removals, removals_in_log,
        "every removal reached the person removed, exactly once"
    );
    let last_removal = *removals_in_log.last().unwrap();
    assert!(
        stream
            .iter()
            .filter(|e| about_churn(&e.event))
            .all(|e| e.seq.get() <= last_removal),
        "nothing about the channel reached the churner after their last removal"
    );

    // The owner never left, so a replay shows them the whole history of the churn, in order.
    let mut owner_ws = app.connect_ws(&owner).await;
    owner_ws.subscribe(app.workspace_id, baseline).await;
    let (owner_events, _) = owner_ws.replay().await;
    let owner_churn: Vec<i64> = owner_events
        .iter()
        .filter(|e| about_churn(&e.event))
        .map(|e| e.seq.get())
        .collect();
    let churn_in_log: Vec<i64> = sqlx::query_scalar(
        "SELECT seq FROM events WHERE conversation_id = $1 AND seq > $2 ORDER BY seq",
    )
    .bind(churn.as_uuid())
    .bind(baseline.get())
    .fetch_all(&app.pool)
    .await
    .unwrap();
    assert_eq!(churn_in_log.len(), CHURN_ROUNDS * 5);
    assert_eq!(owner_churn, churn_in_log);

    // And a fresh replay for the churner, now outside the channel for good, shows only the
    // removals.
    let mut churner_ws = app.connect_ws(&churner).await;
    churner_ws.subscribe(app.workspace_id, baseline).await;
    let (churner_events, _) = churner_ws.replay().await;
    let churner_churn: Vec<&lazy_protocol::EventEnvelope> = churner_events
        .iter()
        .filter(|e| about_churn(&e.event))
        .collect();
    assert_eq!(churner_churn.len(), CHURN_ROUNDS);
    assert!(churner_churn.iter().all(|e| matches!(&e.event,
        Event::MemberRemoved { user_id, .. } if *user_id == churner.user.id)));
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn replay_pages_through_a_long_log_and_then_goes_live(pool: PgPool) {
    const MESSAGES: i64 = 450; // more than two replay pages
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let baseline = app.head().await;
    for n in 0..MESSAGES {
        app.send(&ada, app.general, &format!("m{n}")).await;
    }

    let mut ws = app.connect_ws(&ada).await;
    ws.subscribe(app.workspace_id, baseline).await;
    let (replayed, caught_up) = ws.replay().await;
    let positions: Vec<i64> = replayed.iter().map(|e| e.seq.get()).collect();
    assert_eq!(
        positions,
        (baseline.get() + 1..=baseline.get() + MESSAGES).collect::<Vec<_>>()
    );
    assert_eq!(caught_up.get(), baseline.get() + MESSAGES);

    app.send(&ada, app.general, "after").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), baseline.get() + MESSAGES + 1)
        }
        other => panic!("unexpected frame {other:?}"),
    }
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn reconnecting_with_a_cursor_receives_only_newer_events(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    for text in ["a", "b", "c"] {
        app.send(&ada, app.general, text).await;
    }

    let mut ws = app.connect_ws(&grace).await;
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (first, _) = ws.replay().await;
    let cursor = first.last().unwrap().seq;
    drop(ws);

    // Things happen while grace is away, including an event she could not have predicted.
    app.send(&ada, app.general, "d").await;
    app.send(&ada, app.general, "e").await;

    let mut ws = app.connect_ws(&grace).await;
    ws.subscribe(app.workspace_id, cursor).await;
    let (missed, caught_up) = ws.replay().await;
    let texts: Vec<String> = missed.iter().map(message_text).collect();
    assert_eq!(texts, vec!["d", "e"]);
    assert_eq!(missed[0].seq.get(), cursor.get() + 1);
    assert_eq!(caught_up, app.head().await);

    // Resubscribing at the head replays nothing.
    ws.subscribe(app.workspace_id, caught_up).await;
    let (nothing, again) = ws.replay().await;
    assert!(nothing.is_empty());
    assert_eq!(again, caught_up);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_cursor_beyond_the_log_requires_a_fresh_snapshot(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let head = app.head().await;

    let mut ws = app.connect_ws(&ada).await;
    ws.subscribe(app.workspace_id, EventSeq::new(head.get() + 10).unwrap())
        .await;
    match ws.next().await {
        ServerFrame::SnapshotRequired {
            workspace_id,
            reason,
        } => {
            assert_eq!(workspace_id, app.workspace_id);
            assert_eq!(reason, SnapshotReason::CursorAhead);
        }
        other => panic!("unexpected frame {other:?}"),
    }
    // The connection is still good for a corrected subscription.
    ws.subscribe(app.workspace_id, head).await;
    let (events, caught_up) = ws.replay().await;
    assert!(events.is_empty());
    assert_eq!(caught_up, head);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn losing_access_to_a_channel_stops_delivery_and_replay(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let secret = app.create_conversation("secret", "private", false).await;
    app.add_to_conversation(secret, &ada).await;
    app.add_to_conversation(secret, &grace).await;
    let baseline = app.head().await;

    let mut ws = app.connect_ws(&grace).await;
    ws.subscribe(app.workspace_id, baseline).await;
    let (_, _) = ws.replay().await;

    // While a member, grace receives the private channel's messages.
    app.send(&ada, secret, "while grace was a member").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(message_text(&envelope), "while grace was a member")
        }
        other => panic!("unexpected frame {other:?}"),
    }

    app.remove_from_conversation(secret, &grace).await;
    app.send(&ada, secret, "after grace was removed").await;
    // A later event in the same stream acts as a barrier: had the secret message been deliverable
    // it would have arrived before this one.
    app.send(&ada, app.general, "public marker").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => assert_eq!(
            message_text(&envelope),
            "public marker",
            "the removed member must not receive the private message"
        ),
        other => panic!("unexpected frame {other:?}"),
    }

    // A fresh subscription from the start replays the public history but none of the secret.
    let mut fresh = app.connect_ws(&grace).await;
    fresh.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (replayed, _) = fresh.replay().await;
    for envelope in &replayed {
        if let Event::MessageCreated { message, .. } = &envelope.event {
            assert_ne!(
                message.conversation_id, secret,
                "private history leaked on replay"
            );
        }
    }
    assert!(replayed.iter().any(|e| matches!(&e.event, Event::MessageCreated { message, .. } if message.conversation_id == app.general)));
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn losing_workspace_membership_ends_the_subscription(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let grace = app.register("grace").await;
    let head = app.head().await;

    let mut ws = app.connect_ws(&grace).await;
    ws.subscribe(app.workspace_id, head).await;
    ws.replay().await;

    sqlx::query("DELETE FROM workspace_members WHERE user_id = $1")
        .bind(grace.user.id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    app.send(&ada, app.general, "grace should not see this")
        .await;
    match ws.next().await {
        ServerFrame::Error { code, .. } => assert_eq!(code, ErrorCode::Forbidden),
        other => panic!("expected a forbidden error, got {other:?}"),
    }
    assert!(ws.is_quiet_for(Duration::from_millis(300)).await);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn subscribing_to_someone_elses_workspace_is_forbidden(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let other = WorkspaceId::generate();
    sqlx::query("INSERT INTO workspaces (id, name) VALUES ($1, 'Elsewhere')")
        .bind(other.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();

    let mut ws = app.connect_ws(&ada).await;
    ws.subscribe(other, EventSeq::ZERO).await;
    match ws.next().await {
        ServerFrame::Error { code, .. } => assert_eq!(code, ErrorCode::Forbidden),
        other => panic!("unexpected frame {other:?}"),
    }
    // A workspace that does not exist is indistinguishable.
    ws.subscribe(WorkspaceId::generate(), EventSeq::ZERO).await;
    match ws.next().await {
        ServerFrame::Error { code, .. } => assert_eq!(code, ErrorCode::Forbidden),
        other => panic!("unexpected frame {other:?}"),
    }
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (events, _) = ws.replay().await;
    assert_eq!(events.len(), 2, "ada joining the workspace and #general");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn malformed_frames_get_an_error_and_do_not_end_the_connection(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let mut ws = app.connect_ws(&ada).await;

    for garbage in [
        "not json at all",
        r#"{"type":"unknown_frame"}"#,
        r#"{"type":"subscribe","after":-4}"#,
    ] {
        ws.send_text(garbage).await;
        match ws.next().await {
            ServerFrame::Error { code, .. } => {
                assert_eq!(code, ErrorCode::InvalidRequest, "{garbage}")
            }
            other => panic!("unexpected frame {other:?}"),
        }
    }
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (events, _) = ws.replay().await;
    assert_eq!(events.len(), 2, "ada joining the workspace and #general");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_upgrade_requires_a_valid_session(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    for token in ["", "lzy_nonsense", &format!("lzy_{}", "a".repeat(64))] {
        match Ws::try_connect(&app.server, token).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status().as_u16(), 401, "token {token:?}");
            }
            Ok(_) => panic!("handshake succeeded for token {token:?}"),
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }
    // A revoked session is refused too.
    app.http
        .post(app.url("/v1/auth/logout"))
        .bearer_auth(&ada.token)
        .send()
        .await
        .unwrap();
    assert!(Ws::try_connect(&app.server, &ada.token).await.is_err());
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn shutdown_closes_open_sockets_with_a_going_away_frame(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let mut ws = app.connect_ws(&ada).await;
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    ws.replay().await;

    let shutdown = tokio::spawn(app.server.shutdown());
    match ws.next_or_closed().await {
        Err(Closed::Frame(Some(CloseCode::Away))) => {}
        other => panic!("expected a going-away close frame, got {other:?}"),
    }
    tokio::time::timeout(DEADLINE, shutdown)
        .await
        .expect("shutdown finishes once sockets are closed")
        .unwrap()
        .unwrap();
}

#[sqlx::test]
async fn one_connection_can_hold_many_subscriptions_and_resubscribe(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let head = app.head().await;
    let mut ws = app.connect_ws(&ada).await;

    // Subscribing twice to the same workspace replaces the first subscription: the second replay
    // starts from the new cursor and live events are not delivered twice.
    ws.subscribe(app.workspace_id, head).await;
    ws.replay().await;
    ws.subscribe(app.workspace_id, EventSeq::ZERO).await;
    let (replayed, _) = ws.replay().await;
    assert_eq!(replayed.len(), head.get() as usize);

    app.send(&ada, app.general, "only once").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => assert_eq!(message_text(&envelope), "only once"),
        other => panic!("unexpected frame {other:?}"),
    }
    assert!(
        ws.is_quiet_for(Duration::from_millis(300)).await,
        "the event arrived twice"
    );
    app.server.shutdown().await.unwrap();
}
