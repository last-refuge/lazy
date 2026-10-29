//! Measuring search on a large database (`cargo test --release -p lazy-server --test search_plans
//! -- --ignored --nocapture`). Not run with the suite: it needs `LAZY_SEARCH_PERF_DB`, a database
//! seeded with about a million messages (see llm-docs/SERVER.md, "Search"), and prints the plan
//! and the time of each kind of query for several callers.

use std::time::{Duration, Instant};

use lazy_domain::{UserId, WorkspaceId};
use lazy_protocol::SearchParams;
use lazy_server::SearchOptions;
use sqlx::postgres::PgPoolOptions;

const WORKSPACE: &str = "01a00000-0000-7000-8000-0000000000a0";

fn execution_ms(plan: &str) -> f64 {
    plan.lines()
        .find_map(|line| line.trim().strip_prefix("Execution Time: "))
        .and_then(|rest| rest.trim_end_matches(" ms").parse().ok())
        .unwrap_or(f64::NAN)
}

fn shape(plan: &str) -> String {
    let mut nodes: Vec<String> = Vec::new();
    for line in plan.lines() {
        let line = line.trim_start_matches([' ', '-', '>']).trim();
        for key in [
            "Index Scan",
            "Index Only Scan",
            "Bitmap Heap Scan",
            "Bitmap Index Scan",
            "Seq Scan",
            "Sort",
            "Hash Join",
            "Nested Loop",
            "Merge Join",
            "WindowAgg",
            "Unique",
            "Limit",
            "Merge Append",
        ] {
            if line.starts_with(key) {
                let words: Vec<&str> = line.split_whitespace().collect();
                let using = words
                    .iter()
                    .position(|w| *w == "using")
                    .and_then(|i| words.get(i + 1))
                    .copied()
                    .unwrap_or("");
                let on = words
                    .iter()
                    .position(|w| *w == "on")
                    .and_then(|i| words.get(i + 1))
                    .copied()
                    .unwrap_or("");
                nodes.push(format!(
                    "{key} {using}{}{on}",
                    if using.is_empty() { "" } else { " " }
                ));
                break;
            }
        }
    }
    nodes.dedup();
    nodes.join(" > ")
}

#[tokio::test]
#[ignore = "needs a large seeded database in LAZY_SEARCH_PERF_DB"]
async fn plans_and_times_on_a_large_database() {
    let url = std::env::var("LAZY_SEARCH_PERF_DB").expect("LAZY_SEARCH_PERF_DB");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let workspace: WorkspaceId = WORKSPACE.parse().unwrap();
    let user_of = |name: &'static str| {
        let pool = pool.clone();
        async move {
            let id: uuid::Uuid = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap();
            UserId::from_uuid(id)
        }
    };
    let callers = [
        ("typical (15 conversations)", user_of("u0007").await),
        ("member of 150 private", user_of("perfme").await),
        ("new member (public only)", user_of("newbie").await),
    ];
    let today = time::OffsetDateTime::now_utc().date();
    let fmt = |days: i64| {
        let d = today - time::Duration::days(days);
        format!("{:04}-{:02}-{:02}", d.year(), u8::from(d.month()), d.day())
    };
    let (from_day, to_day) = (fmt(60), fmt(30));
    let queries: Vec<(&str, String)> = vec![
        ("rare word", "kubernetes".into()),
        ("common word (60%)", "the".into()),
        ("two words", "the deploy".into()),
        ("two rare words", "rollback kubernetes".into()),
        ("phrase", "\"rollback plan\"".into()),
        ("prefix of a common word", "dep".into()),
        ("deploy, 5% of messages", "deploy".into()),
        ("from: only", "from:u0007".into()),
        ("in: only (public)", "in:chan010".into()),
        ("in: only (private)", "in:chan100".into()),
        ("has:image only", "has:image".into()),
        ("has:file only", "has:file".into()),
        (
            "date range only",
            format!("after:{from_day} before:{to_day}"),
        ),
        (
            "text + several filters",
            format!("deploy from:u0007 has:file after:{}", fmt(400)),
        ),
        ("text + in:", "deploy in:chan010".into()),
        ("exclusion", "deploy -deployment".into()),
        ("file name word", "report".into()),
    ];
    // No time limit worth speaking of: this measures how long each query takes.
    let options = SearchOptions {
        statement_timeout: Duration::from_secs(120),
        ..SearchOptions::default()
    };
    let mut dump = String::new();
    println!(
        "\n{:<26} {:<28} {:>9} {:>9}  plan",
        "query", "caller", "first ms", "median ms"
    );
    for (label, q) in &queries {
        for (who, user) in &callers {
            let params = || SearchParams {
                q: q.clone(),
                ..SearchParams::default()
            };
            let mut times = Vec::new();
            let mut first_plan = String::new();
            for run in 0..4 {
                let plan =
                    lazy_server::search::explain(&pool, &options, *user, workspace, params())
                        .await
                        .unwrap_or_else(|e| panic!("{label}: {e}"));
                times.push(execution_ms(&plan));
                if run == 0 {
                    first_plan = plan;
                }
            }
            let first = times[0];
            let mut rest = times[1..].to_vec();
            rest.sort_by(|a, b| a.total_cmp(b));
            println!(
                "{label:<26} {who:<28} {first:>9.1} {:>9.1}  {}",
                rest[1],
                shape(&first_plan)
            );
            dump.push_str(&format!("=== {label} / {who} / {q}\n{first_plan}\n\n"));
        }
    }
    if let Ok(path) = std::env::var("LAZY_SEARCH_PERF_OUT") {
        std::fs::write(path, dump).unwrap();
    }

    // The whole request, as the server does it (hits built, snippets made), for a few shapes.
    println!("\nwhole search (release build only is meaningful):");
    for (label, q) in [
        ("rare word", "kubernetes"),
        ("common word", "the"),
        ("deploy", "deploy"),
        ("phrase", "\"rollback plan\""),
        ("from: only", "from:u0007"),
    ] {
        for (who, user) in &callers {
            let mut times = Vec::new();
            for _ in 0..4 {
                let started = Instant::now();
                let found = lazy_server::search::search(
                    &pool,
                    &options,
                    *user,
                    workspace,
                    SearchParams {
                        q: q.into(),
                        ..SearchParams::default()
                    },
                )
                .await
                .unwrap();
                times.push((started.elapsed(), found.hits.len(), found.truncated));
            }
            let mut sorted: Vec<Duration> = times[1..].iter().map(|t| t.0).collect();
            sorted.sort();
            println!(
                "{label:<14} {who:<28} first {:>7.1} ms  median {:>7.1} ms  hits {} truncated {}",
                times[0].0.as_secs_f64() * 1000.0,
                sorted[1].as_secs_f64() * 1000.0,
                times[0].1,
                times[0].2
            );
        }
    }
}
