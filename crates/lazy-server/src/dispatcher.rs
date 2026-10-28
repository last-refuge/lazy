//! Tails the durable event log and fans events out to connected subscribers.
//!
//! One dispatcher task per server reads each active workspace's log from the database, in
//! position order, and publishes contiguous batches on a bounded broadcast channel. `LISTEN/NOTIFY`
//! only wakes it early; a short poll interval covers lost notifications, so a missed `NOTIFY`
//! delays delivery by at most one interval and never loses an event.
//!
//! Subscribers must subscribe *before* replaying from the database; see [`crate::realtime`].

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use lazy_domain::{EventSeq, WorkspaceId};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::AppError;
use crate::events::{self, NOTIFY_CHANNEL, StoredEvent};

/// How often the log is polled when no notification arrives.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Most log rows read and published at once.
pub const BATCH_SIZE: usize = 200;
/// Batches buffered per workspace before a slow subscriber is told it lagged.
const BROADCAST_CAPACITY: usize = 256;

/// Events of one workspace covering positions `(after, last_seq]`, contiguous with the batch
/// published before it.
#[derive(Debug)]
pub struct Batch {
    pub workspace_id: WorkspaceId,
    /// The position the dispatcher had reached before this batch (or the retention floor, if
    /// pruning had passed it).
    pub after: EventSeq,
    /// The last position this batch covers. Events that could not be decoded are skipped but
    /// still covered.
    pub last_seq: EventSeq,
    pub events: Vec<StoredEvent>,
}

struct WorkspaceStream {
    sender: broadcast::Sender<Arc<Batch>>,
    /// Everything up to and including this position has been published.
    dispatched: EventSeq,
}

struct Inner {
    pool: PgPool,
    streams: Mutex<HashMap<WorkspaceId, WorkspaceStream>>,
    wake: Notify,
    shutdown: CancellationToken,
}

/// Handle used by connections to subscribe. Cloning is cheap.
#[derive(Clone)]
pub struct Dispatcher {
    inner: Arc<Inner>,
}

/// Owns the dispatcher's tasks. Cancel the token passed to [`Dispatcher::spawn`] and then call
/// [`DispatcherTasks::join`] to stop it.
pub struct DispatcherTasks {
    tasks: JoinSet<()>,
}

impl Dispatcher {
    /// Starts the notification listener and the dispatch loop.
    pub fn spawn(pool: PgPool, shutdown: CancellationToken) -> (Dispatcher, DispatcherTasks) {
        let inner = Arc::new(Inner {
            pool,
            streams: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            shutdown,
        });
        let mut tasks = JoinSet::new();
        tasks.spawn(listen_loop(Arc::clone(&inner)));
        tasks.spawn(dispatch_loop(Arc::clone(&inner)));
        (Dispatcher { inner }, DispatcherTasks { tasks })
    }

    /// Subscribes to a workspace's live batches.
    ///
    /// The first subscriber starts the workspace's stream at the current log head. Batches that
    /// were published before this call are not delivered, but everything they contain is already
    /// committed and visible to a database read issued after this call returns.
    pub async fn subscribe(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<broadcast::Receiver<Arc<Batch>>, AppError> {
        let mut streams = self.inner.streams.lock().await;
        if let Some(stream) = streams.get(&workspace_id) {
            return Ok(stream.sender.subscribe());
        }
        let head = events::head(&self.inner.pool, workspace_id)
            .await?
            .ok_or_else(|| AppError::not_found("workspace not found"))?;
        let (sender, receiver) = broadcast::channel(BROADCAST_CAPACITY);
        streams.insert(
            workspace_id,
            WorkspaceStream {
                sender,
                dispatched: head,
            },
        );
        Ok(receiver)
    }
}

impl DispatcherTasks {
    /// Waits for the dispatcher tasks to finish, reporting any that failed.
    pub async fn join(mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "dispatcher task failed");
            }
        }
    }
}

/// Forwards Postgres notifications to the dispatch loop, reconnecting as needed.
async fn listen_loop(inner: Arc<Inner>) {
    let mut backoff = Duration::from_millis(100);
    while !inner.shutdown.is_cancelled() {
        match PgListener::connect_with(&inner.pool).await {
            Ok(mut listener) => match listener.listen(NOTIFY_CHANNEL).await {
                Ok(()) => {
                    backoff = Duration::from_millis(100);
                    // Anything committed while we were not listening is caught by the loop below.
                    inner.wake.notify_one();
                    loop {
                        tokio::select! {
                            () = inner.shutdown.cancelled() => return,
                            received = listener.recv() => match received {
                                Ok(_) => inner.wake.notify_one(),
                                Err(error) => {
                                    tracing::warn!(%error, "event listener lost; reconnecting");
                                    break;
                                }
                            },
                        }
                    }
                }
                Err(error) => tracing::warn!(%error, "cannot listen for events"),
            },
            Err(error) => tracing::warn!(%error, "cannot connect event listener"),
        }
        tokio::select! {
            () = inner.shutdown.cancelled() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn dispatch_loop(inner: Arc<Inner>) {
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = inner.shutdown.cancelled() => return,
            () = inner.wake.notified() => {}
            _ = ticker.tick() => {}
        }
        let workspaces: Vec<WorkspaceId> = inner.streams.lock().await.keys().copied().collect();
        for workspace_id in workspaces {
            // A failure in one workspace must not stall the others; it is retried next tick.
            loop {
                match dispatch_once(&inner, workspace_id).await {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(error) => {
                        tracing::warn!(%workspace_id, %error, "dispatch failed; will retry");
                        break;
                    }
                }
            }
        }
    }
}

/// Publishes the next page of one workspace's log. Returns whether more may be waiting.
async fn dispatch_once(inner: &Inner, workspace_id: WorkspaceId) -> Result<bool, AppError> {
    let mut streams = inner.streams.lock().await;
    let Some(stream) = streams.get_mut(&workspace_id) else {
        return Ok(false);
    };
    let page =
        events::fetch_after(&inner.pool, workspace_id, stream.dispatched, BATCH_SIZE).await?;
    // If pruning has overtaken the dispatcher, events up to the floor can no longer be published.
    // The batch then starts at the floor, which is not contiguous with what any lagging
    // subscriber has, so they recover through a replay and are told to take a fresh snapshot.
    let after = stream.dispatched.max(page.floor);
    if page.scanned == 0 {
        stream.dispatched = after;
        return Ok(false);
    }
    let batch = Batch {
        workspace_id,
        after,
        last_seq: page.scanned_to,
        events: page.events,
    };
    stream.dispatched = page.scanned_to;
    // An error only means nobody is subscribed right now.
    let _ = stream.sender.send(Arc::new(batch));
    Ok(page.scanned == BATCH_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `count` decodable events for a new workspace without going through the services,
    /// so the test controls exactly what the log and the floor look like.
    async fn workspace_with_events(pool: &PgPool, count: i64) -> WorkspaceId {
        let workspace_id = WorkspaceId::generate();
        sqlx::query("INSERT INTO workspaces (id, name, event_head) VALUES ($1, 'W', $2)")
            .bind(workspace_id.as_uuid())
            .bind(count)
            .execute(pool)
            .await
            .unwrap();
        let payload = serde_json::json!({
            "type": "member_joined",
            "user": {
                "id": uuid::Uuid::now_v7(),
                "username": "someone",
                "display_name": "Someone"
            }
        });
        sqlx::query(
            "INSERT INTO events (workspace_id, seq, occurred_at, kind, payload) \
             SELECT $1, g, now(), 'member_joined', $3 FROM generate_series(1, $2::bigint) g",
        )
        .bind(workspace_id.as_uuid())
        .bind(count)
        .bind(sqlx::types::Json(payload))
        .execute(pool)
        .await
        .unwrap();
        workspace_id
    }

    /// A dispatcher core with no background tasks, so the test drives it step by step.
    fn idle(pool: PgPool) -> Inner {
        Inner {
            pool,
            streams: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            shutdown: CancellationToken::new(),
        }
    }

    #[sqlx::test]
    async fn batches_are_contiguous_with_the_previous_batch(pool: PgPool) {
        let workspace_id = workspace_with_events(&pool, 450).await;
        let inner = idle(pool);
        let (sender, mut receiver) = broadcast::channel(16);
        inner.streams.lock().await.insert(
            workspace_id,
            WorkspaceStream {
                sender,
                dispatched: EventSeq::ZERO,
            },
        );

        let mut covered = Vec::new();
        while dispatch_once(&inner, workspace_id).await.unwrap() {}
        while let Ok(batch) = receiver.try_recv() {
            covered.push((batch.after.get(), batch.last_seq.get()));
        }
        assert_eq!(covered, vec![(0, 200), (200, 400), (400, 450)]);
    }

    #[sqlx::test]
    async fn a_dispatcher_overtaken_by_pruning_publishes_from_the_floor(pool: PgPool) {
        let workspace_id = workspace_with_events(&pool, 10).await;
        // Events 1..=6 are pruned while the dispatcher is still at position 3.
        sqlx::query("DELETE FROM events WHERE workspace_id = $1 AND seq <= 6")
            .bind(workspace_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET event_floor = 6 WHERE id = $1")
            .bind(workspace_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let inner = idle(pool);
        let (sender, mut receiver) = broadcast::channel(16);
        inner.streams.lock().await.insert(
            workspace_id,
            WorkspaceStream {
                sender,
                dispatched: EventSeq::new(3).unwrap(),
            },
        );

        assert!(!dispatch_once(&inner, workspace_id).await.unwrap());
        let batch = receiver.try_recv().expect("a batch was published");
        // The batch does not claim to be contiguous with position 3: a subscriber at 3 sees
        // `after (6) > cursor (3)` and recovers through a replay, which tells it to bootstrap.
        assert_eq!((batch.after.get(), batch.last_seq.get()), (6, 10));
        let positions: Vec<i64> = batch.events.iter().map(|event| event.seq.get()).collect();
        assert_eq!(positions, vec![7, 8, 9, 10]);
        assert!(receiver.try_recv().is_err(), "nothing more to publish");
    }

    #[sqlx::test]
    async fn the_cursor_advances_to_the_floor_when_no_events_remain_above_it(pool: PgPool) {
        let workspace_id = workspace_with_events(&pool, 5).await;
        sqlx::query("DELETE FROM events WHERE workspace_id = $1")
            .bind(workspace_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE workspaces SET event_floor = 5 WHERE id = $1")
            .bind(workspace_id.as_uuid())
            .execute(&pool)
            .await
            .unwrap();
        let inner = idle(pool);
        let (sender, mut receiver) = broadcast::channel(16);
        inner.streams.lock().await.insert(
            workspace_id,
            WorkspaceStream {
                sender,
                dispatched: EventSeq::ZERO,
            },
        );

        assert!(!dispatch_once(&inner, workspace_id).await.unwrap());
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            inner.streams.lock().await[&workspace_id].dispatched.get(),
            5
        );
    }
}
