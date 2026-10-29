//! Authenticated HTTP to the Lazy server, with failures classified for the core.

use std::sync::RwLock;
use std::time::Duration;

use lazy_client_core::RequestError;
use lazy_protocol::{ApiError, ErrorCode, SessionToken};
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Largest response body accepted.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

pub(crate) struct Api {
    http: reqwest::Client,
    base: String,
    token: RwLock<Option<SessionToken>>,
}

impl Api {
    pub(crate) fn new(base: &str, timeout: Duration) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)))
            .user_agent(concat!("lazy-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_owned(),
            token: RwLock::new(None),
        })
    }

    pub(crate) fn set_token(&self, token: Option<SessionToken>) {
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = token;
    }

    pub(crate) fn token(&self) -> Option<SessionToken> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// The realtime WebSocket URL (`ws`/`wss` scheme).
    pub(crate) fn realtime_url(&self) -> String {
        let base = if let Some(rest) = self.base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = self.base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            self.base.clone()
        };
        format!("{base}{}", lazy_protocol::paths::REALTIME)
    }

    /// Sends a request and logs what came of it: the method, the route pattern (never the URL,
    /// whose query can hold a search), the status or the reason it failed, and how long it took.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let request = request.build()?;
        let method = request.method().clone();
        let route = route_pattern(request.url().path());
        let started = std::time::Instant::now();
        let result = self.http.execute(request).await;
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &result {
            Ok(response) if response.status().is_server_error() => tracing::warn!(
                %method, route, status = response.status().as_u16(), elapsed_ms,
                "the server could not answer a request"
            ),
            Ok(response) => tracing::debug!(
                %method, route, status = response.status().as_u16(), elapsed_ms, "request answered"
            ),
            Err(error) => tracing::warn!(
                %method, route, reason = describe(error), elapsed_ms, "a request got no answer"
            ),
        }
        result
    }

    fn with_auth(
        &self,
        request: reqwest::RequestBuilder,
        token: Option<&SessionToken>,
    ) -> reqwest::RequestBuilder {
        match token {
            Some(token) => request.bearer_auth(token.expose()),
            None => request,
        }
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, RequestError> {
        let request = self.with_auth(self.http.get(self.url(path)), self.token().as_ref());
        finish(self.send(request).await).await
    }

    pub(crate) async fn get_query<T: DeserializeOwned, Q: Serialize + ?Sized>(
        &self,
        path: &str,
        query: &Q,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(
            self.http.get(self.url(path)).query(query),
            self.token().as_ref(),
        );
        finish(self.send(request).await).await
    }

    /// `GET` of a search. Unlike any other request, a refusal because the server could not answer
    /// in time (`503` with the `unavailable` code) is `Rejected`: a search changes nothing, and
    /// asking again unchanged would only time out again, so it is not repeated on its own.
    pub(crate) async fn search<T: DeserializeOwned, Q: Serialize + ?Sized>(
        &self,
        path: &str,
        query: &Q,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(
            self.http.get(self.url(path)).query(query),
            self.token().as_ref(),
        );
        finish_with(self.send(request).await, classify_search_failure).await
    }

    pub(crate) async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(
            self.http.post(self.url(path)).json(body),
            self.token().as_ref(),
        );
        finish(self.send(request).await).await
    }

    pub(crate) async fn put<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(
            self.http.put(self.url(path)).json(body),
            self.token().as_ref(),
        );
        finish(self.send(request).await).await
    }

    pub(crate) async fn patch<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(
            self.http.patch(self.url(path)).json(body),
            self.token().as_ref(),
        );
        finish(self.send(request).await).await
    }

    /// `POST` with no body, answered with JSON.
    pub(crate) async fn post_empty<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(self.http.post(self.url(path)), self.token().as_ref());
        finish(self.send(request).await).await
    }

    /// `POST` with no body, where any success status counts and the body, if any, is ignored.
    pub(crate) async fn post_unit(&self, path: &str) -> Result<(), RequestError> {
        let request = self.with_auth(self.http.post(self.url(path)), self.token().as_ref());
        finish_unit(self.send(request).await).await
    }

    /// `DELETE`, answered with JSON.
    pub(crate) async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T, RequestError> {
        let request = self.with_auth(self.http.delete(self.url(path)), self.token().as_ref());
        finish(self.send(request).await).await
    }

    /// `DELETE`, where any success status counts and the body, if any, is ignored.
    pub(crate) async fn delete_unit(&self, path: &str) -> Result<(), RequestError> {
        let request = self.with_auth(self.http.delete(self.url(path)), self.token().as_ref());
        finish_unit(self.send(request).await).await
    }

    /// `PUT` of a file's bytes, streamed from `body` (`length` bytes). Answered with the pending
    /// attachment. The whole transfer may take as long as `timeout`, which replaces the client's
    /// timeout for ordinary requests.
    pub(crate) async fn put_attachment(
        &self,
        path: &str,
        name: &str,
        media_type: &str,
        length: u64,
        body: reqwest::Body,
        timeout: Duration,
    ) -> Result<lazy_protocol::AttachmentResponse, RequestError> {
        let request = self
            .http
            .put(self.url(path))
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .header(reqwest::header::CONTENT_LENGTH, length)
            .header(
                lazy_protocol::paths::ATTACHMENT_NAME_HEADER,
                percent_encode(name),
            )
            .body(body);
        let request = self.with_auth(request, self.token().as_ref());
        finish(self.send(request).await).await
    }

    /// `PUT` of a custom emoji's image (small enough to hold whole), named in a header.
    pub(crate) async fn put_emoji(
        &self,
        path: &str,
        name: &str,
        media_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<lazy_protocol::CustomEmojiResponse, RequestError> {
        let request = self
            .http
            .put(self.url(path))
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .header(lazy_protocol::paths::EMOJI_NAME_HEADER, name)
            .body(body);
        let request = self.with_auth(request, self.token().as_ref());
        finish(self.send(request).await).await
    }

    /// `PUT` of the person's avatar picture (small enough to hold whole).
    pub(crate) async fn put_avatar(
        &self,
        path: &str,
        media_type: &str,
        body: Vec<u8>,
        timeout: Duration,
    ) -> Result<lazy_protocol::ProfileResponse, RequestError> {
        let request = self
            .http
            .put(self.url(path))
            .timeout(timeout)
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(body);
        let request = self.with_auth(request, self.token().as_ref());
        finish(self.send(request).await).await
    }

    /// `GET` that returns the response once its status is a success, so the body can be read as
    /// it arrives. A failure status is classified like any other.
    pub(crate) async fn get_response(
        &self,
        path: &str,
        timeout: Duration,
    ) -> Result<reqwest::Response, RequestError> {
        let request = self.with_auth(
            self.http.get(self.url(path)).timeout(timeout),
            self.token().as_ref(),
        );
        let response = self
            .send(request)
            .await
            .map_err(|e| RequestError::Unknown(describe(&e)))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let bytes = response.bytes().await.unwrap_or_default();
        Err(classify_failure(status, &bytes))
    }

    /// `GET` of a file's bytes, at most `max` of them.
    pub(crate) async fn get_bytes(
        &self,
        path: &str,
        max: u64,
        timeout: Duration,
    ) -> Result<Vec<u8>, RequestError> {
        let mut response = self.get_response(path, timeout).await?;
        if response.content_length().is_some_and(|len| len > max) {
            return Err(RequestError::Unknown("response too large".into()));
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() as u64 + chunk.len() as u64 > max {
                        return Err(RequestError::Unknown("response too large".into()));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => return Ok(bytes),
                Err(error) => return Err(RequestError::Unknown(describe(&error))),
            }
        }
    }

    /// Revokes a session. Uses the given token because the client's own token is already cleared.
    pub(crate) async fn logout(&self, token: &SessionToken) -> Result<(), RequestError> {
        let request = self.with_auth(
            self.http.post(self.url(lazy_protocol::paths::LOGOUT)),
            Some(token),
        );
        match self.send(request).await {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(RequestError::Unknown(format!(
                "server returned {}",
                response.status()
            ))),
            Err(error) => Err(RequestError::Unknown(describe(&error))),
        }
    }
}

/// A request path as a route pattern: the words of the route are kept and every identifier
/// becomes `:id`, so a log line says which endpoint was called and not about what.
///
/// A segment stays only if it is a short lowercase word or the version (`v1`). Identifiers (which hold digits) and
/// anything else a path could carry are replaced.
fn route_pattern(path: &str) -> String {
    let mut out = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        let word = segment.len() <= 24
            && segment
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'_');
        let version = segment
            .strip_prefix('v')
            .is_some_and(|n| (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
        out.push_str(if word || version { segment } else { ":id" });
    }
    if out.is_empty() {
        out.push('/');
    }
    out
}

/// Percent-encodes a file name for a header: everything but unreserved ASCII becomes `%XX`.
fn percent_encode(name: &str) -> String {
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

/// Says what went wrong with a request in fixed words.
///
/// `reqwest::Error`'s own text names the request URL, whose query string can hold a search; this
/// never does, so the words are safe to show, log, and keep in the diagnostics.
pub(crate) fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else if error.is_redirect() {
        "too many redirects".to_owned()
    } else if error.is_decode() {
        "the response could not be decoded".to_owned()
    } else if error.is_body() {
        "the connection broke while sending or receiving".to_owned()
    } else if error.is_builder() {
        "the request could not be built".to_owned()
    } else {
        "the request failed".to_owned()
    }
}

/// Says what is wrong with some JSON without quoting it.
///
/// `serde_json`'s own text can repeat the offending value (`invalid type: string "..."`), which
/// for a stored file or a frame is a message, a search, or a name. Only the kind and the place
/// are kept.
pub(crate) fn describe_json(error: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let what = match error.classify() {
        Category::Io => "could not be read",
        Category::Syntax => "is not valid JSON",
        Category::Data => "has the wrong shape",
        Category::Eof => "ends too soon",
    };
    format!(
        "it {what}, near line {} column {}",
        error.line(),
        error.column()
    )
}

/// Turns a response into a value or a classified error.
///
/// A body that parses as an [`ApiError`] on a 4xx status, with a code that denotes a definitive
/// refusal, is `Rejected`: the server saw the request and did not act on it. Anything else that
/// is not a clean success (a timeout, a dropped connection, a 5xx, an unparseable body) is
/// `Unknown`, because the server may have processed the request.
async fn finish<T: DeserializeOwned>(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<T, RequestError> {
    finish_with(response, classify_failure).await
}

/// [`finish`] with its own say on what a failure status means.
async fn finish_with<T: DeserializeOwned>(
    response: Result<reqwest::Response, reqwest::Error>,
    classify: fn(reqwest::StatusCode, &[u8]) -> RequestError,
) -> Result<T, RequestError> {
    let response = response.map_err(|e| RequestError::Unknown(describe(&e)))?;
    let status = response.status();
    if response
        .content_length()
        .is_some_and(|len| len > MAX_RESPONSE_BYTES)
    {
        return Err(RequestError::Unknown("response too large".into()));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| RequestError::Unknown(describe(&e)))?;
    if status.is_success() {
        return serde_json::from_slice(&bytes).map_err(|e| {
            RequestError::Unknown(format!("malformed response ({})", describe_json(&e)))
        });
    }
    Err(classify(status, &bytes))
}

/// Like [`finish`] for requests whose success carries nothing to apply: any 2xx status counts,
/// whatever the body (`204 No Content`, or JSON the client has no use for).
async fn finish_unit(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<(), RequestError> {
    let response = response.map_err(|e| RequestError::Unknown(describe(&e)))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let bytes = response.bytes().await.unwrap_or_default();
    Err(classify_failure(status, &bytes))
}

fn classify_failure(status: reqwest::StatusCode, bytes: &[u8]) -> RequestError {
    match serde_json::from_slice::<ApiError>(bytes) {
        Ok(error) if status.is_client_error() && error.code.is_definitive_rejection() => {
            RequestError::Rejected(error)
        }
        _ => RequestError::Unknown(format!("server returned {status}")),
    }
}

/// [`classify_failure`] for a read that changes nothing and can take too long: the server's
/// `unavailable` answer is `Rejected`, so the person is told the search took too long. An
/// unavailable answer from anything else (a proxy, a body that is not ours) stays `Unknown`.
fn classify_search_failure(status: reqwest::StatusCode, bytes: &[u8]) -> RequestError {
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && let Ok(error) = serde_json::from_slice::<ApiError>(bytes)
        && error.code == ErrorCode::Unavailable
    {
        return RequestError::Rejected(error);
    }
    classify_failure(status, bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        classify_failure, classify_search_failure, describe, describe_json, percent_encode,
        route_pattern,
    };
    use lazy_client_core::RequestError;
    use lazy_protocol::{ApiError, ErrorCode};
    use reqwest::StatusCode;

    #[test]
    fn a_route_pattern_keeps_the_words_and_drops_the_identifiers() {
        assert_eq!(route_pattern("/"), "/");
        assert_eq!(route_pattern("/v1/auth/login"), "/v1/auth/login");
        let id = "0192f3b4-7c1e-7a55-8e0d-3b6a9d2c4f10";
        assert_eq!(
            route_pattern(&format!("/v1/messages/{id}/reactions")),
            "/v1/messages/:id/reactions"
        );
        // A secret or a name in a path is never a word of the route.
        assert_eq!(route_pattern("/hook/Secret-123"), "/hook/:id");
        assert_eq!(
            route_pattern("/x/averyveryveryverylongsegmentindeed"),
            "/x/:id"
        );
    }

    #[test]
    fn a_json_complaint_never_quotes_the_json() {
        // serde_json's own text repeats the value that was the wrong shape.
        let error = serde_json::from_str::<u32>("\"quokka zephyr\"").unwrap_err();
        assert!(error.to_string().contains("quokka"), "the premise: {error}");
        let told = describe_json(&error);
        assert!(
            !told.contains("quokka") && !told.contains("zephyr"),
            "{told}"
        );
        assert!(told.contains("wrong shape"), "{told}");
        let error = serde_json::from_str::<serde_json::Value>("{\"quokka\": ").unwrap_err();
        let told = describe_json(&error);
        assert!(!told.contains("quokka"), "{told}");
    }

    /// `reqwest`'s own text for most failures ends with the request's URL, query string included.
    #[test]
    fn a_request_failure_is_told_in_fixed_words_without_the_url() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime
            .block_on(async {
                reqwest::get(format!("http://{address}/v1/search?q=quokka+zephyr"))
                    .await?
                    .error_for_status()
            })
            .unwrap_err();
        server.join().unwrap();
        assert!(error.to_string().contains("quokka"), "the premise: {error}");
        let told = describe(&error);
        assert!(
            !told.contains("quokka") && !told.contains("127.0.0.1") && !told.contains("search"),
            "{told}"
        );
        assert!(!told.is_empty());
    }

    fn body(code: ErrorCode) -> Vec<u8> {
        serde_json::to_vec(&ApiError::new(code, "that search took too long")).unwrap()
    }

    #[test]
    fn only_a_search_keeps_the_unavailable_answer() {
        let unavailable = body(ErrorCode::Unavailable);
        assert!(matches!(
            classify_failure(StatusCode::SERVICE_UNAVAILABLE, &unavailable),
            RequestError::Unknown(_)
        ));
        assert!(matches!(
            classify_search_failure(StatusCode::SERVICE_UNAVAILABLE, &unavailable),
            RequestError::Rejected(error) if error.code == ErrorCode::Unavailable
        ));
        // Not the server's answer, or not the 503: classified as everywhere else.
        for (status, bytes) in [
            (
                StatusCode::SERVICE_UNAVAILABLE,
                b"<html>busy</html>".to_vec(),
            ),
            (StatusCode::SERVICE_UNAVAILABLE, body(ErrorCode::Internal)),
            (StatusCode::BAD_GATEWAY, unavailable.clone()),
            (StatusCode::INTERNAL_SERVER_ERROR, body(ErrorCode::Internal)),
        ] {
            assert!(
                matches!(
                    classify_search_failure(status, &bytes),
                    RequestError::Unknown(_)
                ),
                "{status}"
            );
        }
        // The rest is unchanged: a bad query is the server's refusal.
        assert!(matches!(
            classify_search_failure(StatusCode::BAD_REQUEST, &body(ErrorCode::InvalidRequest)),
            RequestError::Rejected(error) if error.code == ErrorCode::InvalidRequest
        ));
        assert!(matches!(
            classify_search_failure(StatusCode::TOO_MANY_REQUESTS, &body(ErrorCode::RateLimited)),
            RequestError::Rejected(_)
        ));
    }

    #[test]
    fn a_file_name_is_encoded_for_a_header() {
        assert_eq!(percent_encode("report.pdf"), "report.pdf");
        assert_eq!(
            percent_encode("my photo (1).png"),
            "my%20photo%20%281%29.png"
        );
        assert_eq!(
            percent_encode("日本語.txt"),
            "%E6%97%A5%E6%9C%AC%E8%AA%9E.txt"
        );
        assert_eq!(percent_encode("a/b\\c\r\n"), "a%2Fb%5Cc%0D%0A");
    }
}
