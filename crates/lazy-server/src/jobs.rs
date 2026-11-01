//! Durable jobs in Postgres (llm-docs/DECISIONS.md, D31): work that happens outside a request and
//! must survive a restart, such as handling a provider's event or posting to another service.
//!
//! # Delivery
//!
//! A job is **at least once**. A worker claims it with a lease (`lease_owner`, `lease_until`) and
//! renews the lease while the handler runs. If the worker dies, the lease runs out and another
//! worker (or the same one after a restart) claims the job again. So a handler must be idempotent,
//! or, when it cannot tell whether its effect happened (a request to another service timed out),
//! answer [`Outcome::Unknown`] instead of guessing.
//!
//! # Outcomes and failure classes
//!
//! - [`Outcome::Done`] ends `succeeded`.
//! - [`Outcome::Retry`] is retryable (a timeout before anything was sent, a 5xx, a rate limit). It runs
//!   again after a backoff, until `max_attempts`; then it is `failed`, and visible.
//! - [`Outcome::Fail`] is definitive (the request is wrong, the credential was revoked). `failed` at
//!   once.
//! - [`Outcome::Unknown`] means the outcome is unknown. `unknown`, and nothing retries it by itself:
//!   a person (or a handler's reconciliation) decides with [`retry`].
//!
//! A lease that runs out on the job's last attempt is `unknown` too: the worker stopped while it
//! ran, so whether its effect happened is not known.
//!
//! # Workers
//!
//! [`spawn`] runs a worker in the server process (the default) or in `lazy-server worker`, a
//! process of its own for isolation. It claims with `FOR UPDATE SKIP LOCKED`, so any number of
//! workers share one queue, and runs at most `concurrency` jobs at once. It wakes when this process
//! enqueues ([`Worker::notify`]) and otherwise polls. On shutdown it stops claiming, lets running
//! jobs finish for up to `drain`, and puts any still running back in the queue (their lease
//! released, so they run again at once elsewhere).
//!
//! # Logging
//!
//! A job is logged by id, kind, attempt, and failure class, never by its payload or error text.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use lazy_domain::WorkspaceId;
use sqlx::{PgConnection, PgPool};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::error::AppError;
use crate::logging::Describe;

/// How long finished jobs are kept before cleanup deletes them.
const FINISHED_KEPT: Duration = Duration::from_secs(14 * 24 * 60 * 60);
/// The longest error text kept for a job.
const MAX_ERROR_CHARS: usize = 500;

/// A job to enqueue.
#[derive(Debug, Clone)]
pub struct NewJob {
    pub kind: &'static str,
    pub workspace: Option<WorkspaceId>,
    /// A second enqueue with the same kind and key is the same job (a provider's delivery id).
    pub dedupe_key: Option<String>,
    pub payload: serde_json::Value,
    pub max_attempts: u32,
    /// Do not run before this long from now.
    pub delay: Duration,
}

impl NewJob {
    pub fn new(kind: &'static str, payload: serde_json::Value) -> Self {
        Self {
            kind,
            workspace: None,
            dedupe_key: None,
            payload,
            max_attempts: 8,
            delay: Duration::ZERO,
        }
    }

    pub fn workspace(mut self, workspace: WorkspaceId) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn dedupe(mut self, key: impl Into<String>) -> Self {
        self.dedupe_key = Some(key.into());
        self
    }

    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }

    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// What enqueueing did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub id: i64,
    /// False when the dedupe key matched a job already there (which is left as it is).
    pub created: bool,
}

/// Adds a job, on the caller's connection so it commits (or not) with the work that wants it.
pub async fn enqueue(conn: &mut PgConnection, job: NewJob) -> Result<Enqueued, AppError> {
    let delay_ms = i64::try_from(job.delay.as_millis()).unwrap_or(i64::MAX);
    let inserted: Option<i64> = sqlx::query_scalar(
        "INSERT INTO jobs (kind, workspace_id, dedupe_key, payload, max_attempts, run_after) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6::double precision / 1000)) \
         ON CONFLICT (kind, dedupe_key) DO NOTHING RETURNING id",
    )
    .bind(job.kind)
    .bind(job.workspace.map(|w| w.as_uuid()))
    .bind(&job.dedupe_key)
    .bind(&job.payload)
    .bind(i32::try_from(job.max_attempts).unwrap_or(i32::MAX).max(1))
    .bind(delay_ms)
    .fetch_optional(&mut *conn)
    .await?;
    if let Some(id) = inserted {
        return Ok(Enqueued { id, created: true });
    }
    let id: i64 = sqlx::query_scalar("SELECT id FROM jobs WHERE kind = $1 AND dedupe_key = $2")
        .bind(job.kind)
        .bind(&job.dedupe_key)
        .fetch_one(conn)
        .await?;
    Ok(Enqueued { id, created: false })
}

/// What a handler says about one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Done,
    /// Try again later. `after` overrides the backoff (a provider's `Retry-After`).
    Retry {
        error: String,
        after: Option<Duration>,
    },
    /// Do not try again.
    Fail {
        error: String,
    },
    /// It may or may not have happened; do not retry by yourself.
    Unknown {
        error: String,
    },
}

impl Outcome {
    pub fn retry(error: impl Into<String>) -> Self {
        Self::Retry {
            error: error.into(),
            after: None,
        }
    }

    pub fn fail(error: impl Into<String>) -> Self {
        Self::Fail {
            error: error.into(),
        }
    }

    pub fn unknown(error: impl Into<String>) -> Self {
        Self::Unknown {
            error: error.into(),
        }
    }
}

/// What a handler is given.
#[derive(Debug, Clone)]
pub struct JobContext {
    pub id: i64,
    pub kind: String,
    pub workspace: Option<WorkspaceId>,
    pub payload: serde_json::Value,
    /// 1 for the first attempt.
    pub attempt: u32,
    pub max_attempts: u32,
    pub pool: PgPool,
}

/// Runs one attempt of a kind of job.
pub type Handler = Arc<dyn Fn(JobContext) -> BoxFuture<'static, Outcome> + Send + Sync>;

/// The handlers a worker runs, by kind. A claimed job whose kind has no handler here is put back
/// (another process, perhaps a newer one, may have it) after a short delay.
#[derive(Clone, Default)]
pub struct Registry {
    handlers: HashMap<&'static str, Handler>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on<F, Fut>(mut self, kind: &'static str, handler: F) -> Self
    where
        F: Fn(JobContext) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Outcome> + Send + 'static,
    {
        self.handlers
            .insert(kind, Arc::new(move |ctx| Box::pin(handler(ctx))));
        self
    }

    pub fn kinds(&self) -> Vec<&'static str> {
        let mut kinds: Vec<_> = self.handlers.keys().copied().collect();
        kinds.sort_unstable();
        kinds
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }
}

/// How a worker runs.
#[derive(Debug, Clone, Copy)]
pub struct WorkerOptions {
    /// Jobs at once.
    pub concurrency: usize,
    /// How long a claim lasts without renewal; renewed every third of it.
    pub lease: Duration,
    /// How often the queue is looked at when nothing wakes the worker.
    pub poll: Duration,
    /// The first retry's delay; it doubles with each attempt, up to `backoff_cap`, with up to a
    /// fifth of jitter either way.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// How long running jobs may finish after shutdown begins.
    pub drain: Duration,
}

impl Default for WorkerOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            lease: Duration::from_secs(60),
            poll: Duration::from_secs(2),
            backoff_base: Duration::from_secs(10),
            backoff_cap: Duration::from_secs(60 * 60),
            drain: Duration::from_secs(10),
        }
    }
}

/// The delay before attempt `attempt + 1`, after `attempt` failed. `jitter` is in [-1, 1].
pub fn backoff(options: &WorkerOptions, attempt: u32, jitter: f64) -> Duration {
    let exponent = attempt.saturating_sub(1).min(30);
    let base = options
        .backoff_base
        .saturating_mul(1u32 << exponent)
        .min(options.backoff_cap);
    let factor = 1.0 + 0.2 * jitter.clamp(-1.0, 1.0);
    base.mul_f64(factor).min(options.backoff_cap)
}

fn jitter() -> f64 {
    let mut bytes = [0u8; 4];
    if getrandom::fill(&mut bytes).is_err() {
        return 0.0;
    }
    f64::from(u32::from_le_bytes(bytes)) / f64::from(u32::MAX) * 2.0 - 1.0
}

fn clip(error: &str) -> String {
    error.chars().take(MAX_ERROR_CHARS).collect()
}

/// A running worker.
pub struct Worker {
    notify: Arc<Notify>,
    tasks: JoinSet<()>,
}

/// A handle that wakes a worker after an enqueue, so the job runs at once rather than at the next
/// poll. Cheap to clone; does nothing when no worker runs in this process.
#[derive(Clone, Default)]
pub struct Waker(Option<Arc<Notify>>);

impl Waker {
    pub fn none() -> Self {
        Self(None)
    }

    pub fn wake(&self) {
        if let Some(notify) = &self.0 {
            notify.notify_one();
        }
    }
}

impl Worker {
    pub fn waker(&self) -> Waker {
        Waker(Some(Arc::clone(&self.notify)))
    }

    /// Waits for the worker to stop (after its token is cancelled).
    pub async fn join(mut self) {
        while self.tasks.join_next().await.is_some() {}
    }
}

/// Starts a worker. It stops when `stop` is cancelled; [`Worker::join`] waits for it.
pub fn spawn(
    pool: PgPool,
    registry: Registry,
    options: WorkerOptions,
    stop: CancellationToken,
) -> Worker {
    let notify = Arc::new(Notify::new());
    let mut tasks = JoinSet::new();
    let owner = Uuid::new_v4();
    tasks.spawn(run(
        pool,
        Arc::new(registry),
        options,
        owner,
        Arc::clone(&notify),
        stop,
    ));
    Worker { notify, tasks }
}

#[derive(sqlx::FromRow)]
struct Claimed {
    id: i64,
    kind: String,
    workspace_id: Option<Uuid>,
    payload: serde_json::Value,
    attempts: i32,
    max_attempts: i32,
}

async fn run(
    pool: PgPool,
    registry: Arc<Registry>,
    options: WorkerOptions,
    owner: Uuid,
    notify: Arc<Notify>,
    stop: CancellationToken,
) {
    let slots = Arc::new(Semaphore::new(options.concurrency.max(1)));
    let mut running: JoinSet<()> = JoinSet::new();
    let mut last_cleanup = tokio::time::Instant::now();
    tracing::info!(job_kinds = registry.kinds().len(), "job worker started");
    loop {
        while running.try_join_next().is_some() {}
        if stop.is_cancelled() {
            break;
        }
        if last_cleanup.elapsed() > Duration::from_secs(3600) {
            last_cleanup = tokio::time::Instant::now();
            if let Err(error) = cleanup(&pool).await {
                tracing::warn!(error = %error.describe(), "job cleanup failed");
            }
        }
        let free = slots.available_permits();
        let claimed = if free == 0 {
            Vec::new()
        } else {
            match claim(&pool, owner, options.lease, free).await {
                Ok(claimed) => claimed,
                Err(error) => {
                    tracing::warn!(error = %error.describe(), "claiming jobs failed");
                    Vec::new()
                }
            }
        };
        let full = claimed.len() == free && free > 0;
        for job in claimed {
            let Ok(permit) = Arc::clone(&slots).acquire_owned().await else {
                break;
            };
            let pool = pool.clone();
            let registry = Arc::clone(&registry);
            running.spawn(async move {
                execute(pool, registry, options, owner, job).await;
                drop(permit);
            });
        }
        if full {
            // More may be waiting; look again as soon as a slot frees up.
            tokio::select! {
                () = stop.cancelled() => break,
                _ = running.join_next(), if !running.is_empty() => {}
                () = tokio::time::sleep(options.poll) => {}
            }
        } else {
            tokio::select! {
                () = stop.cancelled() => break,
                () = notify.notified() => {}
                _ = running.join_next(), if !running.is_empty() => {}
                () = tokio::time::sleep(options.poll) => {}
            }
        }
    }
    // Draining: let what runs finish, for a while.
    let drained = tokio::time::timeout(options.drain, async {
        while running.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        running.abort_all();
        while running.join_next().await.is_some() {}
        match release(&pool, owner).await {
            Ok(released) => {
                tracing::warn!(released, "jobs still running at shutdown were put back")
            }
            Err(error) => {
                tracing::warn!(error = %error.describe(), "putting back unfinished jobs failed")
            }
        }
    }
    tracing::info!("job worker stopped");
}

/// Claims up to `limit` jobs that are due, or whose lease ran out. A lease that ran out on the
/// last attempt is settled as `unknown` first, not run again.
async fn claim(
    pool: &PgPool,
    owner: Uuid,
    lease: Duration,
    limit: usize,
) -> Result<Vec<Claimed>, AppError> {
    let lease_ms = i64::try_from(lease.as_millis()).unwrap_or(i64::MAX);
    let settled: Vec<(i64, String)> = sqlx::query_as(
        "UPDATE jobs SET state = 'unknown', lease_owner = NULL, lease_until = NULL, \
                last_error_class = 'unknown', \
                last_error = 'the worker running it stopped, so whether it happened is not known', \
                finished_at = now(), updated_at = now() \
         WHERE id IN (SELECT id FROM jobs WHERE state = 'running' AND lease_until < now() \
                      AND attempts >= max_attempts FOR UPDATE SKIP LOCKED) \
         RETURNING id, kind",
    )
    .fetch_all(pool)
    .await?;
    for (id, kind) in settled {
        tracing::warn!(job_id = id, job_kind = %kind, error_class = "unknown", "job lost its worker on its last attempt");
    }
    let rows: Vec<Claimed> = sqlx::query_as(
        "UPDATE jobs SET state = 'running', lease_owner = $1, \
                lease_until = now() + make_interval(secs => $2::double precision / 1000), \
                attempts = attempts + 1, updated_at = now() \
         WHERE id IN (SELECT id FROM jobs \
                      WHERE (state = 'queued' AND run_after <= now()) \
                         OR (state = 'running' AND lease_until < now()) \
                      ORDER BY run_after, id FOR UPDATE SKIP LOCKED LIMIT $3) \
         RETURNING id, kind, workspace_id, payload, attempts, max_attempts",
    )
    .bind(owner)
    .bind(lease_ms)
    .bind(i64::try_from(limit).unwrap_or(1))
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Runs one claimed job, renewing its lease, and records the outcome. Writes are guarded by the
/// lease owner: a worker that lost its lease (it paused past it, and another claimed the job)
/// records nothing.
async fn execute(
    pool: PgPool,
    registry: Arc<Registry>,
    options: WorkerOptions,
    owner: Uuid,
    job: Claimed,
) {
    let attempt = u32::try_from(job.attempts).unwrap_or(1);
    let max_attempts = u32::try_from(job.max_attempts).unwrap_or(1);
    let Some(handler) = registry.handlers.get(job.kind.as_str()).cloned() else {
        // Not ours: put it back, without using up an attempt.
        let _ = sqlx::query(
            "UPDATE jobs SET state = 'queued', lease_owner = NULL, lease_until = NULL, \
                    attempts = attempts - 1, run_after = now() + interval '30 seconds', \
                    updated_at = now() \
             WHERE id = $1 AND lease_owner = $2",
        )
        .bind(job.id)
        .bind(owner)
        .execute(&pool)
        .await;
        tracing::warn!(job_id = job.id, job_kind = %job.kind, "no handler for this kind of job here");
        return;
    };
    let ctx = JobContext {
        id: job.id,
        kind: job.kind.clone(),
        workspace: job.workspace_id.map(WorkspaceId::from_uuid),
        payload: job.payload,
        attempt,
        max_attempts,
        pool: pool.clone(),
    };
    let lease_ms = i64::try_from(options.lease.as_millis()).unwrap_or(i64::MAX);
    let renew_every = (options.lease / 3).max(Duration::from_millis(50));
    let work = handler(ctx);
    tokio::pin!(work);
    let outcome = loop {
        tokio::select! {
            outcome = &mut work => break outcome,
            () = tokio::time::sleep(renew_every) => {
                let renewed = sqlx::query(
                    "UPDATE jobs SET lease_until = now() + make_interval(secs => $3::double precision / 1000) \
                     WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
                )
                .bind(job.id)
                .bind(owner)
                .bind(lease_ms)
                .execute(&pool)
                .await;
                if let Err(error) = renewed {
                    tracing::warn!(job_id = job.id, error = %AppError::from(error).describe(), "renewing a job's lease failed");
                }
            }
        }
    };
    let result = settle(
        &pool,
        &options,
        owner,
        job.id,
        attempt,
        max_attempts,
        &outcome,
    )
    .await;
    match (&outcome, result) {
        (_, Err(error)) => {
            tracing::warn!(job_id = job.id, error = %error.describe(), "recording a job's outcome failed")
        }
        (_, Ok(false)) => tracing::warn!(
            job_id = job.id,
            job_kind = %job.kind,
            "a job's lease was lost before it finished; its outcome was not recorded"
        ),
        (Outcome::Done, Ok(true)) => {
            tracing::debug!(job_id = job.id, job_kind = %job.kind, attempt, "job succeeded")
        }
        (Outcome::Retry { .. }, Ok(true)) => tracing::info!(
            job_id = job.id,
            job_kind = %job.kind,
            attempt,
            error_class = "retryable",
            "job attempt failed"
        ),
        (Outcome::Fail { .. }, Ok(true)) => tracing::warn!(
            job_id = job.id,
            job_kind = %job.kind,
            attempt,
            error_class = "definitive",
            "job failed"
        ),
        (Outcome::Unknown { .. }, Ok(true)) => tracing::warn!(
            job_id = job.id,
            job_kind = %job.kind,
            attempt,
            error_class = "unknown",
            "job outcome unknown"
        ),
    }
}

/// Records an attempt's outcome. Returns false if the lease was no longer this worker's.
async fn settle(
    pool: &PgPool,
    options: &WorkerOptions,
    owner: Uuid,
    id: i64,
    attempt: u32,
    max_attempts: u32,
    outcome: &Outcome,
) -> Result<bool, AppError> {
    let (state, class, error, delay) = match outcome {
        Outcome::Done => ("succeeded", None, None, None),
        Outcome::Retry { error, after } if attempt < max_attempts => (
            "queued",
            Some("retryable"),
            Some(clip(error)),
            Some(after.unwrap_or_else(|| backoff(options, attempt, jitter()))),
        ),
        Outcome::Retry { error, .. } => ("failed", Some("retryable"), Some(clip(error)), None),
        Outcome::Fail { error } => ("failed", Some("definitive"), Some(clip(error)), None),
        Outcome::Unknown { error } => ("unknown", Some("unknown"), Some(clip(error)), None),
    };
    let delay_ms = delay.map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    let updated = sqlx::query(
        "UPDATE jobs SET state = $3, lease_owner = NULL, lease_until = NULL, \
                last_error_class = COALESCE($4, last_error_class), \
                last_error = CASE WHEN $3 = 'succeeded' THEN last_error ELSE $5 END, \
                run_after = CASE WHEN $6::bigint IS NULL THEN run_after \
                            ELSE now() + make_interval(secs => $6::double precision / 1000) END, \
                finished_at = CASE WHEN $3 IN ('succeeded', 'failed', 'unknown') THEN now() END, \
                updated_at = now() \
         WHERE id = $1 AND lease_owner = $2 AND state = 'running'",
    )
    .bind(id)
    .bind(owner)
    .bind(state)
    .bind(class)
    .bind(error)
    .bind(delay_ms)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// Puts this worker's running jobs back in the queue (shutdown, after the drain ran out). The
/// attempt they were on counts.
async fn release(pool: &PgPool, owner: Uuid) -> Result<u64, AppError> {
    let released = sqlx::query(
        "UPDATE jobs SET state = 'queued', lease_owner = NULL, lease_until = NULL, \
                run_after = now(), updated_at = now() \
         WHERE lease_owner = $1 AND state = 'running'",
    )
    .bind(owner)
    .execute(pool)
    .await?;
    Ok(released.rows_affected())
}

/// Deletes finished jobs older than the time they are kept.
pub async fn cleanup(pool: &PgPool) -> Result<u64, AppError> {
    let seconds = FINISHED_KEPT.as_secs_f64();
    let deleted = sqlx::query(
        "DELETE FROM jobs WHERE finished_at IS NOT NULL \
         AND finished_at < now() - make_interval(secs => $1)",
    )
    .bind(seconds)
    .execute(pool)
    .await?;
    Ok(deleted.rows_affected())
}

/// Runs a `failed` or `unknown` job again, now, with its attempts counted from zero. Returns false
/// if the job is not in one of those states (it is queued, running, done, or gone).
pub async fn retry(pool: &PgPool, id: i64) -> Result<bool, AppError> {
    let updated = sqlx::query(
        "UPDATE jobs SET state = 'queued', attempts = 0, run_after = now(), finished_at = NULL, \
                updated_at = now() \
         WHERE id = $1 AND state IN ('failed', 'unknown')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(updated.rows_affected() == 1)
}

/// A job as admins and tests see it.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct JobRow {
    pub id: i64,
    pub kind: String,
    pub state: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub last_error_class: Option<String>,
    pub last_error: Option<String>,
}

pub async fn get(pool: &PgPool, id: i64) -> Result<Option<JobRow>, AppError> {
    Ok(sqlx::query_as(
        "SELECT id, kind, state, attempts, max_attempts, last_error_class, last_error \
         FROM jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_with_jitter_and_stops_at_the_cap() {
        let options = WorkerOptions {
            backoff_base: Duration::from_secs(10),
            backoff_cap: Duration::from_secs(100),
            ..WorkerOptions::default()
        };
        assert_eq!(backoff(&options, 1, 0.0), Duration::from_secs(10));
        assert_eq!(backoff(&options, 2, 0.0), Duration::from_secs(20));
        assert_eq!(backoff(&options, 3, 0.0), Duration::from_secs(40));
        assert_eq!(backoff(&options, 9, 0.0), Duration::from_secs(100));
        assert_eq!(backoff(&options, 1, 1.0), Duration::from_secs(12));
        assert_eq!(backoff(&options, 1, -1.0), Duration::from_secs(8));
        assert_eq!(backoff(&options, 40, 1.0), Duration::from_secs(100));
        assert!((-1.0..=1.0).contains(&jitter()));
    }

    #[test]
    fn errors_are_clipped() {
        assert_eq!(clip(&"x".repeat(2000)).chars().count(), MAX_ERROR_CHARS);
    }
}
