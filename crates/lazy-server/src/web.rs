//! The web client (D47): its sign-in by cookie, the rules a cookie must pass to authenticate a
//! request, and its files.
//!
//! A browser cannot put a token in a WebSocket's headers or an `<img>`'s request, so the web client
//! keeps its session in a cookie instead of the `Authorization` header: `HttpOnly` (no script
//! reads it), `SameSite=Strict` (no other site's page sends it), `Secure` when the page was served
//! over HTTPS, and `Path=/`. The token in it is an ordinary session token, revoked and expired like
//! any other.
//!
//! A cookie goes with every request the browser makes to this server, which is what cross-site
//! request forgery abuses, so a request it authenticates must also show that this server's own page
//! made it ([`check_cookie_request`]): anything that changes state carries
//! [`paths::WEB_REQUEST_HEADER`], which a page elsewhere cannot add without a CORS preflight this
//! server never approves, and a request that says where it comes from (`Origin`) must come from
//! here. A WebSocket upgrade is a `GET` that can change state, so it must carry an `Origin`, and it
//! must be this server's.
//!
//! The files are served from [`crate::app::Integrations::web_dir`] under `/web/`, with a content
//! security policy that allows only this server's own scripts, styles, images, and connections.

use std::path::PathBuf;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::header::{CACHE_CONTROL, COOKIE, HOST, ORIGIN, SET_COOKIE, UPGRADE};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use lazy_protocol::api::paths;
use lazy_protocol::{
    LoginRequest, RegisterRequest, SessionResponse, SessionToken, WebSessionResponse,
};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::accounts;
use crate::app::AppState;
use crate::error::AppError;

/// How long the cookie lives: as long as the session (`accounts::insert_session`).
const COOKIE_MAX_AGE_SECS: u64 = 30 * 24 * 60 * 60;

/// What the web client's pages may load and connect to: this server only. WebAssembly needs
/// `wasm-unsafe-eval` to be compiled; nothing else is evaluated.
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; font-src 'self'; \
     manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// The session token in the request's cookie, if it has one.
pub(crate) fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(name, _)| *name == paths::WEB_SESSION_COOKIE)
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
}

/// Whether a request authenticated by the cookie (not by `Authorization`) may go ahead. See the
/// module documentation.
pub(crate) fn check_cookie_request(parts: &Parts) -> Result<(), AppError> {
    let headers = &parts.headers;
    let origin = headers.get(ORIGIN).and_then(|value| value.to_str().ok());
    if let Some(origin) = origin
        && !same_origin(origin, headers)
    {
        return Err(AppError::forbidden("this request comes from another site"));
    }
    let upgrade = headers.contains_key(UPGRADE);
    if upgrade && origin.is_none() {
        return Err(AppError::forbidden(
            "this request does not say where it comes from",
        ));
    }
    let reads = matches!(parts.method, Method::GET | Method::HEAD);
    if !reads && !from_web_client(headers) {
        return Err(AppError::forbidden(
            "this request does not come from the web client",
        ));
    }
    Ok(())
}

/// The request carries [`paths::WEB_REQUEST_HEADER`].
fn from_web_client(headers: &HeaderMap) -> bool {
    headers
        .get(paths::WEB_REQUEST_HEADER)
        .is_some_and(|value| value.as_bytes() == b"1")
}

/// Whether `origin` (`https://chat.example.com`) names the host the request was sent to, as the
/// `Host` header or a proxy's `X-Forwarded-Host` says. The scheme is not compared: behind a proxy
/// that ends TLS the server cannot see it, and a host is enough to tell a page here from one
/// elsewhere.
fn same_origin(origin: &str, headers: &HeaderMap) -> bool {
    let Some((_, authority)) = origin.split_once("://") else {
        return false;
    };
    let authority = authority.trim_end_matches('/');
    if authority.is_empty() {
        return false;
    }
    let forwarded = HeaderName::from_static("x-forwarded-host");
    [headers.get(HOST), headers.get(&forwarded)]
        .into_iter()
        .flatten()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|host| host.trim().eq_ignore_ascii_case(authority))
}

/// Whether the browser reached the server over HTTPS, as a proxy that ends TLS says.
fn over_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

fn cookie(headers: &HeaderMap, value: &str, max_age: u64) -> HeaderValue {
    let secure = if over_https(headers) { "; Secure" } else { "" };
    let text = format!(
        "{}={value}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}",
        paths::WEB_SESSION_COOKIE
    );
    // The token is `lzy_` and hex, so the header is always valid.
    HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// The answer to a web sign-in: the user in the body, the token in the cookie.
fn signed_in(headers: &HeaderMap, session: SessionResponse) -> Response {
    let SessionResponse { token, user } = session;
    let mut response = Json(WebSessionResponse { user }).into_response();
    let headers_out = response.headers_mut();
    headers_out.insert(
        SET_COOKIE,
        cookie(headers, token_text(&token), COOKIE_MAX_AGE_SECS),
    );
    headers_out.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn token_text(token: &SessionToken) -> &str {
    token.expose()
}

/// Sign-in and registration change who the browser is, so a page elsewhere must not be able to
/// do either (signing someone into an account of the attacker's): they need the web client's
/// header too.
fn require_web_client(headers: &HeaderMap) -> Result<(), AppError> {
    if !from_web_client(headers) {
        return Err(AppError::forbidden(
            "this request does not come from the web client",
        ));
    }
    if let Some(origin) = headers.get(ORIGIN).and_then(|value| value.to_str().ok())
        && !same_origin(origin, headers)
    {
        return Err(AppError::forbidden("this request comes from another site"));
    }
    Ok(())
}

pub(crate) async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::http::ApiJson(request): crate::http::ApiJson<LoginRequest>,
) -> Result<Response, AppError> {
    require_web_client(&headers)?;
    let session = accounts::login(&state, request).await?;
    Ok(signed_in(&headers, session))
}

pub(crate) async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::http::ApiJson(request): crate::http::ApiJson<RegisterRequest>,
) -> Result<Response, AppError> {
    require_web_client(&headers)?;
    let session = accounts::register(&state, request).await?;
    Ok(signed_in(&headers, session))
}

/// Revokes the cookie's session, if it is still good, and clears the cookie either way.
pub(crate) async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    require_web_client(&headers)?;
    if let Some(token) = session_cookie(&headers)
        && let Ok(principal) = accounts::authenticate(&state.pool, token).await
    {
        accounts::logout(&state.pool, principal).await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .insert(SET_COOKIE, cookie(&headers, "", 0));
    Ok(response)
}

/// The web client's files under `/web/`, `/web` and `/` sent there; nothing without a directory.
pub(crate) fn files(web_dir: Option<PathBuf>) -> Router<AppState> {
    let Some(dir) = web_dir else {
        return Router::new();
    };
    let app = format!("{}/", paths::WEB_APP);
    let to_app = app.clone();
    let to_app_root = app.clone();
    let headers = |router: Router<AppState>| {
        router
            .layer(SetResponseHeaderLayer::overriding(
                HeaderName::from_static("content-security-policy"),
                HeaderValue::from_static(CONTENT_SECURITY_POLICY),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                HeaderName::from_static("x-content-type-options"),
                HeaderValue::from_static("nosniff"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                HeaderName::from_static("referrer-policy"),
                HeaderValue::from_static("no-referrer"),
            ))
            .layer(SetResponseHeaderLayer::overriding(
                HeaderName::from_static("x-frame-options"),
                HeaderValue::from_static("DENY"),
            ))
            // Revalidated each time (the files keep their names from one build to the next).
            .layer(SetResponseHeaderLayer::overriding(
                CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
    };
    let served = headers(Router::new().nest_service(&app, ServeDir::new(dir).precompressed_gzip()));
    Router::new()
        .route(
            paths::WEB_APP,
            get(move || async move { Redirect::to(&to_app) }),
        )
        .route("/", get(move || async move { Redirect::to(&to_app_root) }))
        .merge(served)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        map
    }

    fn parts(method: Method, pairs: &[(&'static str, &'static str)]) -> Parts {
        let mut request = axum::http::Request::builder()
            .method(method)
            .uri("/v1/bootstrap")
            .body(())
            .unwrap();
        *request.headers_mut() = headers(pairs);
        request.into_parts().0
    }

    #[test]
    fn the_session_cookie_is_found_among_others() {
        let map = headers(&[("cookie", "a=1; lazy_session=lzs_abc ; b=2")]);
        assert_eq!(session_cookie(&map), Some("lzs_abc"));
        assert_eq!(session_cookie(&headers(&[("cookie", "a=1")])), None);
        assert_eq!(
            session_cookie(&headers(&[("cookie", "lazy_session=")])),
            None
        );
        let two = headers(&[("cookie", "a=1"), ("cookie", "lazy_session=lzs_x")]);
        assert_eq!(session_cookie(&two), Some("lzs_x"));
    }

    #[test]
    fn origins_must_name_the_host() {
        let here = headers(&[("host", "chat.example.com")]);
        assert!(same_origin("https://chat.example.com", &here));
        assert!(same_origin("http://CHAT.example.com", &here));
        assert!(!same_origin("https://evil.example", &here));
        assert!(!same_origin("https://chat.example.com.evil.example", &here));
        assert!(!same_origin("null", &here));
        let proxied = headers(&[
            ("host", "lazy:8080"),
            ("x-forwarded-host", "chat.example.com"),
        ]);
        assert!(same_origin("https://chat.example.com", &proxied));
        let port = headers(&[("host", "127.0.0.1:8080")]);
        assert!(same_origin("http://127.0.0.1:8080", &port));
        assert!(!same_origin("http://127.0.0.1:9090", &port));
    }

    #[test]
    fn a_cookie_reads_freely_but_changes_only_from_the_web_client() {
        let host = ("host", "chat.example.com");
        assert!(check_cookie_request(&parts(Method::GET, &[host])).is_ok());
        assert!(check_cookie_request(&parts(Method::POST, &[host])).is_err());
        assert!(check_cookie_request(&parts(Method::POST, &[host, ("x-lazy-web", "1")])).is_ok());
        assert!(
            check_cookie_request(&parts(
                Method::POST,
                &[
                    host,
                    ("x-lazy-web", "1"),
                    ("origin", "https://evil.example")
                ]
            ))
            .is_err()
        );
        assert!(
            check_cookie_request(&parts(
                Method::GET,
                &[host, ("origin", "https://evil.example")]
            ))
            .is_err()
        );
    }

    #[test]
    fn a_websocket_upgrade_by_cookie_must_come_from_here() {
        let host = ("host", "chat.example.com");
        let upgrade = ("upgrade", "websocket");
        assert!(check_cookie_request(&parts(Method::GET, &[host, upgrade])).is_err());
        assert!(
            check_cookie_request(&parts(
                Method::GET,
                &[host, upgrade, ("origin", "https://evil.example")]
            ))
            .is_err()
        );
        assert!(
            check_cookie_request(&parts(
                Method::GET,
                &[host, upgrade, ("origin", "https://chat.example.com")]
            ))
            .is_ok()
        );
    }

    #[test]
    fn the_cookie_is_secure_only_behind_https() {
        let plain = cookie(&headers(&[]), "lzs_x", 60);
        assert_eq!(
            plain.to_str().unwrap(),
            "lazy_session=lzs_x; Path=/; Max-Age=60; HttpOnly; SameSite=Strict"
        );
        let tls = cookie(&headers(&[("x-forwarded-proto", "https")]), "lzs_x", 60);
        assert!(tls.to_str().unwrap().ends_with("; Secure"));
    }
}
