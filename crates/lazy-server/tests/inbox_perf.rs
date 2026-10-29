//! Measuring the inbox on a large database (`cargo test --release -p lazy-server --test inbox_perf
//! -- --ignored --nocapture`). Not run with the suite: it needs `LAZY_INBOX_PERF_DB`, a database
//! seeded with `tests/fixtures/inbox_perf_seed.sql` (a person following 20,000 of 60,000 threads).

use std::time::Instant;

use lazy_domain::{UserId, WorkspaceId};
use lazy_protocol::InboxQuery;
use sqlx::postgres::PgPoolOptions;

fn ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[tokio::test]
#[ignore = "needs a large seeded database in LAZY_INBOX_PERF_DB"]
async fn the_inbox_and_read_all_on_twenty_thousand_followed_threads() {
    let url = std::env::var("LAZY_INBOX_PERF_DB").expect("LAZY_INBOX_PERF_DB");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let workspace: WorkspaceId = "01a00000-0000-7000-8000-0000000000a0".parse().unwrap();
    let me: UserId = "01a00000-0000-7000-8000-000000000001".parse().unwrap();

    let run = |label: &'static str, query: InboxQuery| {
        let pool = pool.clone();
        async move {
            let mut times = Vec::new();
            let mut shape = (0, false);
            for _ in 0..5 {
                let started = Instant::now();
                let page = lazy_server::notifications::inbox(&pool, me, workspace, query.clone())
                    .await
                    .unwrap();
                times.push(ms(started));
                shape = (page.entries.len(), page.has_more);
            }
            let mut rest = times[1..].to_vec();
            rest.sort_by(|a, b| a.total_cmp(b));
            println!(
                "{label:<34} first {:>7.1} ms  median {:>7.1} ms  entries {} has_more {}",
                times[0], rest[1], shape.0, shape.1
            );
            shape
        }
    };
    println!();
    run("first page (20)", InboxQuery::default()).await;
    run(
        "first page, 50",
        InboxQuery {
            limit: Some(50),
            ..InboxQuery::default()
        },
    )
    .await;
    run(
        "unread only, first page",
        InboxQuery {
            unread: Some(true),
            ..InboxQuery::default()
        },
    )
    .await;
    // The newest reply position of thread n is 5n + 3; the person follows multiples of three.
    for (label, n) in [
        ("deep page (middle)", 30_000i64),
        ("deep page (near the end)", 600i64),
    ] {
        let before = lazy_domain::EventSeq::new(n * 5 + 3).unwrap();
        run(
            label,
            InboxQuery {
                before: Some(before),
                ..InboxQuery::default()
            },
        )
        .await;
        run(
            &*Box::leak(format!("{label}, unread only").into_boxed_str()),
            InboxQuery {
                before: Some(before),
                unread: Some(true),
                ..InboxQuery::default()
            },
        )
        .await;
    }
    println!();
    for round in 1..=3 {
        let started = Instant::now();
        let done = lazy_server::notifications::mark_inbox_read(&pool, me, workspace)
            .await
            .unwrap();
        println!(
            "read all, round {round}: {} threads in {:.1} ms",
            done.updated,
            ms(started)
        );
    }
}
