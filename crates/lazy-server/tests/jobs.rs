//! The durable job queue (D31): enqueueing and deduplication, running at most `concurrency` at
//! once, the three failure classes, retries with backoff until the last attempt, a worker that dies
//! mid-job, shutdown draining, and a job no handler here knows.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use lazy_server::jobs::{self, JobRow, NewJob, Outcome, Registry, WorkerOptions};
use serde_json::json;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

fn fast() -> WorkerOptions {
    WorkerOptions {
        concurrency: 4,
        lease: Duration::from_millis(600),
        poll: Duration::from_millis(50),
        backoff_base: Duration::from_millis(50),
        backoff_cap: Duration::from_millis(200),
        drain: Duration::from_millis(500),
    }
}

async fn enqueue(pool: &PgPool, job: NewJob) -> i64 {
    let mut conn = pool.acquire().await.unwrap();
    jobs::enqueue(&mut conn, job).await.unwrap().id
}

async fn row(pool: &PgPool, id: i64) -> JobRow {
    jobs::get(pool, id).await.unwrap().expect("the job exists")
}

/// Waits (bounded) until the job is in `state`.
async fn until_state(pool: &PgPool, id: i64, state: &str) -> JobRow {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let job = row(pool, id).await;
        if job.state == state {
            return job;
        }
        assert!(
            Instant::now() < deadline,
            "job {id} is {} after waiting for {state}: {job:?}",
            job.state
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[sqlx::test]
async fn a_job_runs_once_and_a_duplicate_enqueue_is_the_same_job(pool: PgPool) {
    let runs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&runs);
    let registry = Registry::new().on("test.echo", move |ctx| {
        let seen = Arc::clone(&seen);
        async move {
            assert_eq!(ctx.payload, json!({"n": 1}));
            assert_eq!(ctx.attempt, 1);
            seen.fetch_add(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    let mut conn = pool.acquire().await.unwrap();
    let first = jobs::enqueue(
        &mut conn,
        NewJob::new("test.echo", json!({"n": 1})).dedupe("delivery-1"),
    )
    .await
    .unwrap();
    let again = jobs::enqueue(
        &mut conn,
        NewJob::new("test.echo", json!({"n": 2})).dedupe("delivery-1"),
    )
    .await
    .unwrap();
    assert!(first.created && !again.created);
    assert_eq!(first.id, again.id, "the same delivery is the same job");
    // The same key under another kind is another job.
    let other = jobs::enqueue(
        &mut conn,
        NewJob::new("test.other", json!({})).dedupe("delivery-1"),
    )
    .await
    .unwrap();
    assert_ne!(other.id, first.id);
    drop(conn);

    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    let done = until_state(&pool, first.id, "succeeded").await;
    assert_eq!(done.attempts, 1);
    assert!(done.last_error.is_none());
    stop.cancel();
    worker.join().await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    // The other kind has no handler here: it was put back, without using an attempt.
    let unhandled = row(&pool, other.id).await;
    assert_eq!(
        (unhandled.state.as_str(), unhandled.attempts),
        ("queued", 0)
    );
}

#[sqlx::test]
async fn retryable_failures_back_off_and_end_failed_after_the_last_attempt(pool: PgPool) {
    let registry = Registry::new()
        .on("test.flaky", |ctx| async move {
            if ctx.attempt < 3 {
                Outcome::retry(format!("timed out on attempt {}", ctx.attempt))
            } else {
                Outcome::Done
            }
        })
        .on("test.down", |_| async {
            Outcome::retry("503 from the provider")
        });
    let flaky = enqueue(&pool, NewJob::new("test.flaky", json!({})).max_attempts(5)).await;
    let down = enqueue(&pool, NewJob::new("test.down", json!({})).max_attempts(3)).await;
    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());

    let flaky = until_state(&pool, flaky, "succeeded").await;
    assert_eq!(flaky.attempts, 3, "it ran until it worked");
    assert_eq!(
        flaky.last_error.as_deref(),
        Some("timed out on attempt 2"),
        "the last failure is kept for the record"
    );
    let down = until_state(&pool, down, "failed").await;
    assert_eq!(down.attempts, 3, "no more than max_attempts");
    assert_eq!(down.last_error_class.as_deref(), Some("retryable"));
    assert_eq!(down.last_error.as_deref(), Some("503 from the provider"));
    stop.cancel();
    worker.join().await;
}

#[sqlx::test]
async fn definitive_and_unknown_outcomes_are_not_retried_until_someone_asks(pool: PgPool) {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&attempts);
    let registry = Registry::new()
        .on("test.refused", |_| async {
            Outcome::fail("401: the token was revoked")
        })
        .on("test.lost", move |_| {
            let counted = Arc::clone(&counted);
            async move {
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    Outcome::unknown("timed out after the request was sent")
                } else {
                    Outcome::Done
                }
            }
        });
    let refused = enqueue(&pool, NewJob::new("test.refused", json!({}))).await;
    let lost = enqueue(&pool, NewJob::new("test.lost", json!({}))).await;
    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());

    let refused_row = until_state(&pool, refused, "failed").await;
    assert_eq!(
        (
            refused_row.attempts,
            refused_row.last_error_class.as_deref()
        ),
        (1, Some("definitive"))
    );
    let lost_row = until_state(&pool, lost, "unknown").await;
    assert_eq!(lost_row.last_error_class.as_deref(), Some("unknown"));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "unknown is not retried by itself"
    );
    assert_eq!(row(&pool, lost).await.state, "unknown");

    // A person decides to run it again.
    assert!(jobs::retry(&pool, lost).await.unwrap());
    let rerun = until_state(&pool, lost, "succeeded").await;
    assert_eq!(rerun.attempts, 1, "attempts count from zero again");
    assert!(
        !jobs::retry(&pool, lost).await.unwrap(),
        "a succeeded job is not retried"
    );
    stop.cancel();
    worker.join().await;
}

#[sqlx::test]
async fn no_more_than_concurrency_jobs_run_at_once(pool: PgPool) {
    let now = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let (n, p) = (Arc::clone(&now), Arc::clone(&peak));
    let registry = Registry::new().on("test.slow", move |_| {
        let (n, p) = (Arc::clone(&n), Arc::clone(&p));
        async move {
            let running = n.fetch_add(1, Ordering::SeqCst) + 1;
            p.fetch_max(running, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(80)).await;
            n.fetch_sub(1, Ordering::SeqCst);
            Outcome::Done
        }
    });
    let mut ids = Vec::new();
    for i in 0..12 {
        ids.push(enqueue(&pool, NewJob::new("test.slow", json!({ "i": i }))).await);
    }
    let stop = CancellationToken::new();
    let worker = jobs::spawn(
        pool.clone(),
        registry,
        WorkerOptions {
            concurrency: 3,
            ..fast()
        },
        stop.clone(),
    );
    for id in ids {
        until_state(&pool, id, "succeeded").await;
    }
    stop.cancel();
    worker.join().await;
    let peak = peak.load(Ordering::SeqCst);
    assert!((2..=3).contains(&peak), "peak {peak}");
}

#[sqlx::test]
async fn two_workers_share_a_queue_and_never_run_a_job_twice(pool: PgPool) {
    let runs = Arc::new(std::sync::Mutex::new(Vec::new()));
    let registry = {
        let runs = Arc::clone(&runs);
        Registry::new().on("test.once", move |ctx| {
            let runs = Arc::clone(&runs);
            async move {
                runs.lock().unwrap().push(ctx.id);
                Outcome::Done
            }
        })
    };
    let mut ids = Vec::new();
    for i in 0..40 {
        ids.push(enqueue(&pool, NewJob::new("test.once", json!({ "i": i }))).await);
    }
    let stop = CancellationToken::new();
    let a = jobs::spawn(pool.clone(), registry.clone(), fast(), stop.clone());
    let b = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    for id in &ids {
        until_state(&pool, *id, "succeeded").await;
    }
    stop.cancel();
    a.join().await;
    b.join().await;
    let mut seen = runs.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, ids, "each job ran exactly once");
}

#[sqlx::test]
async fn a_job_whose_worker_died_runs_again_and_on_its_last_attempt_is_unknown(pool: PgPool) {
    // A worker claimed these and died: their leases ran out.
    let again = enqueue(
        &pool,
        NewJob::new("test.recover", json!({})).max_attempts(3),
    )
    .await;
    let last = enqueue(
        &pool,
        NewJob::new("test.recover", json!({})).max_attempts(1),
    )
    .await;
    sqlx::query(
        "UPDATE jobs SET state = 'running', attempts = 1, lease_owner = gen_random_uuid(), \
         lease_until = now() - interval '1 second' WHERE id = ANY($1)",
    )
    .bind(vec![again, last])
    .execute(&pool)
    .await
    .unwrap();
    let registry = Registry::new().on("test.recover", |_| async { Outcome::Done });
    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    let recovered = until_state(&pool, again, "succeeded").await;
    assert_eq!(recovered.attempts, 2, "the lost attempt counts");
    let settled = until_state(&pool, last, "unknown").await;
    assert_eq!(settled.last_error_class.as_deref(), Some("unknown"));
    stop.cancel();
    worker.join().await;
}

#[sqlx::test]
async fn a_long_job_keeps_its_lease_and_is_not_taken_by_another_worker(pool: PgPool) {
    let runs = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&runs);
    let registry = Registry::new().on("test.long", move |_| {
        let counted = Arc::clone(&counted);
        async move {
            counted.fetch_add(1, Ordering::SeqCst);
            // Three leases long: only renewal keeps it.
            tokio::time::sleep(Duration::from_millis(1800)).await;
            Outcome::Done
        }
    });
    let id = enqueue(&pool, NewJob::new("test.long", json!({}))).await;
    let stop = CancellationToken::new();
    let a = jobs::spawn(pool.clone(), registry.clone(), fast(), stop.clone());
    let b = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    let done = until_state(&pool, id, "succeeded").await;
    assert_eq!(done.attempts, 1);
    assert_eq!(runs.load(Ordering::SeqCst), 1, "no second worker took it");
    stop.cancel();
    a.join().await;
    b.join().await;
}

#[sqlx::test]
async fn shutdown_lets_short_jobs_finish_and_puts_long_ones_back(pool: PgPool) {
    let registry = Registry::new()
        .on("test.short", |_| async {
            tokio::time::sleep(Duration::from_millis(150)).await;
            Outcome::Done
        })
        .on("test.endless", |_| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Outcome::Done
        });
    let short = enqueue(&pool, NewJob::new("test.short", json!({}))).await;
    let endless = enqueue(&pool, NewJob::new("test.endless", json!({}))).await;
    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    until_state(&pool, endless, "running").await;
    stop.cancel();
    let started = Instant::now();
    worker.join().await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the drain is bounded"
    );
    assert_eq!(
        row(&pool, short).await.state,
        "succeeded",
        "it finished while draining"
    );
    let back = row(&pool, endless).await;
    assert_eq!(back.state, "queued", "put back for the next worker");
    let lease: Option<uuid::Uuid> =
        sqlx::query_scalar("SELECT lease_owner FROM jobs WHERE id = $1")
            .bind(endless)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(lease.is_none());
}

#[sqlx::test]
async fn a_delayed_job_waits_and_cleanup_removes_only_old_finished_jobs(pool: PgPool) {
    let registry = Registry::new().on("test.later", |_| async { Outcome::Done });
    let id = enqueue(
        &pool,
        NewJob::new("test.later", json!({})).delay(Duration::from_millis(500)),
    )
    .await;
    let stop = CancellationToken::new();
    let worker = jobs::spawn(pool.clone(), registry, fast(), stop.clone());
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(row(&pool, id).await.state, "queued", "not before its time");
    until_state(&pool, id, "succeeded").await;
    stop.cancel();
    worker.join().await;

    assert_eq!(
        jobs::cleanup(&pool).await.unwrap(),
        0,
        "a fresh one is kept"
    );
    sqlx::query("UPDATE jobs SET finished_at = now() - interval '30 days' WHERE id = $1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(jobs::cleanup(&pool).await.unwrap(), 1);
    assert!(jobs::get(&pool, id).await.unwrap().is_none());
}
