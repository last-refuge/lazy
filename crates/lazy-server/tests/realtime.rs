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
    // ada joined (1), three messages (2..=4), grace joined (5).
    let positions: Vec<i64> = replayed.iter().map(|e| e.seq.get()).collect();
    assert_eq!(positions, vec![1, 2, 3, 4, 5]);
    assert_eq!(caught_up.get(), 5);
    assert!(matches!(replayed[0].event, Event::MemberJoined { .. }));
    assert_eq!(message_text(&replayed[1]), "one");
    assert_eq!(message_text(&replayed[3]), "three");

    // Live: a message, with the sender's operation id so its sender can reconcile.
    let operation = OperationId::generate();
    let sent = app.send_raw(&ada, app.general, operation, "live one").await;
    assert!(sent.status().is_success());
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), 6);
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
            assert_eq!(envelope.seq.get(), 7);
            match envelope.event {
                Event::MemberJoined { user } => assert_eq!(user, hopper.user),
                other => panic!("unexpected event {other:?}"),
            }
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
    const EXPECTED: usize = SENDERS * PER_SENDER + REGISTRATIONS;
    const OLD_EVENTS: usize = 300;
    const KEEP_NEWEST: u32 = 50;

    let app = App::start(pool).await;
    let watcher = app.register("watcher").await;
    let mut senders = Vec::new();
    for index in 0..SENDERS {
        senders.push(app.register(&format!("sender{index}")).await);
    }
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

    stop_pruning.cancel();
    let passes = pruning.await.unwrap();
    assert!(passes >= 1, "the pruner ran alongside the writers");
    // One last pass: only the aged-out history was eligible, and all of it is gone.
    prune_all(&app.pool, policy, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(app.event_floor().await, baseline);

    let expected = app.log_positions_after(EventSeq::ZERO).await;
    assert_eq!(
        expected.len(),
        EXPECTED,
        "only the new events remain in the log"
    );
    assert_eq!(
        expected,
        (baseline.get() + 1..=baseline.get() + EXPECTED as i64).collect::<Vec<_>>(),
        "the log itself is gapless"
    );
    for (index, reader) in readers.into_iter().enumerate() {
        let received = reader.await.unwrap();
        assert_eq!(
            received, expected,
            "subscriber {index} saw a different stream"
        );
    }
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
    assert_eq!(events.len(), 1);
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
    assert_eq!(events.len(), 1);
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
