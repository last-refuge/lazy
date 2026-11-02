//! Link previews (D41), end to end against a stand-in site on this machine: what a page says about
//! itself, the cache, who may read previews, and the fetch rules (redirects, private addresses,
//! size, time, and kind of content).

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use common::*;
use lazy_domain::{ConversationId, MessageDocument, MessageId};
use lazy_protocol::api::paths;
use lazy_protocol::{ErrorCode, LinkPreviewsResponse};
use lazy_server::IntegrationOptions;
use lazy_server::link_previews::PreviewOptions;
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Site {
    hits: Arc<AtomicUsize>,
}

const PAGE: &str = r#"<!doctype html><html><head>
<title>Roadmap board</title>
<meta property="og:title" content="Q4 roadmap &amp; risks">
<meta property="og:description" content="The board the team plans on.">
<meta property="og:site_name" content="Miro">
</head><body>hello</body></html>"#;

async fn page(State(site): State<Site>) -> impl IntoResponse {
    site.hits.fetch_add(1, Ordering::SeqCst);
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], PAGE)
}

async fn plain_title() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html")],
        "<html><head><title>Only a title</title></head></html>",
    )
}

async fn json() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"title": "not a page"}"#,
    )
}

async fn huge() -> impl IntoResponse {
    // The title comes after more than the fetcher reads.
    let body = format!(
        "<html>{}<title>Too far</title></html>",
        " ".repeat(400 * 1024)
    );
    ([(header::CONTENT_TYPE, "text/html")], body)
}

async fn slow() -> impl IntoResponse {
    tokio::time::sleep(Duration::from_secs(20)).await;
    ([(header::CONTENT_TYPE, "text/html")], PAGE)
}

async fn site(site: Site) -> String {
    let router = Router::new()
        .route("/page", get(page))
        .route("/title", get(plain_title))
        .route("/json", get(json))
        .route("/huge", get(huge))
        .route("/slow", get(slow))
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .route("/hop", get(|| async { Redirect::temporary("/page") }))
        .route(
            "/private",
            get(|| async { Redirect::temporary("http://10.1.2.3/admin") }),
        )
        .route(
            "/metadata",
            get(|| async { Redirect::temporary("http://169.254.169.254/latest/meta-data/") }),
        )
        .route("/loop", get(|| async { Redirect::temporary("/loop") }))
        .with_state(site);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

async fn start(pool: PgPool, allow_loopback: bool) -> App {
    App::start_with_integrations(
        pool,
        IntegrationOptions {
            previews: PreviewOptions { allow_loopback },
            ..IntegrationOptions::default()
        },
    )
    .await
}

async fn post(app: &App, who: &Account, conversation: ConversationId, markdown: &str) -> MessageId {
    let response = app
        .send_doc_raw(
            who,
            conversation,
            MessageDocument::from_markdown(markdown).unwrap(),
        )
        .await;
    assert!(response.status().is_success(), "{}", response.status());
    response
        .json::<lazy_protocol::SendMessageResponse>()
        .await
        .unwrap()
        .message
        .id
}

async fn previews(app: &App, who: &Account, message: MessageId) -> reqwest::Response {
    app.call(
        reqwest::Method::GET,
        &paths::message_previews(message),
        who,
        None,
    )
    .await
}

async fn previews_ok(app: &App, who: &Account, message: MessageId) -> LinkPreviewsResponse {
    let response = previews(app, who, message).await;
    assert!(response.status().is_success(), "{}", response.status());
    response.json().await.unwrap()
}

#[sqlx::test]
async fn a_page_says_what_it_is_once_per_workspace_and_only_to_readers(pool: PgPool) {
    let app = start(pool, true).await;
    let alice = app.register("alice").await;
    let bob = app.register("bob").await;
    let stand = Site::default();
    let base = site(stand.clone()).await;

    let message = post(
        &app,
        &alice,
        app.general,
        &format!(
            "Plan: [board]({base}/page), [title]({base}/title), [data]({base}/json), \
             and [again]({base}/page)\n\n> [quoted]({base}/title)"
        ),
    )
    .await;
    let got = previews_ok(&app, &bob, message).await.previews;
    // One per distinct link that has something to say, in order; JSON is not a page, and a quote
    // is not previewed.
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0].url, format!("{base}/page"));
    assert_eq!(got[0].title.as_deref(), Some("Q4 roadmap & risks"));
    assert_eq!(
        got[0].description.as_deref(),
        Some("The board the team plans on.")
    );
    assert_eq!(got[0].site_name.as_deref(), Some("Miro"));
    assert_eq!(got[1].title.as_deref(), Some("Only a title"));
    assert_eq!(got[1].site_name.as_deref(), Some("127.0.0.1"));
    assert_eq!(stand.hits.load(Ordering::SeqCst), 1);

    // Asked again, or for another message with the same link: from the cache.
    assert_eq!(previews_ok(&app, &alice, message).await.previews, got);
    let other = post(&app, &bob, app.general, &format!("[same]({base}/page)")).await;
    assert_eq!(previews_ok(&app, &alice, other).await.previews.len(), 1);
    assert_eq!(stand.hits.load(Ordering::SeqCst), 1);

    // Someone who cannot read the message cannot read its previews.
    let private = app.create_channel(&alice, "secret", "private").await.id;
    let hidden = post(&app, &alice, private, &format!("[board]({base}/page)")).await;
    assert_eq!(previews_ok(&app, &alice, hidden).await.previews.len(), 1);
    let (status, error) = api_error(previews(&app, &bob, hidden).await).await;
    assert_error(status, &error, ErrorCode::NotFound);

    // A message without links, and an app's message, have none, and nothing is fetched.
    let words = post(&app, &alice, app.general, "no links here").await;
    assert!(previews_ok(&app, &alice, words).await.previews.is_empty());
    let hook = app.create_webhook(&alice, app.general, "CI").await;
    let from_bot = app.deliver(&hook, &format!("[board]({base}/page)")).await;
    assert!(
        previews_ok(&app, &alice, from_bot)
            .await
            .previews
            .is_empty()
    );
    assert_eq!(stand.hits.load(Ordering::SeqCst), 1);
}

#[sqlx::test]
async fn fetching_keeps_to_its_rules(pool: PgPool) {
    let app = start(pool, true).await;
    let alice = app.register("alice").await;
    let stand = Site::default();
    let base = site(stand.clone()).await;
    let one = |path: &str| format!("[x]({base}{path})");

    // A redirect within the rules is followed.
    let hop = post(&app, &alice, app.general, &one("/hop")).await;
    let got = previews_ok(&app, &alice, hop).await.previews;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].url, format!("{base}/hop"), "the link as written");
    assert_eq!(got[0].title.as_deref(), Some("Q4 roadmap & risks"));

    // Redirects to private or metadata addresses, loops, failures, pages too large to find a
    // title in, and pages too slow to answer give no preview.
    for path in ["/private", "/metadata", "/loop", "/missing", "/huge"] {
        let message = post(&app, &alice, app.general, &one(path)).await;
        assert!(
            previews_ok(&app, &alice, message).await.previews.is_empty(),
            "{path}"
        );
    }
    let slow = post(&app, &alice, app.general, &one("/slow")).await;
    let started = Instant::now();
    assert!(previews_ok(&app, &alice, slow).await.previews.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "gave up after {:?}",
        started.elapsed()
    );
    // A failure is remembered, not fetched again on every view.
    let started = Instant::now();
    assert!(previews_ok(&app, &alice, slow).await.previews.is_empty());
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[sqlx::test]
async fn this_machine_is_off_limits_by_default(pool: PgPool) {
    let app = start(pool, false).await;
    let alice = app.register("alice").await;
    let stand = Site::default();
    let base = site(stand.clone()).await;
    for link in [
        format!("{base}/page"),
        "http://localhost/page".to_owned(),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
        "http://10.0.0.1/".to_owned(),
    ] {
        let message = post(&app, &alice, app.general, &format!("[x]({link})")).await;
        assert!(
            previews_ok(&app, &alice, message).await.previews.is_empty(),
            "{link}"
        );
    }
    assert_eq!(
        stand.hits.load(Ordering::SeqCst),
        0,
        "nothing reached the site"
    );
}
