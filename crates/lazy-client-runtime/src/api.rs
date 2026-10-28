//! Authenticated HTTP to the Lazy server, with failures classified for the core.

use std::sync::RwLock;
use std::time::Duration;

use lazy_client_core::RequestError;
use lazy_protocol::{ApiError, SessionToken};
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
        finish(request.send().await).await
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
        finish(request.send().await).await
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
        finish(request.send().await).await
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
        finish(request.send().await).await
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
        finish(request.send().await).await
    }

    /// `POST` with no body, answered with JSON.
    pub(crate) async fn post_empty<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, RequestError> {
        let request = self.with_auth(self.http.post(self.url(path)), self.token().as_ref());
        finish(request.send().await).await
    }

    /// `POST` with no body, where any success status counts and the body, if any, is ignored.
    pub(crate) async fn post_unit(&self, path: &str) -> Result<(), RequestError> {
        let request = self.with_auth(self.http.post(self.url(path)), self.token().as_ref());
        finish_unit(request.send().await).await
    }

    /// `DELETE`, where any success status counts and the body, if any, is ignored.
    pub(crate) async fn delete_unit(&self, path: &str) -> Result<(), RequestError> {
        let request = self.with_auth(self.http.delete(self.url(path)), self.token().as_ref());
        finish_unit(request.send().await).await
    }

    /// Revokes a session. Uses the given token because the client's own token is already cleared.
    pub(crate) async fn logout(&self, token: &SessionToken) -> Result<(), RequestError> {
        let request = self.with_auth(
            self.http.post(self.url(lazy_protocol::paths::LOGOUT)),
            Some(token),
        );
        match request.send().await {
            Ok(response) if response.status().is_success() => Ok(()),
            Ok(response) => Err(RequestError::Unknown(format!(
                "server returned {}",
                response.status()
            ))),
            Err(error) => Err(RequestError::Unknown(describe(&error))),
        }
    }
}

fn describe(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else {
        error.to_string()
    }
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
        return serde_json::from_slice(&bytes)
            .map_err(|e| RequestError::Unknown(format!("malformed response: {e}")));
    }
    Err(classify_failure(status, &bytes))
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
