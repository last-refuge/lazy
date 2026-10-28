mod common;

use std::sync::Arc;

use common::*;
use lazy_domain::{EventSeq, OperationId};
use lazy_protocol::{Event, OperationStatus, ServerFrame, SnapshotReason};
use lazy_server::pruner::{self, prune_all};
use lazy_server::{PasswordCost, RetentionPolicy, ServerOptions};
use sqlx::PgPool;
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

/// Registers ada and sends `count` messages, returning her and the log head.
async fn log_of(app: &App, count: usize) -> (Account, EventSeq) {
    let ada = app.register("ada").await;
    for n in 0..count {
        app.send(&ada, app.general, &format!("message {n}")).await;
    }
    (ada, app.head().await)
}

async fn count_events(app: &App) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM events WHERE workspace_id = $1")
        .bind(app.workspace_id.as_uuid())
        .fetch_one(&app.pool)
        .await
        .unwrap()
}

fn never() -> CancellationToken {
    CancellationToken::new()
}

#[sqlx::test]
async fn pruning_removes_aged_out_events_and_always_keeps_the_newest(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 20).await;
    assert_eq!(head.get(), 22);
    app.backdate_events(head, 40).await;

    // Nothing happens when the policy is off, or the events are younger than the window.
    assert!(
        prune_all(&app.pool, RetentionPolicy::disabled(), &never())
            .await
            .unwrap()
            .is_empty()
    );
    let young = RetentionPolicy {
        days: 60,
        ..retention(5)
    };
    let reports = prune_all(&app.pool, young, &never()).await.unwrap();
    assert_eq!(reports[0].deleted, 0);
    // The default keeps the newest 1000, which is more than exist.
    let reports = prune_all(&app.pool, RetentionPolicy::from_days(30), &never())
        .await
        .unwrap();
    assert_eq!(reports[0].deleted, 0);
    assert_eq!(
        (app.event_floor().await.get(), count_events(&app).await),
        (0, 22)
    );

    // Everything is old, but the newest five stay.
    let reports = prune_all(&app.pool, retention(5), &never()).await.unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!((reports[0].deleted, reports[0].floor.get()), (17, 17));
    assert_eq!(app.event_floor().await.get(), 17);
    assert_eq!(
        app.log_positions_after(EventSeq::ZERO).await,
        vec![18, 19, 20, 21, 22]
    );

    // Pruning again changes nothing, and a gentler policy never lowers the floor.
    let again = prune_all(&app.pool, retention(5), &never()).await.unwrap();
    assert_eq!((again[0].deleted, again[0].floor.get()), (0, 17));
    prune_all(&app.pool, retention(100), &never())
        .await
        .unwrap();
    assert_eq!(app.event_floor().await.get(), 17);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_default_policy_keeps_the_newest_thousand_events(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 1100).await;
    assert_eq!(head.get(), 1102);
    app.backdate_events(head, 90).await;

    let reports = prune_all(&app.pool, RetentionPolicy::from_days(30), &never())
        .await
        .unwrap();
    assert_eq!(reports[0].deleted, 102);
    assert_eq!(app.event_floor().await.get(), 102);
    assert_eq!(count_events(&app).await, 1000);
    assert_eq!(
        app.log_positions_after(EventSeq::ZERO).await.first(),
        Some(&103)
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn only_a_prefix_that_has_aged_out_is_pruned(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 20).await;
    // Only the first ten events are old; the rest are within the window.
    app.backdate_events(EventSeq::new(10).unwrap(), 40).await;

    let reports = prune_all(&app.pool, retention(5), &never()).await.unwrap();
    assert_eq!((reports[0].deleted, reports[0].floor.get()), (10, 10));
    assert_eq!(
        app.log_positions_after(EventSeq::ZERO).await.len() as i64,
        head.get() - 10
    );
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn large_backlogs_are_pruned_in_several_bounded_transactions(pool: PgPool) {
    // More than two chunks' worth of aged-out events, written directly for speed.
    const TOTAL: i64 = 12_000;
    let app = App::start(pool).await;
    sqlx::query(
        "INSERT INTO events (workspace_id, seq, occurred_at, kind, audience, payload) \
         SELECT $1, g, now() - interval '40 days', 'filler', 'workspace', '{}'::jsonb \
         FROM generate_series(1, $2::bigint) g",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(TOTAL)
    .execute(&app.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workspaces SET event_head = $2 WHERE id = $1")
        .bind(app.workspace_id.as_uuid())
        .bind(TOTAL)
        .execute(&app.pool)
        .await
        .unwrap();

    let report = pruner::prune_workspace(&app.pool, app.workspace_id, retention(10), &never())
        .await
        .unwrap();
    assert_eq!(report.deleted as i64, TOTAL - 10);
    assert_eq!(report.floor.get(), TOTAL - 10);
    assert_eq!(app.event_floor().await.get(), TOTAL - 10);
    assert_eq!(count_events(&app).await, 10);

    // A pruner told to stop finishes the chunk it is on and no more.
    sqlx::query("UPDATE workspaces SET event_floor = 0 WHERE id = $1")
        .bind(app.workspace_id.as_uuid())
        .execute(&app.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO events (workspace_id, seq, occurred_at, kind, audience, payload) \
         SELECT $1, g, now() - interval '40 days', 'filler', 'workspace', '{}'::jsonb \
         FROM generate_series(1, $2::bigint - 10) g",
    )
    .bind(app.workspace_id.as_uuid())
    .bind(TOTAL)
    .execute(&app.pool)
    .await
    .unwrap();
    let stopped = CancellationToken::new();
    stopped.cancel();
    let report = pruner::prune_workspace(&app.pool, app.workspace_id, retention(10), &stopped)
        .await
        .unwrap();
    assert_eq!(report.deleted, 5_000, "exactly one chunk");
    assert_eq!(report.floor.get(), 5_000);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn expired_cursors_take_a_snapshot_and_the_floor_itself_replays(pool: PgPool) {
    let app = App::start(pool).await;
    let (ada, head) = log_of(&app, 20).await;
    app.backdate_events(head, 40).await;
    prune_all(&app.pool, retention(5), &never()).await.unwrap();
    let floor = app.event_floor().await;
    assert_eq!(floor.get(), 17);

    let mut ws = app.connect_ws(&ada).await;
    for stale in [0, 1, 15] {
        ws.subscribe(app.workspace_id, EventSeq::new(stale).unwrap())
            .await;
        match ws.next().await {
            ServerFrame::SnapshotRequired {
                workspace_id,
                reason,
            } => {
                assert_eq!(workspace_id, app.workspace_id);
                assert_eq!(reason, SnapshotReason::CursorExpired, "cursor {stale}");
            }
            other => panic!("cursor {stale}: unexpected frame {other:?}"),
        }
    }

    // A cursor exactly at the floor is replayable: everything after it still exists.
    ws.subscribe(app.workspace_id, floor).await;
    let (replayed, caught_up) = ws.replay().await;
    let positions: Vec<i64> = replayed.iter().map(|e| e.seq.get()).collect();
    assert_eq!(positions, vec![18, 19, 20, 21, 22]);
    assert_eq!(caught_up, head);

    // A cursor beyond the head is still the other kind of refusal.
    ws.subscribe(app.workspace_id, EventSeq::new(head.get() + 1).unwrap())
        .await;
    match ws.next().await {
        ServerFrame::SnapshotRequired { reason, .. } => {
            assert_eq!(reason, SnapshotReason::CursorAhead)
        }
        other => panic!("unexpected frame {other:?}"),
    }

    // Live delivery carries on across the prune: the next position is simply head + 1.
    ws.subscribe(app.workspace_id, head).await;
    ws.replay().await;
    app.send(&ada, app.general, "after the prune").await;
    match ws.next().await {
        ServerFrame::Event(envelope) => {
            assert_eq!(envelope.seq.get(), head.get() + 1);
            assert!(matches!(envelope.event, Event::MessageCreated { .. }));
        }
        other => panic!("unexpected frame {other:?}"),
    }
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn pruning_does_not_affect_history_bootstrap_or_operation_status(pool: PgPool) {
    let app = App::start(pool).await;
    let ada = app.register("ada").await;
    let operation = OperationId::generate();
    let committed: lazy_protocol::SendMessageResponse = app
        .send_raw(&ada, app.general, operation, "remember me")
        .await
        .json()
        .await
        .unwrap();
    for n in 0..10 {
        app.send(&ada, app.general, &format!("filler {n}")).await;
    }
    let head = app.head().await;
    let before = app.bootstrap_ok(&ada).await;
    app.backdate_events(head, 40).await;
    prune_all(&app.pool, retention(3), &never()).await.unwrap();
    assert!(app.event_floor().await.get() > 0);

    // History comes from messages, not events.
    let history = app.history_ok(&ada, app.general, &[]).await;
    assert_eq!(history.messages.len(), 11);
    assert_eq!(history.messages[0], committed.message);
    // Operation receipts are not pruned with the log.
    assert_eq!(
        app.operation(&ada, operation).await,
        OperationStatus::Committed {
            message: committed.message.clone()
        }
    );
    // The same operation id still deduplicates.
    let again: lazy_protocol::SendMessageResponse = app
        .send_raw(&ada, app.general, operation, "remember me")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again, committed);
    // Bootstrap is unchanged, and a client that bootstraps can subscribe at its head.
    assert_eq!(app.bootstrap_ok(&ada).await, before);
    let mut ws = app.connect_ws(&ada).await;
    ws.subscribe(app.workspace_id, before.workspaces[0].head)
        .await;
    let (events, _) = ws.replay().await;
    assert!(events.is_empty());
    app.server.shutdown().await.unwrap();
}

/// What the replay loop relies on: a page read after a prune reports the raised floor in the same
/// snapshot as its events, so a cursor that the prune overtook is detectable instead of silently
/// skipping the deleted events.
#[sqlx::test]
async fn a_page_read_after_a_prune_reports_that_the_cursor_was_overtaken(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 250).await;
    app.backdate_events(head, 40).await;

    // The first page of a replay from the start.
    let first = lazy_server::events::fetch_after(&app.pool, app.workspace_id, EventSeq::ZERO, 100)
        .await
        .unwrap();
    assert_eq!((first.scanned, first.floor.get()), (100, 0));

    // A prune lands between this replay's pages.
    prune_all(&app.pool, retention(50), &never()).await.unwrap();
    let floor = app.event_floor().await;
    assert!(floor > first.scanned_to);

    // The next page starts after the floor, not after the cursor. Without the floor in the same
    // read the replay could not tell, and would have skipped positions 101..=floor.
    let second =
        lazy_server::events::fetch_after(&app.pool, app.workspace_id, first.scanned_to, 100)
            .await
            .unwrap();
    assert_eq!(second.floor, floor);
    assert!(
        second.floor > first.scanned_to,
        "the overtaken cursor is detectable"
    );
    assert_eq!(
        second.events.first().map(|e| e.seq.get()),
        Some(floor.get() + 1)
    );
    app.server.shutdown().await.unwrap();
}

/// Subscribers that start from the beginning race a prune. Each must see either a complete,
/// gapless stream or a request to take a fresh snapshot, never a stream with a hole.
#[sqlx::test]
async fn subscribers_racing_a_prune_never_see_a_hole(pool: PgPool) {
    const SUBSCRIBERS: usize = 24;
    let app = App::start(pool).await;
    let (ada, head) = log_of(&app, 450).await;
    app.backdate_events(head, 40).await;

    let start = Arc::new(Barrier::new(SUBSCRIBERS + 1));
    let mut readers = Vec::new();
    for _ in 0..SUBSCRIBERS {
        let mut ws = app.connect_ws(&ada).await;
        let workspace_id = app.workspace_id;
        let start = Arc::clone(&start);
        readers.push(tokio::spawn(async move {
            start.wait().await;
            ws.subscribe(workspace_id, EventSeq::ZERO).await;
            let mut positions = Vec::new();
            loop {
                match ws.next().await {
                    ServerFrame::Event(envelope) => positions.push(envelope.seq.get()),
                    ServerFrame::CaughtUp { seq, .. } => return Ok((positions, seq)),
                    ServerFrame::SnapshotRequired { reason, .. } => {
                        return Err((positions, reason));
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        }));
    }
    let pool = app.pool.clone();
    let pruning = tokio::spawn(async move {
        start.wait().await;
        prune_all(&pool, retention(100), &CancellationToken::new())
            .await
            .unwrap();
    });

    let (mut complete, mut refused) = (0, 0);
    for reader in readers {
        match reader.await.unwrap() {
            Ok((positions, caught_up)) => {
                complete += 1;
                assert_eq!(caught_up, head);
                assert_eq!(
                    positions,
                    (1..=head.get()).collect::<Vec<_>>(),
                    "a hole in a complete stream"
                );
            }
            Err((positions, reason)) => {
                refused += 1;
                assert_eq!(reason, SnapshotReason::CursorExpired);
                // Whatever arrived before the refusal was a gapless prefix.
                assert_eq!(positions, (1..=positions.len() as i64).collect::<Vec<_>>());
            }
        }
    }
    pruning.await.unwrap();
    assert_eq!(complete + refused, SUBSCRIBERS);
    assert_eq!(app.event_floor().await.get(), head.get() - 100);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_pruner_runs_periodically_and_shuts_down_cleanly(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 20).await;
    app.backdate_events(head, 40).await;

    let shutdown = CancellationToken::new();
    let handle = pruner::spawn(app.pool.clone(), retention(5), shutdown.clone());
    let mut passes = handle.passes();
    // Several passes prove it repeats on its interval, not just at startup.
    tokio::time::timeout(DEADLINE, passes.wait_for(|count| *count >= 3))
        .await
        .expect("the pruner keeps running")
        .unwrap();
    assert_eq!(app.event_floor().await.get(), head.get() - 5);

    shutdown.cancel();
    tokio::time::timeout(DEADLINE, handle.join())
        .await
        .expect("the pruner stops when asked");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn a_server_prunes_at_startup_and_stops_its_pruner_on_shutdown(pool: PgPool) {
    let app = App::start(pool).await;
    let (_, head) = log_of(&app, 20).await;
    app.backdate_events(head, 40).await;
    let pool = app.pool.clone();
    app.server.shutdown().await.unwrap();

    // A new server with pruning on prunes during startup; its shutdown waits for the pruner.
    let server = lazy_server::testing::spawn_test_server_with(
        pool.clone(),
        ServerOptions {
            open_registration: true,
            password_cost: PasswordCost::FAST_FOR_TESTS,
            retention: retention(5),
        },
    )
    .await;
    server.shutdown().await.unwrap();

    let floor: i64 = sqlx::query_scalar("SELECT event_floor FROM workspaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(floor, head.get() - 5);
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(remaining, 5);
}
