//! Event-log retention.
//!
//! The pruner removes old events from the front of each workspace's log and records how far it
//! got in `workspaces.event_floor`. Pruning only ever removes a **prefix** of the log, never
//! renumbers anything, and never touches the newest `keep_newest` events, so the properties the
//! rest of the server relies on keep holding:
//!
//! * positions above the floor stay gapless and commit-ordered up to `event_head`;
//! * a writer's transaction still holds the workspace row lock from its position bump until it
//!   commits, and the pruner takes the same lock, so pruning and writing never interleave;
//! * deleting the prefix and raising the floor commit together, so no reader's snapshot can see
//!   events missing without also seeing the raised floor ([`crate::events::fetch_after`]).
//!
//! A cursor is replayable only if it is at or above the floor; a subscriber whose cursor has
//! fallen below it is told to bootstrap again (`snapshot_required`, `cursor_expired`).

use lazy_domain::{EventSeq, WorkspaceId};
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::RetentionPolicy;
use crate::error::AppError;
use crate::logging::Describe;

/// Most events deleted by one transaction. Large backlogs are pruned in several transactions so
/// that writers, who wait on the workspace row lock, are never stalled for long.
const CHUNK: i64 = 5_000;

/// What pruning one workspace achieved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub workspace_id: WorkspaceId,
    /// Events deleted by this run.
    pub deleted: u64,
    /// The workspace's floor after the run.
    pub floor: EventSeq,
}

/// Prunes every workspace according to `policy`.
///
/// Does nothing if the policy is disabled. Cancellation is honored after the current chunk and
/// between workspaces; a pass always completes at least one chunk of its first workspace, so a
/// server that is started and immediately stopped still makes progress.
pub async fn prune_all(
    pool: &PgPool,
    policy: RetentionPolicy,
    stop: &CancellationToken,
) -> Result<Vec<PruneReport>, AppError> {
    if !policy.enabled() {
        return Ok(Vec::new());
    }
    let workspaces: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM workspaces ORDER BY created_at, id")
            .fetch_all(pool)
            .await?;
    let mut reports = Vec::with_capacity(workspaces.len());
    for id in workspaces {
        reports.push(prune_workspace(pool, WorkspaceId::from_uuid(id), policy, stop).await?);
        if stop.is_cancelled() {
            break;
        }
    }
    Ok(reports)
}

/// Prunes one workspace, in as many bounded transactions as it takes.
pub async fn prune_workspace(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    policy: RetentionPolicy,
    stop: &CancellationToken,
) -> Result<PruneReport, AppError> {
    let mut deleted = 0u64;
    let mut floor = EventSeq::ZERO;
    loop {
        let chunk = prune_chunk(pool, workspace_id, policy).await?;
        deleted += chunk.deleted;
        floor = chunk.floor.max(floor);
        if !chunk.more || stop.is_cancelled() {
            break;
        }
    }
    Ok(PruneReport {
        workspace_id,
        deleted,
        floor,
    })
}

struct Chunk {
    deleted: u64,
    floor: EventSeq,
    /// Whether the chunk was full, so another may find more to delete.
    more: bool,
}

/// One transaction: lock the workspace, delete a prefix of old events, raise the floor.
async fn prune_chunk(
    pool: &PgPool,
    workspace_id: WorkspaceId,
    policy: RetentionPolicy,
) -> Result<Chunk, AppError> {
    let mut tx = pool.begin().await?;
    // FOR NO KEY UPDATE is the lock a writer's `UPDATE workspaces SET event_head` takes, so the
    // two exclude each other; unlike FOR UPDATE it does not block the foreign-key checks that
    // inserts into other tables perform against this row.
    let row: Option<(i64, i64)> = sqlx::query_as(
        "SELECT event_head, event_floor FROM workspaces WHERE id = $1 FOR NO KEY UPDATE",
    )
    .bind(workspace_id.as_uuid())
    .fetch_optional(&mut *tx)
    .await?;
    let Some((head, floor)) = row else {
        return Ok(Chunk {
            deleted: 0,
            floor: EventSeq::ZERO,
            more: false,
        });
    };
    let current = EventSeq::new(floor).unwrap_or(EventSeq::ZERO);

    // Positions above `ceiling` are the newest `keep_newest` events and are never pruned.
    let ceiling = head.saturating_sub(i64::from(policy.keep_newest));
    if ceiling <= floor {
        return Ok(Chunk {
            deleted: 0,
            floor: current,
            more: false,
        });
    }

    // Of the oldest `CHUNK` surviving candidates, the newest one that has aged out. Everything
    // up to it is deleted, so the floor always names the end of a contiguous deleted prefix.
    let new_floor: Option<i64> = sqlx::query_scalar(
        "SELECT max(seq) FROM ( \
             SELECT seq, occurred_at FROM events \
             WHERE workspace_id = $1 AND seq > $2 AND seq <= $3 ORDER BY seq LIMIT $4 \
         ) oldest WHERE occurred_at < now() - make_interval(days => $5::int)",
    )
    .bind(workspace_id.as_uuid())
    .bind(floor)
    .bind(ceiling)
    .bind(CHUNK)
    .bind(i32::try_from(policy.days).unwrap_or(i32::MAX))
    .fetch_one(&mut *tx)
    .await?;
    let Some(new_floor) = new_floor else {
        return Ok(Chunk {
            deleted: 0,
            floor: current,
            more: false,
        });
    };

    let deleted = sqlx::query("DELETE FROM events WHERE workspace_id = $1 AND seq <= $2")
        .bind(workspace_id.as_uuid())
        .bind(new_floor)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // `seq > floor` above guarantees `new_floor > floor`, so the floor only ever rises.
    sqlx::query("UPDATE workspaces SET event_floor = $2 WHERE id = $1")
        .bind(workspace_id.as_uuid())
        .bind(new_floor)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    Ok(Chunk {
        deleted,
        floor: EventSeq::new(new_floor).unwrap_or(current),
        more: i64::try_from(deleted).unwrap_or(i64::MAX) >= CHUNK,
    })
}

/// A running pruner. Cancel the token passed to [`spawn`], then [`PrunerHandle::join`].
pub struct PrunerHandle {
    passes: watch::Receiver<u64>,
    tasks: JoinSet<()>,
}

impl PrunerHandle {
    /// A counter of completed passes, for waiting on the pruner without sleeping.
    pub fn passes(&self) -> watch::Receiver<u64> {
        self.passes.clone()
    }

    /// Waits for the pruner task to finish, reporting a failure.
    pub async fn join(mut self) {
        while let Some(result) = self.tasks.join_next().await {
            if let Err(error) = result {
                tracing::error!(error = %error.describe(), "pruner task failed");
            }
        }
    }
}

/// Starts the pruner: one pass immediately, then one every `policy.interval`, until `shutdown`.
///
/// Returns a handle even when the policy is disabled; its task then ends at once.
pub fn spawn(pool: PgPool, policy: RetentionPolicy, shutdown: CancellationToken) -> PrunerHandle {
    let (completed, passes) = watch::channel(0u64);
    let mut tasks = JoinSet::new();
    if policy.enabled() {
        tasks.spawn(async move {
            loop {
                match prune_all(&pool, policy, &shutdown).await {
                    Ok(reports) => {
                        let deleted: u64 = reports.iter().map(|report| report.deleted).sum();
                        if deleted > 0 {
                            tracing::info!(deleted, "pruned old events");
                        }
                    }
                    Err(error) => tracing::warn!(error = %error.describe(), "event pruning failed; will retry"),
                }
                completed.send_modify(|count| *count += 1);
                tokio::select! {
                    () = shutdown.cancelled() => return,
                    () = tokio::time::sleep(policy.interval) => {}
                }
            }
        });
    }
    PrunerHandle { passes, tasks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_disabled_policy_spawns_nothing_and_joins_immediately() {
        // The pool is never used when pruning is disabled, so a lazily connected one is enough.
        let pool = PgPool::connect_lazy("postgres://nobody@127.0.0.1:1/none").unwrap();
        let handle = spawn(pool, RetentionPolicy::disabled(), CancellationToken::new());
        assert_eq!(*handle.passes().borrow(), 0);
        handle.join().await;
    }
}
