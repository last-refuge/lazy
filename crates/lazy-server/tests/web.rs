//! The web client's side of the server (D47): sign-in by cookie, what a cookie may authenticate,
//! and the files under `/web/`.

mod common;

use common::*;
use futures_util::StreamExt;
use lazy_domain::{OperationId, Username};
use lazy_protocol::api::paths;
use lazy_protocol::{
    BootstrapResponse, ClientFrame, ErrorCode, LoginRequest, Password, ServerFrame,
    WebSessionResponse,
};
use lazy_server::IntegrationOptions;
use sqlx::PgPool;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// A directory with a stand-in web client, removed when dropped.
struct WebDir(std::path::PathBuf);

impl WebDir {
    fn new() -> WebDir {
        let dir = std::env::temp_dir().join(format!("lazy-web-test-{}", OperationId::generate()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html><title>Lazy</title>").unwrap();
        std::fs::write(dir.join("app.js"), "export {};").unwrap();
        WebDir(dir)
    }
}

impl Drop for WebDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start(pool: PgPool, web: &WebDir) -> App {
    App::start_with_integrations(
        pool,
        IntegrationOptions {
            web_dir: Some(web.0.clone()),
            ..IntegrationOptions::default()
        },
    )
    .await
}

fn login_body(username: &str) -> LoginRequest {
    LoginRequest {
        username: Username::parse(username).unwrap(),
        password: Password::new(PASSWORD),
    }
}

/// Signs in through the web route and returns the cookie pair (`lazy_session=...`).
async fn web_login(app: &App, username: &str) -> (String, String) {
    let response = app
        .http
        .post(app.url(paths::WEB_LOGIN))
        .header(paths::WEB_REQUEST_HEADER, "1")
        .json(&login_body(username))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .expect("a cookie")
        .to_str()
        .unwrap()
        .to_owned();
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(
        body.get("token").is_none(),
        "the token never goes in the body"
    );
    let session: WebSessionResponse = serde_json::from_value(body).unwrap();
    assert_eq!(session.user.username.as_str(), username);
    let pair = set_cookie.split(';').next().unwrap().trim().to_owned();
    (pair, set_cookie)
}

#[sqlx::test]
async fn signing_in_on_the_web_sets_a_cookie_that_authenticates(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    let ada = app.register("ada").await;
    let (cookie, set_cookie) = web_login(&app, "ada").await;
    assert!(cookie.starts_with("lazy_session=lzy_"));
    for attribute in ["Path=/", "HttpOnly", "SameSite=Strict", "Max-Age=2592000"] {
        assert!(set_cookie.contains(attribute), "{set_cookie}");
    }
    assert!(!set_cookie.contains("Secure"), "plain HTTP: {set_cookie}");

    // Reads go with the cookie alone.
    let response = app
        .http
        .get(app.url(paths::BOOTSTRAP))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let snapshot: BootstrapResponse = response.json().await.unwrap();
    assert_eq!(snapshot.user.id, ada.user.id);

    // A change needs the web client's header too.
    let send = |with_header: bool| {
        let mut request = app
            .http
            .post(app.url(&paths::messages(app.general)))
            .header("cookie", &cookie)
            .json(&serde_json::json!({
                "operation_id": OperationId::generate(),
                "document": App::mention_document("from the web", &[]),
            }));
        if with_header {
            request = request.header(paths::WEB_REQUEST_HEADER, "1");
        }
        request.send()
    };
    let (status, error) = api_error(send(false).await.unwrap()).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    assert_eq!(send(true).await.unwrap().status(), 200);

    // ...and must not say it comes from another site.
    let response = app
        .http
        .post(app.url(&paths::messages(app.general)))
        .header("cookie", &cookie)
        .header(paths::WEB_REQUEST_HEADER, "1")
        .header("origin", "https://evil.example")
        .json(&serde_json::json!({
            "operation_id": OperationId::generate(),
            "document": App::mention_document("forged", &[]),
        }))
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Forbidden);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn web_sign_in_is_refused_without_the_header_or_from_another_site(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    app.register("ada").await;
    let response = app
        .http
        .post(app.url(paths::WEB_LOGIN))
        .json(&login_body("ada"))
        .send()
        .await
        .unwrap();
    assert!(response.headers().get("set-cookie").is_none());
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Forbidden);

    let response = app
        .http
        .post(app.url(paths::WEB_LOGIN))
        .header(paths::WEB_REQUEST_HEADER, "1")
        .header("origin", "https://evil.example")
        .json(&login_body("ada"))
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Forbidden);

    // A wrong password is the ordinary answer, with no cookie.
    let response = app
        .http
        .post(app.url(paths::WEB_LOGIN))
        .header(paths::WEB_REQUEST_HEADER, "1")
        .json(&LoginRequest {
            username: Username::parse("ada").unwrap(),
            password: Password::new("not the password"),
        })
        .send()
        .await
        .unwrap();
    assert!(response.headers().get("set-cookie").is_none());
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::InvalidCredentials);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_cookie_is_secure_behind_https(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    app.register("ada").await;
    let response = app
        .http
        .post(app.url(paths::WEB_LOGIN))
        .header(paths::WEB_REQUEST_HEADER, "1")
        .header("x-forwarded-proto", "https")
        .json(&login_body("ada"))
        .send()
        .await
        .unwrap();
    let set_cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(set_cookie.ends_with("; Secure"), "{set_cookie}");
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn signing_out_revokes_the_session_and_clears_the_cookie(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    app.register("ada").await;
    let (cookie, _) = web_login(&app, "ada").await;
    let response = app
        .http
        .post(app.url(paths::WEB_LOGOUT))
        .header("cookie", &cookie)
        .header(paths::WEB_REQUEST_HEADER, "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let cleared = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cleared.starts_with("lazy_session=;"), "{cleared}");
    assert!(cleared.contains("Max-Age=0"), "{cleared}");
    let response = app
        .http
        .get(app.url(paths::BOOTSTRAP))
        .header("cookie", &cookie)
        .send()
        .await
        .unwrap();
    let (status, error) = api_error(response).await;
    assert_error(status, &error, ErrorCode::Unauthenticated);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_realtime_socket_takes_the_cookie_only_from_this_site(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    app.register("ada").await;
    let (cookie, _) = web_login(&app, "ada").await;
    let request = |origin: Option<&str>| {
        let mut request = format!("ws://{}{}", app.server.addr, paths::REALTIME)
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("cookie", cookie.parse().unwrap());
        if let Some(origin) = origin {
            request
                .headers_mut()
                .insert("origin", origin.parse().unwrap());
        }
        request
    };
    // No origin, or another site's: refused before the upgrade.
    for origin in [None, Some("https://evil.example")] {
        match tokio_tungstenite::connect_async(request(origin)).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), 403);
            }
            other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
        }
    }
    // This site: subscribed and caught up like any client.
    let here = format!("http://{}", app.server.addr);
    let (mut socket, _) = tokio_tungstenite::connect_async(request(Some(&here)))
        .await
        .expect("upgrade");
    let subscribe = serde_json::to_string(&ClientFrame::Subscribe {
        workspace_id: app.workspace_id,
        after: app.head().await,
    })
    .unwrap();
    futures_util::SinkExt::send(
        &mut socket,
        tokio_tungstenite::tungstenite::Message::text(subscribe),
    )
    .await
    .unwrap();
    let caught_up = tokio::time::timeout(DEADLINE, async {
        while let Some(Ok(message)) = socket.next().await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message
                && let Ok(ServerFrame::CaughtUp { .. }) = serde_json::from_str(text.as_str())
            {
                return true;
            }
        }
        false
    })
    .await
    .expect("caught up in time");
    assert!(caught_up);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn the_web_client_is_served_with_a_strict_policy(pool: PgPool) {
    let web = WebDir::new();
    let app = start(pool, &web).await;
    let no_redirects = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    for path in ["/", "/web"] {
        let response = no_redirects.get(app.url(path)).send().await.unwrap();
        assert_eq!(response.status(), 303, "{path}");
        assert_eq!(response.headers()["location"], "/web/", "{path}");
    }
    let response = app.http.get(app.url("/web/")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_owned();
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(
        csp.contains("script-src 'self' 'wasm-unsafe-eval'"),
        "{csp}"
    );
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    assert_eq!(response.headers()["cache-control"], "no-cache");
    assert!(
        response
            .text()
            .await
            .unwrap()
            .contains("<title>Lazy</title>")
    );
    let script = app.http.get(app.url("/web/app.js")).send().await.unwrap();
    assert_eq!(script.status(), 200);
    assert!(
        script.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    // Outside the directory: nothing.
    let escape = app
        .http
        .get(app.url("/web/../Cargo.toml"))
        .send()
        .await
        .unwrap();
    assert_eq!(escape.status(), 404);
    // The API is untouched.
    let response = app.http.get(app.url(paths::HEALTH)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    app.server.shutdown().await.unwrap();
}

#[sqlx::test]
async fn without_a_web_client_nothing_is_served(pool: PgPool) {
    let app = App::start(pool).await;
    let response = app.http.get(app.url("/web/")).send().await.unwrap();
    assert_eq!(response.status(), 404);
    let response = app.http.get(app.url("/")).send().await.unwrap();
    assert_eq!(response.status(), 404);
    app.server.shutdown().await.unwrap();
}
