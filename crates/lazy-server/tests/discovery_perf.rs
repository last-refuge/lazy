//! Measuring the directories and the saved list on a large workspace (`cargo test --release -p
//! lazy-server --test discovery_perf -- --ignored --nocapture`). Not run with the suite: it needs
//! `LAZY_DISCOVERY_PERF_DB`, a database seeded with `tests/fixtures/discovery_perf_seed.sql`
//! (20,000 people, 5,050 channels, 100,000 messages, 50,000 saved for one person).

use std::future::Future;
use std::time::Instant;

use lazy_domain::{UserId, WorkspaceId};
use lazy_protocol::{BrowseChannelsQuery, ChannelSort, PeopleQuery, SavedQuery};
use sqlx::postgres::PgPoolOptions;

async fn timed<T, F: Future<Output = T>>(
    label: &str,
    mut run: impl FnMut() -> F,
    shape: impl Fn(&T) -> String,
) {
    let mut times = Vec::new();
    let mut last = None;
    for _ in 0..5 {
        let started = Instant::now();
        let value = run().await;
        times.push(started.elapsed().as_secs_f64() * 1000.0);
        last = Some(value);
    }
    let mut rest = times[1..].to_vec();
    rest.sort_by(|a, b| a.total_cmp(b));
    println!(
        "{label:<46} first {:>7.1} ms  median {:>7.1} ms  {}",
        times[0],
        rest[1],
        shape(&last.unwrap())
    );
}

#[tokio::test]
#[ignore = "needs a large seeded database in LAZY_DISCOVERY_PERF_DB"]
async fn directories_and_saved_on_a_large_workspace() {
    let url = std::env::var("LAZY_DISCOVERY_PERF_DB").expect("LAZY_DISCOVERY_PERF_DB");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let workspace: WorkspaceId = "01a00000-0000-7000-8000-0000000000a0".parse().unwrap();
    let me: UserId = "01a00000-0000-7000-8000-000000000001".parse().unwrap();
    println!();
    let browse = |query: BrowseChannelsQuery| {
        let pool = pool.clone();
        move || {
            let (pool, query) = (pool.clone(), query.clone());
            async move {
                lazy_server::conversations::browse_channels(&pool, me, workspace, query)
                    .await
                    .unwrap()
            }
        }
    };
    let shape_b = |r: &lazy_protocol::BrowseChannelsResponse| {
        format!("rows {} more {}", r.channels.len(), r.has_more)
    };
    timed(
        "browse: first page by name",
        browse(BrowseChannelsQuery::default()),
        shape_b,
    )
    .await;
    timed(
        "browse: first page by members",
        browse(BrowseChannelsQuery {
            sort: Some(ChannelSort::Members),
            ..Default::default()
        }),
        shape_b,
    )
    .await;
    timed(
        "browse: deep page (offset 4000) by name",
        browse(BrowseChannelsQuery {
            offset: Some(4000),
            ..Default::default()
        }),
        shape_b,
    )
    .await;
    timed(
        "browse: text query (rare: 'thing 5')",
        browse(BrowseChannelsQuery {
            query: Some("thing 5".into()),
            ..Default::default()
        }),
        shape_b,
    )
    .await;
    timed(
        "browse: text query (common: 'topic')",
        browse(BrowseChannelsQuery {
            query: Some("topic".into()),
            ..Default::default()
        }),
        shape_b,
    )
    .await;
    timed(
        "browse: joined only",
        browse(BrowseChannelsQuery {
            joined: Some(true),
            ..Default::default()
        }),
        shape_b,
    )
    .await;
    timed(
        "browse: not joined, include archived, members",
        browse(BrowseChannelsQuery {
            joined: Some(false),
            include_archived: Some(true),
            sort: Some(ChannelSort::Members),
            ..Default::default()
        }),
        shape_b,
    )
    .await;

    let people = |query: PeopleQuery| {
        let pool = pool.clone();
        move || {
            let (pool, query) = (pool.clone(), query.clone());
            async move {
                lazy_server::discovery::people(&pool, me, workspace, query)
                    .await
                    .unwrap()
            }
        }
    };
    let shape_p =
        |r: &lazy_protocol::PeoplePage| format!("rows {} more {}", r.people.len(), r.has_more);
    timed(
        "people: first page",
        people(PeopleQuery::default()),
        shape_p,
    )
    .await;
    timed(
        "people: deep page (offset 15000)",
        people(PeopleQuery {
            offset: Some(15000),
            ..Default::default()
        }),
        shape_p,
    )
    .await;
    timed(
        "people: query (rare: 'u1999')",
        people(PeopleQuery {
            query: Some("u1999".into()),
            ..Default::default()
        }),
        shape_p,
    )
    .await;
    timed(
        "people: query (common: 'person 1')",
        people(PeopleQuery {
            query: Some("person 1".into()),
            ..Default::default()
        }),
        shape_p,
    )
    .await;
    timed(
        "people: query matching nobody",
        people(PeopleQuery {
            query: Some("zzzz".into()),
            ..Default::default()
        }),
        shape_p,
    )
    .await;

    let saved = |query: SavedQuery| {
        let pool = pool.clone();
        move || {
            let (pool, query) = (pool.clone(), query.clone());
            async move {
                lazy_server::discovery::saved(&pool, me, workspace, query)
                    .await
                    .unwrap()
            }
        }
    };
    let shape_s =
        |r: &lazy_protocol::SavedPage| format!("rows {} more {}", r.items.len(), r.has_more);
    timed(
        "saved: first page (50)",
        saved(SavedQuery::default()),
        shape_s,
    )
    .await;
    timed(
        "saved: first page (100)",
        saved(SavedQuery {
            limit: Some(100),
            ..Default::default()
        }),
        shape_s,
    )
    .await;
    // Orders run from 1 to 50,000 in saved order; the middle and the near-oldest ends.
    for (label, before) in [
        ("saved: deep page (cursor 25,000)", 25_000u64),
        ("saved: oldest page (cursor 60)", 60),
    ] {
        timed(
            label,
            saved(SavedQuery {
                before: Some(before),
                ..Default::default()
            }),
            shape_s,
        )
        .await;
    }
    let started = Instant::now();
    let boot = lazy_server::bootstrap::bootstrap(&pool, me).await.unwrap();
    println!(
        "bootstrap (whole, includes {} saved ids and {} members): {:.1} ms",
        boot.workspaces[0].saved.len(),
        boot.workspaces[0].members.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
}
