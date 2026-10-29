//! Logging (llm-docs/DECISIONS.md, D24): structured `tracing` events that never carry credentials
//! or content.
//!
//! **What is logged.** One line per request, after it is answered: the request id, the method, the
//! *matched route pattern* (never the path: a webhook address holds its secret, a conversation path
//! holds ids that mean nothing in a log), the status, the latency in milliseconds, the signed-in
//! person's id when the request was authenticated, the declared size of the body in and the size of
//! the body out where they are known, and the error code of a refusal. Inside a request, every event
//! carries the request id through the `request` span. Startup logs the effective configuration
//! without a secret in it ([`describe_database_url`], [`describe_url`]).
//!
//! **What is never logged:** passwords, tokens, invitation codes, webhook secrets, message or reply
//! text, names, topics, search queries, file names, hashes, pictures, request or response bodies,
//! headers other than the request id, query strings. The mechanisms:
//!
//! - the request layer ([`log_requests`]) reads only the method, the matched route, the status, the
//!   sizes and the one header, and never formats a URI;
//! - an error is logged through [`Describe`], which says what *kind* of failure it was (a status
//!   code, an SQLSTATE and constraint name, an I/O error kind, a serde category) and never its text,
//!   because the text of a database or parse error can quote the value that was refused;
//! - SQLx statement logging is off ([`crate::db::quiet`]), the one target that logs request-shaped
//!   data at the most verbose level of a dependency (`axum::rejection`, which prints why a body was
//!   refused, quoting it) is switched off whatever the filter says, and `log` records from other
//!   crates are not bridged into the output;
//! - a panic is reported by place, never by message ([`install_panic_hook`]);
//! - `Config`'s `Debug` hides the database URL, and the secret-carrying protocol types print
//!   `<redacted>`;
//! - `tests/log_audit.rs` reads this crate's source and fails on any `tracing` call that formats with
//!   `Debug`, or has a field nobody has reviewed.
//!
//! **Configuration.** `LAZY_LOG` (then `RUST_LOG`) is an `EnvFilter` (default `info,sqlx=warn`);
//! `LAZY_LOG_FORMAT` (or `--log-format`) is `text` (default) or `json` (one object per line).

use std::fmt;
use std::io::{IsTerminal, Write as _};
use std::panic::AssertUnwindSafe;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::HttpBody;
use axum::extract::{MatchedPath, Request};
use axum::http::header::CONTENT_LENGTH;
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use lazy_domain::UserId;
use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Instrument, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{EnvFilter, Layer};

use crate::error::AppError;

// --- Setup -------------------------------------------------------------------------------------

/// How log events are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// Human-readable lines.
    #[default]
    Text,
    /// One JSON object per line: `timestamp`, `level`, `target`, `message`, `fields`, `spans`.
    Json,
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text.trim().to_ascii_lowercase().as_str() {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            _ => Err("the log format is `text` or `json`".to_owned()),
        }
    }
}

/// The filter used when none is given.
pub const DEFAULT_FILTER: &str = "info,sqlx=warn";

/// Directives added after the operator's, so no filter can switch them on: `axum::rejection` logs
/// why a request body or query was refused, quoting the part that was wrong, and `tungstenite`
/// (the WebSocket library) logs every frame it sends or receives at TRACE, message text included.
const ALWAYS_OFF: &[&str] = &[
    "axum::rejection=off",
    "tungstenite=off",
    "tokio_tungstenite=off",
];

/// The filter for `spec` (an `EnvFilter` string; the default if absent or empty), and a note when
/// `spec` was not valid and the default was used instead.
pub fn filter(spec: Option<&str>) -> (EnvFilter, Option<&'static str>) {
    let (mut filter, note) = match spec.map(str::trim).filter(|text| !text.is_empty()) {
        None => (EnvFilter::new(DEFAULT_FILTER), None),
        Some(spec) => match EnvFilter::try_new(spec) {
            Ok(filter) => (filter, None),
            Err(_) => (
                EnvFilter::new(DEFAULT_FILTER),
                Some("the log filter is not valid; using the default"),
            ),
        },
    };
    for directive in ALWAYS_OFF {
        filter = filter.add_directive(directive.parse().expect("a fixed directive"));
    }
    (filter, note)
}

/// [`filter`] of `LAZY_LOG`, else `RUST_LOG`.
pub fn filter_from_env() -> (EnvFilter, Option<&'static str>) {
    let spec = ["LAZY_LOG", "RUST_LOG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty());
    filter(spec.as_deref())
}

/// A subscriber writing `format` events that pass `filter` to `writer`.
pub fn build<W>(
    format: LogFormat,
    filter: EnvFilter,
    writer: W,
    ansi: bool,
) -> Box<dyn Subscriber + Send + Sync>
where
    W: for<'a> MakeWriter<'a> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => Box::new(
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(writer)
                .with_ansi(ansi)
                .finish(),
        ),
        LogFormat::Json => Box::new(
            tracing_subscriber::registry()
                .with(filter)
                .with(JsonLayer { writer }),
        ),
    }
}

/// Installs the process-wide subscriber (to standard error, colours only on a terminal) and the
/// panic hook. A second call changes nothing.
pub fn init(format: LogFormat) {
    let (filter, note) = filter_from_env();
    let subscriber = build(
        format,
        filter,
        std::io::stderr,
        std::io::stderr().is_terminal(),
    );
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        return;
    }
    install_panic_hook();
    if let Some(note) = note {
        tracing::warn!("{note}");
    }
}

/// Reports a panic by thread and place and nothing else: the message of a panic can quote the data
/// it was working on (an `unwrap` on an error prints the error), and the default hook would print it
/// to standard error, outside every filter.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let place = info.location().map_or_else(
            || "unknown".to_owned(),
            |location| format!("{}:{}", location.file(), location.line()),
        );
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_owned();
        tracing::error!(place = %place, thread = %thread, "panic (the message is withheld)");
    }));
}

// --- What may be said about an error ------------------------------------------------------------

/// A description of a failure that is safe to log: its kind, never its text.
pub trait Describe {
    fn describe(&self) -> String;
}

impl Describe for AppError {
    fn describe(&self) -> String {
        match self {
            // The message of a refusal is written for the person and may name what they sent.
            Self::Api(error) => format!("refused ({})", code_text(&error.code)),
            Self::Database(error) => error.describe(),
            // Written by the server's own code, without data.
            Self::Internal(message) => message.clone(),
        }
    }
}

impl Describe for sqlx::Error {
    fn describe(&self) -> String {
        match self {
            // The message of a database error can quote the value it refused.
            Self::Database(error) => format!(
                "database error (sqlstate {}, constraint {})",
                error.code().as_deref().unwrap_or("-"),
                error.constraint().unwrap_or("-"),
            ),
            Self::Io(error) => format!("database connection error ({:?})", error.kind()),
            Self::PoolTimedOut => "no database connection was free in time".to_owned(),
            Self::PoolClosed => "the database pool is closed".to_owned(),
            Self::Configuration(_) => "database configuration error".to_owned(),
            Self::Tls(_) => "database TLS error".to_owned(),
            Self::Protocol(_) => "database protocol error".to_owned(),
            Self::RowNotFound => "a row was not found".to_owned(),
            Self::ColumnDecode { index, .. } => format!("cannot decode column {index}"),
            Self::Decode(_) => "cannot decode a value".to_owned(),
            Self::WorkerCrashed => "the database worker crashed".to_owned(),
            _ => "other database error".to_owned(),
        }
    }
}

impl Describe for tokio::task::JoinError {
    fn describe(&self) -> String {
        // The text of a panic is in its `Display`.
        if self.is_panic() {
            "the task panicked".to_owned()
        } else {
            "the task was cancelled".to_owned()
        }
    }
}

impl Describe for serde_json::Error {
    fn describe(&self) -> String {
        // The text of a parse error quotes the value that did not fit.
        format!("{:?} error in JSON", self.classify())
    }
}

impl Describe for std::io::Error {
    fn describe(&self) -> String {
        format!("I/O error ({:?})", self.kind())
    }
}

fn code_text(code: &lazy_protocol::ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// `host:port/database` of a Postgres URL, without the user, password or options.
pub fn describe_database_url(url: &str) -> String {
    use sqlx::postgres::PgConnectOptions;
    match PgConnectOptions::from_str(url) {
        Ok(options) => format!(
            "{}:{}/{}",
            options.get_host(),
            options.get_port(),
            options.get_database().unwrap_or("")
        ),
        Err(_) => "<not a postgres url>".to_owned(),
    }
}

/// `scheme://host:port` of a URL, without credentials, path, query or fragment.
pub fn describe_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return "<not a url>".to_owned();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}")
}

// --- The request layer --------------------------------------------------------------------------

/// The header that carries a request id, in and out.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// What a request is logged as when no route matched (the path may hold a secret).
pub const UNMATCHED_ROUTE: &str = "<unmatched>";

/// A request that takes longer than this is logged as a warning (unless it moved a megabyte or
/// more: a big file is slow honestly).
pub const SLOW_REQUEST: Duration = Duration::from_secs(1);

const BIG_TRANSFER: u64 = 1024 * 1024;

/// The error code of a refusal, attached to the response so the request line can say it.
#[derive(Debug, Clone)]
pub(crate) struct LoggedErrorCode(pub String);

/// Shared between the request layer and the authentication extractor: who the request was made by.
#[derive(Debug, Clone, Default)]
pub(crate) struct RequestInfo(Arc<OnceLock<UserId>>);

impl RequestInfo {
    pub(crate) fn note_user(&self, user: UserId) {
        let _ = self.0.set(user);
    }
}

/// Whether `id` is acceptable as a request id from a client: 1 to 64 letters, digits, `.`, `_`, `-`.
pub fn request_id_is_well_formed(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn request_id_of(headers: &HeaderMap) -> String {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|id| request_id_is_well_formed(id))
        .map_or_else(|| uuid::Uuid::now_v7().simple().to_string(), str::to_owned)
}

fn declared_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// The middleware that logs every request, gives it an id (the client's `x-request-id` if it is
/// well formed, else a fresh one), echoes the id in the response, and turns a panic in a handler
/// into a logged `internal` error instead of a dropped connection. See the module documentation for
/// what is logged.
pub async fn log_requests(mut request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    let route = request.extensions().get::<MatchedPath>().map_or_else(
        || UNMATCHED_ROUTE.to_owned(),
        |path| path.as_str().to_owned(),
    );
    let request_id = request_id_of(request.headers());
    let bytes_in = declared_length(request.headers());
    let info = RequestInfo::default();
    request.extensions_mut().insert(info.clone());

    let span = tracing::info_span!("request", request_id = %request_id);
    let outcome = AssertUnwindSafe(next.run(request))
        .catch_unwind()
        .instrument(span.clone())
        .await;
    let mut response = match outcome {
        Ok(response) => response,
        // The panic hook has said where; nothing of the request or the panic is repeated here.
        Err(_) => {
            span.in_scope(|| AppError::internal("a request handler panicked").into_response())
        }
    };
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(REQUEST_ID_HEADER), value);
    }

    let status = response.status();
    let latency = started.elapsed();
    let bytes_out =
        declared_length(response.headers()).or_else(|| response.body().size_hint().exact());
    let error_code = response
        .extensions()
        .get::<LoggedErrorCode>()
        .map(|code| code.0.clone());
    let user_id = info.0.get().map(ToString::to_string);
    let slow = latency > SLOW_REQUEST
        && bytes_in.unwrap_or(0) < BIG_TRANSFER
        && bytes_out.unwrap_or(0) < BIG_TRANSFER;
    let latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX);

    macro_rules! line {
        ($level:ident) => {
            tracing::$level!(
                request_id = %request_id,
                method = %method,
                route = %route,
                status = status.as_u16(),
                latency_ms,
                user_id = user_id.as_deref(),
                bytes_in,
                bytes_out,
                error_code = error_code.as_deref(),
                slow,
                "request"
            )
        };
    }
    if status.is_server_error() || slow {
        line!(warn);
    } else if route == lazy_protocol::api::paths::HEALTH {
        line!(debug);
    } else {
        line!(info);
    }
    response
}

// --- JSON lines ---------------------------------------------------------------------------------

/// Writes each event as one JSON object on one line: `timestamp` (RFC 3339, UTC), `level`,
/// `target`, `message`, `fields` (the event's other fields) and `spans` (from the outermost, each
/// with its `name` and fields). Written here, not taken from `tracing-subscriber`'s `json` feature,
/// to keep the dependency tree as it is.
struct JsonLayer<W> {
    writer: W,
}

struct SpanFields(Map<String, Value>);

#[derive(Default)]
struct JsonVisitor {
    message: Option<String>,
    fields: Map<String, Value>,
}

impl JsonVisitor {
    fn put(&mut self, field: &Field, value: Value) {
        if field.name() == "message" {
            self.message = Some(match value {
                Value::String(text) => text,
                other => other.to_string(),
            });
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for JsonVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, Value::String(value.to_owned()));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, Value::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.put(field, Value::from(value));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, Value::String(format!("{value:?}")));
    }
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        let mut visitor = JsonVisitor::default();
        attributes.record(&mut visitor);
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(SpanFields(visitor.fields));
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: Context<'_, S>,
    ) {
        if let Some(span) = context.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<SpanFields>()
        {
            let mut visitor = JsonVisitor::default();
            values.record(&mut visitor);
            fields.0.extend(visitor.fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        let mut object = Map::new();
        let timestamp = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        object.insert("timestamp".to_owned(), Value::String(timestamp));
        object.insert("level".to_owned(), Value::from(metadata.level().as_str()));
        object.insert("target".to_owned(), Value::from(metadata.target()));
        object.insert(
            "message".to_owned(),
            Value::String(visitor.message.unwrap_or_default()),
        );
        object.insert("fields".to_owned(), Value::Object(visitor.fields));
        let mut spans = Vec::new();
        if let Some(scope) = context.event_scope(event) {
            for span in scope.from_root() {
                let mut entry = span
                    .extensions()
                    .get::<SpanFields>()
                    .map(|fields| fields.0.clone())
                    .unwrap_or_default();
                entry.insert("name".to_owned(), Value::from(span.name()));
                spans.push(Value::Object(entry));
            }
        }
        object.insert("spans".to_owned(), Value::Array(spans));
        let mut line = Value::Object(object).to_string();
        line.push('\n');
        // One write, so lines from different threads do not interleave.
        let _ = self.writer.make_writer().write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn request_ids_from_clients_are_short_and_plain() {
        for good in ["a", "req-123", "01HZX.9_k", &"x".repeat(64)] {
            assert!(request_id_is_well_formed(good), "{good}");
        }
        for bad in [
            "",
            "has space",
            "new\nline",
            "semi;colon",
            "é",
            &"x".repeat(65),
        ] {
            assert!(!request_id_is_well_formed(bad), "{bad:?}");
        }
    }

    #[test]
    fn database_and_other_urls_are_described_without_credentials() {
        let described = describe_database_url(
            "postgres://lazy:hunter2secret@db.internal:6543/chat?sslmode=require",
        );
        assert_eq!(described, "db.internal:6543/chat");
        assert_eq!(describe_database_url("not a url"), "<not a postgres url>");
        let url = describe_url("https://user:pw-secret@chat.example.com:8443/path/hooks?x=1#f");
        assert_eq!(url, "https://chat.example.com:8443");
        assert_eq!(describe_url("chat.example.com"), "<not a url>");
    }

    #[test]
    fn errors_are_described_by_kind_not_by_text() {
        let refused = AppError::new(
            lazy_protocol::ErrorCode::NameTaken,
            "the name SECRETNAME is taken",
        );
        assert_eq!(refused.describe(), "refused (name_taken)");
        let decode: Result<u32, _> = serde_json::from_str("\"SECRETVALUE\"");
        let text = decode.unwrap_err().describe();
        assert!(!text.contains("SECRETVALUE"), "{text}");
        let io = std::io::Error::other("SECRETPATH");
        assert_eq!(io.describe(), "I/O error (Other)");
        assert_eq!(
            sqlx::Error::PoolTimedOut.describe(),
            "no database connection was free in time"
        );
    }

    #[derive(Debug)]
    struct Refusal;

    impl fmt::Display for Refusal {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("invalid input syntax for type uuid: \"SECRETINPUT\"")
        }
    }

    impl std::error::Error for Refusal {}

    impl sqlx::error::DatabaseError for Refusal {
        fn message(&self) -> &str {
            "invalid input syntax for type uuid: \"SECRETINPUT\""
        }

        fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
            Some("22P02".into())
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    #[test]
    fn a_database_error_is_described_by_its_code_and_never_its_message() {
        let error = AppError::Database(sqlx::Error::Database(Box::new(Refusal)));
        let text = error.describe();
        assert_eq!(text, "database error (sqlstate 22P02, constraint -)");
    }

    #[test]
    fn the_default_filter_applies_and_an_invalid_one_falls_back_with_a_note() {
        let (_, note) = filter(None);
        assert!(note.is_none());
        let (_, note) = filter(Some("info,,=="));
        assert!(note.is_some());
        let (filter, note) = filter(Some("trace"));
        assert!(note.is_none());
        // The rejection target stays off whatever was asked.
        for directive in ALWAYS_OFF {
            assert!(filter.to_string().contains(directive), "{directive}");
        }
        assert_eq!("json".parse::<LogFormat>(), Ok(LogFormat::Json));
        assert_eq!(" TEXT ".parse::<LogFormat>(), Ok(LogFormat::Text));
        assert!("yaml".parse::<LogFormat>().is_err());
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Buffer {
        type Writer = Buffer;

        fn make_writer(&'a self) -> Buffer {
            self.clone()
        }
    }

    #[test]
    fn json_lines_have_the_documented_shape_and_carry_span_fields() {
        let buffer = Buffer::default();
        let subscriber = build(
            LogFormat::Json,
            EnvFilter::new("trace"),
            buffer.clone(),
            false,
        );
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("request", request_id = %"abc-123");
            let _entered = span.enter();
            tracing::warn!(
                status = 503u16,
                route = "/v1/x/{id}",
                slow = true,
                "request"
            );
        });
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert_eq!(text.lines().count(), 1);
        let line: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(line["level"], "WARN");
        assert_eq!(line["message"], "request");
        assert_eq!(line["fields"]["status"], 503);
        assert_eq!(line["fields"]["route"], "/v1/x/{id}");
        assert_eq!(line["fields"]["slow"], true);
        assert_eq!(line["spans"][0]["name"], "request");
        assert_eq!(line["spans"][0]["request_id"], "abc-123");
        assert!(line["timestamp"].as_str().unwrap().ends_with('Z'));
        assert!(line["target"].as_str().unwrap().contains("lazy_server"));
    }
}
