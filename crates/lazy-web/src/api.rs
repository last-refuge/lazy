//! HTTP requests through the browser's `fetch`, classified the way the desktop runtime classifies
//! them (`lazy-client-runtime/src/api.rs`): a definitive refusal from the server is
//! [`RequestError::Rejected`]; anything else (no answer, a timeout, a proxy's error page, a body
//! that is not ours) is [`RequestError::Unknown`], whose outcome the core reconciles.
//!
//! Every request is to this page's own server (relative paths), sends the session cookie (the
//! browser does that for same-origin requests), and carries [`paths::WEB_REQUEST_HEADER`], without
//! which the server refuses a cookie-authenticated change (D47).

use std::time::Duration;

use js_sys::Uint8Array;
use lazy_client_core::RequestError;
use lazy_protocol::api::paths;
use lazy_protocol::{ApiError, ErrorCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, Headers, RequestCredentials, RequestInit, Response};

/// How long an ordinary request may take.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(30);
/// How long an upload may take at least; more for a large file.
pub(crate) const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// How many times a read that is safe to repeat is tried.
const READ_ATTEMPTS: u32 = 4;

/// A request body.
pub(crate) enum Body {
    None,
    Json(String),
    Bytes {
        bytes: Vec<u8>,
        content_type: String,
        /// Extra headers (an attachment's name).
        headers: Vec<(&'static str, String)>,
    },
}

/// What came back: the status and the body.
pub(crate) struct Answer {
    pub status: u16,
    pub bytes: Vec<u8>,
}

fn window() -> web_sys::Window {
    web_sys::window().expect("a browser window")
}

/// Waits `duration` without blocking the page.
pub(crate) async fn sleep(duration: Duration) {
    let millis = i32::try_from(duration.as_millis()).unwrap_or(i32::MAX);
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, millis);
    });
    let _ = JsFuture::from(promise).await;
}

/// Sends one request. `Err` is a reason in a few words when nothing usable came back.
pub(crate) async fn send(
    method: &str,
    path: &str,
    body: Body,
    timeout: Duration,
) -> Result<Answer, String> {
    let headers = Headers::new().map_err(|_| "headers".to_owned())?;
    let _ = headers.set(paths::WEB_REQUEST_HEADER, "1");
    let init = RequestInit::new();
    init.set_method(method);
    init.set_credentials(RequestCredentials::SameOrigin);
    match body {
        Body::None => {}
        Body::Json(text) => {
            let _ = headers.set("content-type", "application/json");
            init.set_body(&JsValue::from_str(&text));
        }
        Body::Bytes {
            bytes,
            content_type,
            headers: extra,
        } => {
            let _ = headers.set("content-type", &content_type);
            for (name, value) in extra {
                let _ = headers.set(name, &value);
            }
            let array = Uint8Array::from(bytes.as_slice());
            init.set_body(&array);
        }
    }
    init.set_headers(&headers);
    let controller = AbortController::new().map_err(|_| "abort controller".to_owned())?;
    init.set_signal(Some(&controller.signal()));
    let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let abort = Closure::once_into_js(move || controller.abort());
    let timer = window()
        .set_timeout_with_callback_and_timeout_and_arguments_0(abort.unchecked_ref(), millis)
        .ok();
    let result = async {
        let response = JsFuture::from(window().fetch_with_str_and_init(path, &init))
            .await
            .map_err(|_| "no answer from the server".to_owned())?;
        let response: Response = response
            .dyn_into()
            .map_err(|_| "not a response".to_owned())?;
        let status = response.status();
        let buffer = response
            .array_buffer()
            .map_err(|_| "the answer could not be read".to_owned())?;
        let buffer = JsFuture::from(buffer)
            .await
            .map_err(|_| "the answer was cut off".to_owned())?;
        Ok(Answer {
            status,
            bytes: Uint8Array::new(&buffer).to_vec(),
        })
    }
    .await;
    if let Some(timer) = timer {
        window().clear_timeout_with_handle(timer);
    }
    result
}

/// What a failed status means (see the module documentation).
pub(crate) fn classify_failure(status: u16, bytes: &[u8]) -> RequestError {
    match serde_json::from_slice::<ApiError>(bytes) {
        Ok(error) if (400..500).contains(&status) && error.code.is_definitive_rejection() => {
            RequestError::Rejected(error)
        }
        _ => RequestError::Unknown(format!("the server answered {status}")),
    }
}

/// [`classify_failure`] for search, whose `unavailable` (it took too long) is an answer.
pub(crate) fn classify_search_failure(status: u16, bytes: &[u8]) -> RequestError {
    if status == 503
        && let Ok(error) = serde_json::from_slice::<ApiError>(bytes)
        && error.code == ErrorCode::Unavailable
    {
        return RequestError::Rejected(error);
    }
    classify_failure(status, bytes)
}

/// The answer as `T`, or why not.
pub(crate) fn decode<T: DeserializeOwned>(
    answer: Result<Answer, String>,
    classify: fn(u16, &[u8]) -> RequestError,
) -> Result<T, RequestError> {
    let answer = answer.map_err(RequestError::Unknown)?;
    if !(200..300).contains(&answer.status) {
        return Err(classify(answer.status, &answer.bytes));
    }
    serde_json::from_slice(&answer.bytes)
        .map_err(|_| RequestError::Unknown("the server's answer could not be read".to_owned()))
}

/// Success with no body wanted.
pub(crate) fn decode_unit(answer: Result<Answer, String>) -> Result<(), RequestError> {
    let answer = answer.map_err(RequestError::Unknown)?;
    if (200..300).contains(&answer.status) {
        Ok(())
    } else {
        Err(classify_failure(answer.status, &answer.bytes))
    }
}

fn json<B: Serialize + ?Sized>(body: &B) -> Body {
    Body::Json(serde_json::to_string(body).unwrap_or_else(|_| "null".to_owned()))
}

/// `path?query`.
pub(crate) fn with_query<Q: Serialize + ?Sized>(path: &str, query: &Q) -> String {
    match serde_urlencoded::to_string(query) {
        Ok(query) if !query.is_empty() => format!("{path}?{query}"),
        _ => path.to_owned(),
    }
}

pub(crate) async fn get<T: DeserializeOwned>(path: &str) -> Result<T, RequestError> {
    decode(
        send("GET", path, Body::None, TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(
    path: &str,
    body: &B,
) -> Result<T, RequestError> {
    decode(
        send("POST", path, json(body), TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn post_empty<T: DeserializeOwned>(path: &str) -> Result<T, RequestError> {
    decode(
        send("POST", path, Body::None, TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn post_unit(path: &str) -> Result<(), RequestError> {
    decode_unit(send("POST", path, Body::None, TIMEOUT).await)
}

pub(crate) async fn put<T: DeserializeOwned, B: Serialize + ?Sized>(
    path: &str,
    body: &B,
) -> Result<T, RequestError> {
    decode(
        send("PUT", path, json(body), TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn patch<T: DeserializeOwned, B: Serialize + ?Sized>(
    path: &str,
    body: &B,
) -> Result<T, RequestError> {
    decode(
        send("PATCH", path, json(body), TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn delete<T: DeserializeOwned>(path: &str) -> Result<T, RequestError> {
    decode(
        send("DELETE", path, Body::None, TIMEOUT).await,
        classify_failure,
    )
}

pub(crate) async fn delete_unit(path: &str) -> Result<(), RequestError> {
    decode_unit(send("DELETE", path, Body::None, TIMEOUT).await)
}

/// Repeats a read that is safe to repeat, with backoff, while its outcome is unknown.
pub(crate) async fn retry_read<T, F, Fut>(mut call: F) -> Result<T, RequestError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, RequestError>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Err(RequestError::Unknown(_)) if attempt < READ_ATTEMPTS => {
                sleep(crate::realtime::backoff(attempt)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Percent-encodes an attachment's name for its header, as the desktop client does.
pub(crate) fn percent_encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_rejected_and_everything_else_is_unknown() {
        let body = br#"{"code":"forbidden","message":"no"}"#;
        assert!(matches!(
            classify_failure(403, body),
            RequestError::Rejected(_)
        ));
        assert!(matches!(
            classify_failure(502, b"<html>bad gateway</html>"),
            RequestError::Unknown(_)
        ));
        assert!(matches!(
            classify_failure(404, b"not json"),
            RequestError::Unknown(_)
        ));
        let slow = br#"{"code":"unavailable","message":"took too long"}"#;
        assert!(matches!(
            classify_search_failure(503, slow),
            RequestError::Rejected(_)
        ));
        assert!(matches!(
            classify_failure(503, slow),
            RequestError::Unknown(_)
        ));
    }

    #[test]
    fn queries_and_names_are_encoded() {
        #[derive(Serialize)]
        struct Q {
            q: &'static str,
            limit: Option<u32>,
        }
        assert_eq!(
            with_query(
                "/s",
                &Q {
                    q: "a b&c",
                    limit: Some(5)
                }
            ),
            "/s?q=a+b%26c&limit=5"
        );
        assert_eq!(with_query("/s", &Q { q: "", limit: None }), "/s?q=");
        assert_eq!(percent_encode("résumé 1.pdf"), "r%C3%A9sum%C3%A9%201.pdf");
    }
}
