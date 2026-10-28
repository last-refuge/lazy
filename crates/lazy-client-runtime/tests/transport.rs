//! The runtime against an in-process stub of the Lazy server.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lazy_client_core::{
    ActionKind, ActionOutcome, CachedTimeline, CachedWorkspace, ContentCache, Effect, Input,
    JournalEntry, RequestError, SocketEvent,
};
use lazy_client_runtime::{
    BackoffPolicy, MemoryStorage, Runtime, RuntimeConfig, SecretVault, SecureStorage,
    SecureStorageChoice, StorageError, async_channel::Receiver, inspect_state,
};
use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, DisplayName, EventSeq, MessageDocument,
    MessageId, OperationId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use lazy_protocol::{
    AddMembersRequest, ApiError, BootstrapResponse, BrowseChannelsResponse, BrowsedChannel,
    ClientFrame, Conversation, ConversationResponse, ConversationUserState, CreateChannelRequest,
    ErrorCode, Event, EventEnvelope, LoginRequest, Message, OpenConversationRequest,
    OperationStatus, PROTOCOL_VERSION, SendMessageRequest, SendMessageResponse, ServerFrame,
    SessionResponse, SessionToken, UpdateConversationRequest, User, UserStateResponse, Workspace,
    WorkspaceBootstrap,
};
use tempfile::TempDir;

// ----- Stub server -----

#[derive(Clone, Copy)]
enum SendBehavior {
    Ok,
    Conflict,
    ServerError,
    Malformed,
}

/// What the conversation-management routes of the stub do with the next request.
#[derive(Clone, Copy)]
enum ApiBehavior {
    Ok,
    /// Success with an empty JSON object, where the route usually answers `204 No Content`.
    OkWithJson,
    NameTaken,
    Forbidden,
    ServerError,
}

/// A request to one of the conversation-management routes.
#[derive(Clone, Debug)]
struct Call {
    method: String,
    path: String,
    body: serde_json::Value,
}

struct Stub {
    user: User,
    workspace: WorkspaceId,
    /// Every request to the conversation-management routes, oldest first.
    calls: Mutex<Vec<Call>>,
    api_plan: Mutex<VecDeque<ApiBehavior>>,
    logins: AtomicUsize,
    sends: AtomicUsize,
    status_calls: AtomicUsize,
    status_failures: AtomicUsize,
    send_plan: Mutex<VecDeque<SendBehavior>>,
    /// `Authorization` header of each request, with its path.
    requests: Mutex<Vec<(String, Option<String>)>>,
    upgrades: AtomicUsize,
    /// The first this-many upgrade attempts are refused with 503.
    upgrade_failures: AtomicUsize,
    reject_upgrade_as_unauthorized: AtomicBool,
    sessions_started: AtomicUsize,
    drop_first_session_after_caught_up: AtomicBool,
    /// The first session answers its subscription with `snapshot_required` and hangs up.
    snapshot_required_on_first_session: AtomicBool,
    active_sessions: AtomicUsize,
    afters: Mutex<Vec<i64>>,
}

impl Stub {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            user: User {
                id: UserId::generate(),
                username: Username::parse("stubby").unwrap(),
                display_name: DisplayName::parse("Stubby").unwrap(),
            },
            workspace: WorkspaceId::generate(),
            logins: AtomicUsize::new(0),
            sends: AtomicUsize::new(0),
            status_calls: AtomicUsize::new(0),
            status_failures: AtomicUsize::new(0),
            send_plan: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            upgrades: AtomicUsize::new(0),
            upgrade_failures: AtomicUsize::new(0),
            reject_upgrade_as_unauthorized: AtomicBool::new(false),
            sessions_started: AtomicUsize::new(0),
            drop_first_session_after_caught_up: AtomicBool::new(false),
            snapshot_required_on_first_session: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            afters: Mutex::new(Vec::new()),
            calls: Mutex::new(Vec::new()),
            api_plan: Mutex::new(VecDeque::new()),
        })
    }

    fn record(&self, path: &str, headers: &HeaderMap) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        self.requests.lock().unwrap().push((path.to_owned(), auth));
    }

    fn message(&self, conversation: ConversationId, seq: i64, text: &str) -> Message {
        Message {
            id: MessageId::generate(),
            conversation_id: conversation,
            author_id: self.user.id,
            document: MessageDocument::from_plain_text(text).unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(seq).unwrap(),
        }
    }
}

/// A public channel the stub reports.
fn conversation(workspace: WorkspaceId, name: &str) -> Conversation {
    Conversation {
        id: ConversationId::generate(),
        workspace_id: workspace,
        kind: lazy_domain::ConversationKind::Channel,
        name: Some(ChannelName::parse(name).unwrap()),
        topic: None,
        description: None,
        visibility: ChannelVisibility::Public,
        archived: false,
        is_default: false,
        created_by: None,
        participants: Vec::new(),
        member_count: 1,
    }
}

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("Bearer tok-"))
}

fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(ApiError::new(ErrorCode::Unauthenticated, "sign in")),
    )
        .into_response()
}

async fn login(State(s): State<Arc<Stub>>, Json(_): Json<LoginRequest>) -> Json<SessionResponse> {
    let n = s.logins.fetch_add(1, Ordering::SeqCst) + 1;
    Json(SessionResponse {
        token: SessionToken::new(format!("tok-{n}")),
        user: s.user.clone(),
    })
}

async fn bootstrap(State(s): State<Arc<Stub>>, headers: HeaderMap) -> Response {
    s.record("/v1/bootstrap", &headers);
    if !bearer_ok(&headers) {
        return unauthenticated();
    }
    Json(BootstrapResponse {
        protocol_version: PROTOCOL_VERSION,
        user: s.user.clone(),
        workspaces: vec![WorkspaceBootstrap {
            workspace: Workspace {
                id: s.workspace,
                name: "Stub".into(),
            },
            role: WorkspaceRole::Member,
            conversations: vec![conversation(s.workspace, "general")],
            user_state: Vec::new(),
            members: vec![s.user.clone()],
            head: EventSeq::new(10).unwrap(),
        }],
    })
    .into_response()
}

async fn send_message(
    State(s): State<Arc<Stub>>,
    Path(conversation): Path<ConversationId>,
    headers: HeaderMap,
    Json(request): Json<SendMessageRequest>,
) -> Response {
    s.record("POST messages", &headers);
    s.sends.fetch_add(1, Ordering::SeqCst);
    let behavior = s
        .send_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(SendBehavior::Ok);
    match behavior {
        SendBehavior::Ok => {
            let mut message = s.message(conversation, 11, "ignored");
            message.document = request.document;
            Json(SendMessageResponse { message }).into_response()
        }
        SendBehavior::Conflict => (
            StatusCode::CONFLICT,
            Json(ApiError::new(ErrorCode::Conflict, "different payload")),
        )
            .into_response(),
        SendBehavior::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        SendBehavior::Malformed => (StatusCode::OK, "this is not json").into_response(),
    }
}

async fn operation_status(
    State(s): State<Arc<Stub>>,
    Path(_operation): Path<OperationId>,
    headers: HeaderMap,
) -> Response {
    s.record("GET operation", &headers);
    s.status_calls.fetch_add(1, Ordering::SeqCst);
    if s.status_failures.load(Ordering::SeqCst) > 0 {
        s.status_failures.fetch_sub(1, Ordering::SeqCst);
        return StatusCode::BAD_GATEWAY.into_response();
    }
    Json(OperationStatus::NotFound).into_response()
}

async fn realtime(
    State(s): State<Arc<Stub>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    s.record("GET realtime", &headers);
    let attempt = s.upgrades.fetch_add(1, Ordering::SeqCst);
    if !bearer_ok(&headers) || s.reject_upgrade_as_unauthorized.load(Ordering::SeqCst) {
        return unauthenticated();
    }
    if attempt < s.upgrade_failures.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    ws.on_upgrade(move |socket| serve_socket(s, socket))
}

struct ActiveGuard(Arc<Stub>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.active_sessions.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn send_frame(socket: &mut WebSocket, frame: &ServerFrame) -> bool {
    socket
        .send(WsMessage::Text(
            serde_json::to_string(frame).unwrap().into(),
        ))
        .await
        .is_ok()
}

async fn announce(
    stub: &Stub,
    socket: &mut WebSocket,
    workspace: WorkspaceId,
    after: EventSeq,
) -> bool {
    let seq = EventSeq::new(after.get() + 1).unwrap();
    let message = stub.message(ConversationId::generate(), seq.get(), "from the stub");
    let event = ServerFrame::Event(EventEnvelope {
        workspace_id: workspace,
        seq,
        occurred_at: Timestamp::now(),
        event: Event::MessageCreated {
            message,
            operation_id: None,
        },
    });
    send_frame(socket, &event).await
        && send_frame(
            socket,
            &ServerFrame::CaughtUp {
                workspace_id: workspace,
                seq,
            },
        )
        .await
}

async fn serve_socket(stub: Arc<Stub>, mut socket: WebSocket) {
    let index = stub.sessions_started.fetch_add(1, Ordering::SeqCst);
    stub.active_sessions.fetch_add(1, Ordering::SeqCst);
    let _guard = ActiveGuard(stub.clone());
    while let Some(Ok(message)) = socket.recv().await {
        match message {
            WsMessage::Text(text) => {
                let Ok(ClientFrame::Subscribe {
                    workspace_id,
                    after,
                }) = serde_json::from_str(text.as_str())
                else {
                    continue;
                };
                stub.afters.lock().unwrap().push(after.get());
                if index == 0
                    && stub
                        .snapshot_required_on_first_session
                        .load(Ordering::SeqCst)
                {
                    let frame = ServerFrame::SnapshotRequired {
                        workspace_id,
                        reason: lazy_protocol::SnapshotReason::CursorExpired,
                    };
                    send_frame(&mut socket, &frame).await;
                    return;
                }
                if !announce(&stub, &mut socket, workspace_id, after).await {
                    return;
                }
                if index == 0
                    && stub
                        .drop_first_session_after_caught_up
                        .load(Ordering::SeqCst)
                {
                    return; // closes the connection abruptly
                }
            }
            WsMessage::Close(_) => return,
            _ => {}
        }
    }
}

/// Stands in for every conversation-management route: records the request and answers with a
/// canned success, or with the failure the test asked for.
async fn conversation_api(
    State(stub): State<Arc<Stub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    stub.record(uri.path(), &headers);
    let call = Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    };
    stub.calls.lock().unwrap().push(call);
    let behavior = stub
        .api_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(ApiBehavior::Ok);
    let failure = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        ApiBehavior::NameTaken => return failure(StatusCode::CONFLICT, ErrorCode::NameTaken),
        ApiBehavior::Forbidden => return failure(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        ApiBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        ApiBehavior::Ok | ApiBehavior::OkWithJson => {}
    }
    let segments: Vec<&str> = uri.path().trim_matches('/').split('/').collect();
    let last = segments.last().copied().unwrap_or_default();
    let canned = || {
        Json(ConversationResponse {
            conversation: conversation(stub.workspace, "design"),
        })
    };
    match (method.as_str(), last) {
        ("GET", "browse") => Json(BrowseChannelsResponse {
            channels: vec![BrowsedChannel {
                conversation: conversation(stub.workspace, "design"),
                joined: false,
            }],
        })
        .into_response(),
        ("PUT", "favorite") | ("POST", "read") => Json(UserStateResponse {
            state: ConversationUserState {
                conversation_id: ConversationId::generate(),
                favorite: true,
                last_read: EventSeq::new(7).unwrap(),
                unread_count: 2,
                mention_count: 0,
            },
        })
        .into_response(),
        ("POST", "leave") | ("DELETE", _) => match behavior {
            ApiBehavior::OkWithJson => Json(serde_json::json!({})).into_response(),
            _ => StatusCode::NO_CONTENT.into_response(),
        },
        _ => canned().into_response(),
    }
}

fn router(stub: Arc<Stub>) -> Router {
    Router::new()
        .route("/v1/auth/login", post(login))
        .route("/v1/bootstrap", get(bootstrap))
        .route(
            "/v1/conversations/{conversation_id}/messages",
            post(send_message),
        )
        .route("/v1/operations/{operation_id}", get(operation_status))
        .route("/v1/realtime", get(realtime))
        .fallback(conversation_api)
        .with_state(stub)
}

// ----- Fixture -----

struct Server {
    /// Keeps the stub's runtime alive.
    _rt: tokio::runtime::Runtime,
    stub: Arc<Stub>,
    url: String,
}

impl Server {
    fn start() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let stub = Stub::new();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = router(stub.clone());
        rt.spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { _rt: rt, stub, url }
    }
}

fn fast_config(url: &str, profile: &TempDir, storage: Arc<dyn SecureStorage>) -> RuntimeConfig {
    RuntimeConfig {
        server_url: url.to_owned(),
        profile_dir: Some(profile.path().to_owned()),
        secure_storage: SecureStorageChoice::Custom(storage),
        request_timeout: Duration::from_secs(3),
        read_attempts: 3,
        reconnect: BackoffPolicy {
            base: Duration::from_millis(10),
            cap: Duration::from_millis(40),
        },
        idle_timeout: Duration::from_secs(5),
        save_debounce: Duration::from_millis(20),
    }
}

struct Client {
    runtime: Option<Runtime>,
    inputs: Receiver<Input>,
    profile: TempDir,
    storage: Arc<MemoryStorage>,
    url: String,
    /// Inputs seen while waiting for something else.
    skipped: Vec<Input>,
}

impl Client {
    fn start(server: &Server) -> (Self, Input) {
        Self::start_with(server, tempfile::tempdir().unwrap(), MemoryStorage::new())
    }

    fn start_with(server: &Server, profile: TempDir, storage: Arc<MemoryStorage>) -> (Self, Input) {
        let (runtime, inputs) = Runtime::start(fast_config(&server.url, &profile, storage.clone()))
            .expect("runtime starts");
        let mut client = Self {
            runtime: Some(runtime),
            inputs,
            profile,
            storage,
            url: server.url.clone(),
            skipped: Vec::new(),
        };
        let started = client.expect("Started", |i| matches!(i, Input::Started(_)));
        (client, started)
    }

    fn runtime(&self) -> &Runtime {
        self.runtime.as_ref().unwrap()
    }

    /// Shuts the client down and starts a new one on the same profile directory and credential
    /// store, as an app restart would.
    fn restart(mut self, server: &Server) -> (Self, Input) {
        self.shutdown();
        Self::start_with(server, self.profile, self.storage)
    }

    fn execute(&self, effects: Vec<Effect>) {
        self.runtime().execute(effects);
    }

    /// Waits for the next input matching `matches`, remembering the ones passed over.
    fn expect(&mut self, what: &str, matches: impl Fn(&Input) -> bool) -> Input {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let received = futures_block_on(async {
                tokio::time::timeout(remaining, self.inputs.recv()).await
            });
            match received {
                Ok(Ok(input)) if matches(&input) => return input,
                Ok(Ok(other)) => self.skipped.push(other),
                Ok(Err(_)) => panic!(
                    "input channel closed while waiting for {what}; skipped: {:?}",
                    self.skipped
                ),
                Err(_) => panic!("timed out waiting for {what}; skipped: {:?}", self.skipped),
            }
        }
    }

    /// Asserts that no further input arrives within `window`.
    fn expect_quiet(&mut self, window: Duration) {
        let received =
            futures_block_on(async { tokio::time::timeout(window, self.inputs.recv()).await });
        if let Ok(Ok(input)) = received {
            panic!("unexpected input: {input:?}");
        }
    }

    fn login(&mut self) {
        self.execute(vec![Effect::Login(LoginRequest {
            username: Username::parse("stubby").unwrap(),
            password: lazy_protocol::Password::new("password-1"),
        })]);
        self.expect("Authenticated", |i| {
            matches!(i, Input::Authenticated { .. })
        });
    }

    fn vault(&self) -> SecretVault {
        SecretVault::new(self.storage.clone(), self.profile.path(), &self.url)
    }

    fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }
}

/// Runs a future on a throwaway current-thread runtime, so tests can stay synchronous.
fn futures_block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn journal_entry() -> JournalEntry {
    JournalEntry {
        operation_id: OperationId::generate(),
        user_id: UserId::generate(),
        conversation_id: ConversationId::generate(),
        document: MessageDocument::from_plain_text("hello").unwrap(),
        submitted_at: Timestamp::now(),
    }
}

fn send_effect(operation: OperationId) -> Effect {
    Effect::SendMessage {
        operation,
        conversation: ConversationId::generate(),
        document: MessageDocument::from_plain_text("hi").unwrap(),
    }
}

// ----- Tests -----

#[test]
fn an_invalid_server_url_is_refused() {
    let profile = tempfile::tempdir().unwrap();
    let config = fast_config("ftp://nope", &profile, MemoryStorage::new());
    assert!(matches!(
        Runtime::start(config),
        Err(lazy_client_runtime::RuntimeError::InvalidServerUrl(_))
    ));
}

#[test]
fn the_server_url_defaults_from_configuration() {
    // The default is whatever LAZY_SERVER_URL says, or the documented fallback.
    let config = RuntimeConfig::default();
    assert!(config.server_url.starts_with("http"));
    assert_eq!(
        RuntimeConfig::new("http://example.test:9").server_url,
        "http://example.test:9"
    );
}

#[test]
fn started_is_first_and_a_fresh_profile_has_no_session() {
    let server = Server::start();
    let (mut client, started) = Client::start(&server);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert!(!state.session_restored);
    assert!(state.journal.is_empty() && state.drafts.is_empty() && state.problems.is_empty());
    assert!(client.skipped.is_empty(), "nothing precedes Started");
    client.shutdown();
}

#[test]
fn login_saves_the_session_under_this_profiles_namespace_and_later_requests_carry_it() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();

    let namespace = client.vault().namespace().to_owned();
    assert!(
        client
            .storage
            .keys()
            .contains(&format!("session-token:{namespace}"))
    );

    client.execute(vec![Effect::LoadBootstrap]);
    let Input::BootstrapLoaded(bootstrap) =
        client.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)))
    else {
        unreachable!()
    };
    assert_eq!(bootstrap.workspaces.len(), 1);
    let requests = server.stub.requests.lock().unwrap().clone();
    assert_eq!(
        requests,
        vec![("/v1/bootstrap".to_owned(), Some("Bearer tok-1".to_owned()))]
    );
    client.shutdown();
}

#[test]
fn a_restored_session_is_reported_and_two_profiles_do_not_share_one() {
    let server = Server::start();
    let storage = MemoryStorage::new();
    let (mut first, _) = Client::start_with(&server, tempfile::tempdir().unwrap(), storage.clone());
    first.login();

    // A second client with its own profile directory, sharing the same credential store, is not
    // signed in.
    let (mut other, started) =
        Client::start_with(&server, tempfile::tempdir().unwrap(), storage.clone());
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert!(!state.session_restored, "profiles must not share sessions");
    other.shutdown();

    // The first profile, restarted, finds its own session again.
    let (mut again, started) = first.restart(&server);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert!(state.session_restored);
    again.shutdown();
}

#[test]
fn requests_without_a_session_are_definitively_rejected() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.execute(vec![Effect::LoadBootstrap]);
    let Input::BootstrapFailed(error) =
        client.expect("failure", |i| matches!(i, Input::BootstrapFailed(_)))
    else {
        unreachable!()
    };
    assert!(matches!(error, RequestError::Rejected(e) if e.code == ErrorCode::Unauthenticated));
    client.shutdown();
}

#[test]
fn send_failures_are_classified_by_whether_the_outcome_is_known() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    server.stub.send_plan.lock().unwrap().extend([
        SendBehavior::Conflict,
        SendBehavior::ServerError,
        SendBehavior::Malformed,
        SendBehavior::Ok,
    ]);

    let mut outcomes = Vec::new();
    for _ in 0..4 {
        let operation = OperationId::generate();
        client.execute(vec![send_effect(operation)]);
        let Input::SendResult { result, .. } = client.expect(
            "a send result",
            |i| matches!(i, Input::SendResult { operation: o, .. } if *o == operation),
        ) else {
            unreachable!()
        };
        outcomes.push(result);
    }
    assert!(
        matches!(&outcomes[0], Err(RequestError::Rejected(e)) if e.code == ErrorCode::Conflict)
    );
    assert!(
        matches!(&outcomes[1], Err(RequestError::Unknown(_))),
        "a 5xx leaves the outcome unknown"
    );
    assert!(
        matches!(&outcomes[2], Err(RequestError::Unknown(_))),
        "a 2xx we cannot parse is unknown too"
    );
    assert!(outcomes[3].is_ok());
    assert_eq!(
        server.stub.sends.load(Ordering::SeqCst),
        4,
        "each send is attempted exactly once"
    );
    client.shutdown();
}

#[test]
fn a_dead_server_is_unknown_not_rejected() {
    let server = Server::start();
    let dead_url = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (runtime, inputs) =
        Runtime::start(fast_config(&dead_url, &profile, storage.clone())).unwrap();
    let mut client = Client {
        runtime: Some(runtime),
        inputs,
        profile,
        storage,
        url: dead_url,
        skipped: Vec::new(),
    };
    client.expect("Started", |i| matches!(i, Input::Started(_)));
    let operation = OperationId::generate();
    client.execute(vec![send_effect(operation)]);
    let Input::SendResult { result, .. } =
        client.expect("send result", |i| matches!(i, Input::SendResult { .. }))
    else {
        unreachable!()
    };
    assert!(matches!(result, Err(RequestError::Unknown(_))));
    client.shutdown();
    drop(server);
}

#[test]
fn reads_are_retried_but_sends_are_not() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();

    server.stub.status_failures.store(2, Ordering::SeqCst);
    let operation = OperationId::generate();
    client.execute(vec![Effect::QueryOperationStatus { operation }]);
    let Input::OperationStatusResult { result, .. } = client.expect("status", |i| {
        matches!(i, Input::OperationStatusResult { .. })
    }) else {
        unreachable!()
    };
    assert!(matches!(result, Ok(OperationStatus::NotFound)));
    assert_eq!(
        server.stub.status_calls.load(Ordering::SeqCst),
        3,
        "two failures then a success"
    );

    // When retries run out, the caller is told the outcome is unknown.
    server.stub.status_failures.store(100, Ordering::SeqCst);
    let before = server.stub.status_calls.load(Ordering::SeqCst);
    client.execute(vec![Effect::QueryOperationStatus { operation }]);
    let Input::OperationStatusResult { result, .. } = client.expect("status", |i| {
        matches!(i, Input::OperationStatusResult { .. })
    }) else {
        unreachable!()
    };
    assert!(matches!(result, Err(RequestError::Unknown(_))));
    assert_eq!(
        server.stub.status_calls.load(Ordering::SeqCst) - before,
        3,
        "bounded by read_attempts"
    );

    server
        .stub
        .send_plan
        .lock()
        .unwrap()
        .push_back(SendBehavior::ServerError);
    let sends_before = server.stub.sends.load(Ordering::SeqCst);
    client.execute(vec![send_effect(OperationId::generate())]);
    client.expect("send result", |i| matches!(i, Input::SendResult { .. }));
    assert_eq!(server.stub.sends.load(Ordering::SeqCst) - sends_before, 1);
    client.shutdown();
}

#[test]
fn realtime_forwards_frames_and_resumes_from_the_last_position_after_a_drop() {
    let server = Server::start();
    server
        .stub
        .drop_first_session_after_caught_up
        .store(true, Ordering::SeqCst);
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    let sockets = |i: &Input, wanted: SocketEvent| matches!(i, Input::Socket(e) if *e == wanted);
    client.expect("first connect", |i| {
        sockets(
            i,
            SocketEvent::Connecting {
                workspace,
                attempt: 0,
            },
        )
    });
    client.expect("opened", |i| sockets(i, SocketEvent::Opened { workspace }));
    let Input::Frame(ServerFrame::Event(first)) = client.expect("first event", |i| {
        matches!(i, Input::Frame(ServerFrame::Event(_)))
    }) else {
        unreachable!()
    };
    assert_eq!(first.seq.get(), 11);
    client.expect("caught up", |i| {
        matches!(i, Input::Frame(ServerFrame::CaughtUp { .. }))
    });

    // The stub drops the connection; the runtime reconnects and resumes after position 11.
    client.expect("closed", |i| {
        sockets(
            i,
            SocketEvent::Closed {
                workspace,
                attempt: 1,
            },
        )
    });
    client.expect("reconnecting", |i| {
        sockets(
            i,
            SocketEvent::Connecting {
                workspace,
                attempt: 1,
            },
        )
    });
    client.expect("reopened", |i| {
        sockets(i, SocketEvent::Opened { workspace })
    });
    let Input::Frame(ServerFrame::Event(second)) = client.expect("second event", |i| {
        matches!(i, Input::Frame(ServerFrame::Event(_)))
    }) else {
        unreachable!()
    };
    assert_eq!(second.seq.get(), 12);
    assert_eq!(*server.stub.afters.lock().unwrap(), vec![10, 11]);

    let upgrades = server
        .stub
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p == "GET realtime")
        .count();
    assert_eq!(upgrades, 2);
    assert!(
        server
            .stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, auth)| auth.as_deref() == Some("Bearer tok-1"))
    );
    client.shutdown();
}

#[test]
fn a_new_subscribe_effect_replaces_the_position_on_the_open_connection() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    client.expect("caught up", |i| {
        matches!(i, Input::Frame(ServerFrame::CaughtUp { .. }))
    });
    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(40).unwrap(),
    }]);
    let Input::Frame(ServerFrame::Event(event)) = client.expect(
        "event after resubscribe",
        |i| matches!(i, Input::Frame(ServerFrame::Event(e)) if e.seq.get() == 41),
    ) else {
        unreachable!()
    };
    assert_eq!(event.seq.get(), 41);
    assert_eq!(
        server.stub.sessions_started.load(Ordering::SeqCst),
        1,
        "same connection"
    );
    client.shutdown();
}

#[test]
fn after_snapshot_required_the_runtime_waits_for_a_fresh_position_instead_of_reconnecting() {
    let server = Server::start();
    server
        .stub
        .snapshot_required_on_first_session
        .store(true, Ordering::SeqCst);
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    client.expect("snapshot required", |i| {
        matches!(i, Input::Frame(ServerFrame::SnapshotRequired { .. }))
    });
    client.expect("closed", |i| {
        matches!(i, Input::Socket(SocketEvent::Closed { .. }))
    });

    // The refused position must not be offered again. Well past the reconnect backoff, the
    // runtime has still not reconnected.
    client.expect_quiet(Duration::from_millis(500));
    assert_eq!(server.stub.upgrades.load(Ordering::SeqCst), 1);

    // The core bootstraps and subscribes from the new position; only then does it reconnect.
    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(40).unwrap(),
    }]);
    client.expect(
        "event after the fresh position",
        |i| matches!(i, Input::Frame(ServerFrame::Event(e)) if e.seq.get() == 41),
    );
    assert_eq!(*server.stub.afters.lock().unwrap(), vec![10, 40]);
    client.shutdown();
}

#[test]
fn reconnect_attempts_are_counted_and_the_delay_stays_bounded() {
    let server = Server::start();
    server.stub.upgrade_failures.store(4, Ordering::SeqCst);
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    let started = Instant::now();
    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::ZERO,
    }]);
    for attempt in 1..=4 {
        client.expect(
            "closed",
            |i| matches!(i, Input::Socket(SocketEvent::Closed { attempt: a, .. }) if *a == attempt),
        );
    }
    client.expect("opened eventually", |i| {
        matches!(i, Input::Socket(SocketEvent::Opened { .. }))
    });
    // Four refusals with a 40 ms cap cannot take anywhere near this long.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    client.shutdown();
}

#[test]
fn an_unauthorized_upgrade_reports_session_expiry_and_stops_retrying() {
    let server = Server::start();
    server
        .stub
        .reject_upgrade_as_unauthorized
        .store(true, Ordering::SeqCst);
    let (mut client, _) = Client::start(&server);
    client.login();
    client.execute(vec![Effect::Subscribe {
        workspace: server.stub.workspace,
        after: EventSeq::ZERO,
    }]);
    client.expect("session expiry", |i| matches!(i, Input::SessionExpired));
    client.expect_quiet(Duration::from_millis(400));
    assert_eq!(
        server.stub.upgrades.load(Ordering::SeqCst),
        1,
        "no reconnect storm"
    );
    client.shutdown();
}

#[test]
fn shutdown_closes_connections_and_the_input_channel_without_leaking_tasks() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    client.execute(vec![Effect::Subscribe {
        workspace: server.stub.workspace,
        after: EventSeq::ZERO,
    }]);
    client.expect("caught up", |i| {
        matches!(i, Input::Frame(ServerFrame::CaughtUp { .. }))
    });
    assert_eq!(server.stub.active_sessions.load(Ordering::SeqCst), 1);

    let started = Instant::now();
    client.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "shutdown is prompt: {:?}",
        started.elapsed()
    );
    wait_until("the stub to see the socket close", || {
        server.stub.active_sessions.load(Ordering::SeqCst) == 0
    });

    // Everything that held a sender is gone, so the channel drains and then reports closed.
    let inputs = client.inputs.clone();
    let closed = futures_block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while inputs.recv().await.is_ok() {}
        })
        .await
    });
    assert!(
        closed.is_ok(),
        "the input channel must close after shutdown"
    );
}

#[test]
fn a_submission_is_acknowledged_only_after_it_is_durable() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    let entry = journal_entry();
    client.execute(vec![Effect::PersistSubmission(entry.clone())]);
    client.expect("persisted", |i| matches!(i, Input::SubmissionPersisted { operation } if *operation == entry.operation_id));

    // The acknowledgment has arrived, so the data must already be readable from disk.
    let key = client.vault().state_key().unwrap();
    let on_disk = inspect_state(client.profile.path(), &key);
    assert_eq!(on_disk.journal, vec![entry.clone()]);

    client.execute(vec![Effect::SettleSubmission {
        operation: entry.operation_id,
    }]);
    client.shutdown();
    assert!(
        inspect_state(client.profile.path(), &key)
            .journal
            .is_empty()
    );
}

/// A cache with one conversation of `messages` messages, current to `cursor`.
fn sample_cache(cursor: i64, messages: usize) -> ContentCache {
    let me = User {
        id: UserId::generate(),
        username: Username::parse("cached").unwrap(),
        display_name: DisplayName::parse("Cached").unwrap(),
    };
    let workspace = Workspace {
        id: WorkspaceId::generate(),
        name: "Cached workspace".into(),
    };
    let conversation = conversation(workspace.id, "general");
    let timeline = CachedTimeline {
        conversation: conversation.id,
        messages: (0..messages)
            .map(|index| Message {
                id: MessageId::generate(),
                conversation_id: conversation.id,
                author_id: me.id,
                document: MessageDocument::from_plain_text(&format!("secret message {index}"))
                    .unwrap(),
                created_at: Timestamp::now(),
                revision: 1,
                seq: EventSeq::new(index as i64 + 1).unwrap(),
            })
            .collect(),
        has_more: false,
    };
    ContentCache {
        user: me.clone(),
        workspaces: vec![CachedWorkspace {
            workspace,
            role: WorkspaceRole::Member,
            conversations: vec![conversation],
            members: vec![me],
            cursor: EventSeq::new(cursor).unwrap(),
            timelines: vec![timeline],
            user_state: Vec::new(),
        }],
    }
}

fn cache_file(client: &Client) -> std::path::PathBuf {
    client.profile.path().join("cache.enc")
}

#[test]
fn drafts_the_cache_and_the_journal_survive_a_restart() {
    let server = Server::start();
    let (mut first, _) = Client::start(&server);

    let entry = journal_entry();
    let conversation = ConversationId::generate();
    let cache = sample_cache(7, 3);
    first.execute(vec![
        Effect::PersistSubmission(entry.clone()),
        Effect::PersistDrafts(vec![(conversation, "half-written".into())]),
        Effect::PersistCache(Box::new(cache.clone())),
    ]);
    first.expect("persisted", |i| {
        matches!(i, Input::SubmissionPersisted { .. })
    });

    // Restarting flushes the debounced writes and reloads everything.
    let (mut second, started) = first.restart(&server);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert_eq!(state.journal, vec![entry]);
    assert_eq!(state.drafts[&conversation], "half-written");
    assert_eq!(state.cache, Some(cache));
    assert!(state.problems.is_empty());
    second.shutdown();
}

#[test]
fn a_burst_of_cache_changes_is_written_as_one_whole_latest_cache() {
    let server = Server::start();
    let (first, _) = Client::start(&server);
    let effects: Vec<_> = (1..=50)
        .map(|cursor| Effect::PersistCache(Box::new(sample_cache(cursor, cursor as usize))))
        .collect();
    let last = sample_cache(50, 50);
    // Each effect supersedes the previous one; a distinct user per cache would make mixing
    // visible, so check the winner is exactly the last one, cursor and messages together.
    first.execute(effects);
    let (mut second, started) = first.restart(&server);
    let Input::Started(state) = started else {
        unreachable!()
    };
    let cache = state
        .cache
        .expect("the latest cache was flushed on shutdown");
    assert_eq!(cache.workspaces[0].cursor.get(), 50);
    assert_eq!(cache.workspaces[0].timelines[0].messages.len(), 50);
    assert_eq!(
        cache.workspaces[0].timelines[0].messages.len(),
        last.message_count()
    );
    second.shutdown();
}

#[test]
fn the_cache_file_is_encrypted_and_private() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.execute(vec![Effect::PersistCache(Box::new(sample_cache(5, 2)))]);
    wait_until("the cache file", || cache_file(&client).exists());
    let raw = std::fs::read(cache_file(&client)).unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("secret message"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(cache_file(&client))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0);
    }
    client.shutdown();
}

#[test]
fn discarding_deletes_the_cache_and_drops_a_write_still_waiting() {
    let server = Server::start();
    let (client, _) = Client::start(&server);
    client.execute(vec![Effect::PersistCache(Box::new(sample_cache(5, 2)))]);
    wait_until("the cache file", || cache_file(&client).exists());

    client.execute(vec![Effect::DiscardCache]);
    wait_until("the cache file to be deleted", || {
        !cache_file(&client).exists()
    });

    // A write that has not happened yet when the discard arrives never happens.
    client.execute(vec![
        Effect::PersistCache(Box::new(sample_cache(9, 4))),
        Effect::DiscardCache,
    ]);
    let (mut again, started) = client.restart(&server);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert!(
        state.cache.is_none(),
        "the discarded cache must not come back"
    );
    again.shutdown();
}

#[test]
fn signing_out_deletes_the_cache_with_everything_else() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    client.execute(vec![Effect::PersistCache(Box::new(sample_cache(5, 2)))]);
    wait_until("the cache file", || cache_file(&client).exists());
    client.execute(vec![Effect::SignOut]);
    wait_until("the cache to be wiped", || !cache_file(&client).exists());
    client.shutdown();
}

#[test]
fn a_corrupt_cache_is_reported_in_started_and_does_not_block_startup() {
    let server = Server::start();
    let (mut first, _) = Client::start(&server);
    first.execute(vec![Effect::PersistCache(Box::new(sample_cache(5, 2)))]);
    wait_until("the cache file", || cache_file(&first).exists());
    let path = cache_file(&first);
    first.shutdown();
    let bytes = std::fs::read(&path).unwrap();
    std::fs::write(&path, &bytes[..bytes.len() / 3]).unwrap();

    let (mut second, started) = Client::start_with(&server, first.profile, first.storage);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert!(state.cache.is_none(), "an unreadable cache is discarded");
    assert!(
        state.problems.iter().any(|p| p.contains("content cache")),
        "{:?}",
        state.problems
    );
    second.shutdown();
}

#[test]
fn signing_out_wipes_this_profile_and_leaves_another_profiles_state_and_secrets_alone() {
    let server = Server::start();
    let storage = MemoryStorage::new();
    let (mut a, _) = Client::start_with(&server, tempfile::tempdir().unwrap(), storage.clone());
    let (mut b, _) = Client::start_with(&server, tempfile::tempdir().unwrap(), storage.clone());
    a.login();
    b.login();
    let entry_a = journal_entry();
    let entry_b = journal_entry();
    a.execute(vec![Effect::PersistSubmission(entry_a)]);
    b.execute(vec![Effect::PersistSubmission(entry_b.clone())]);
    a.expect("a persisted", |i| {
        matches!(i, Input::SubmissionPersisted { .. })
    });
    b.expect("b persisted", |i| {
        matches!(i, Input::SubmissionPersisted { .. })
    });
    let key_b = b.vault().state_key().unwrap();
    let b_namespace = b.vault().namespace().to_owned();
    assert_eq!(
        storage.keys().len(),
        4,
        "a session token and a state key per profile"
    );

    a.execute(vec![Effect::SignOut]);
    let a_dir = a.profile.path().to_owned();
    wait_until("profile A to be wiped", || {
        std::fs::read_dir(&a_dir).map_or(true, |d| {
            d.filter_map(Result::ok)
                .all(|e| !e.file_name().to_string_lossy().contains(".enc"))
        })
    });
    wait_until("A's secrets to be deleted", || storage.keys().len() == 2);

    assert!(
        storage.keys().iter().all(|k| k.ends_with(&b_namespace)),
        "only B's secrets remain: {:?}",
        storage.keys()
    );
    assert_eq!(
        inspect_state(b.profile.path(), &key_b).journal,
        vec![entry_b]
    );
    a.shutdown();
    b.shutdown();
}

struct BrokenStorage;

impl SecureStorage for BrokenStorage {
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::Unavailable("no credential store".into()))
    }
    fn set(&self, _: &str, _: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::Unavailable("no credential store".into()))
    }
    fn delete(&self, _: &str) -> Result<(), StorageError> {
        Err(StorageError::Unavailable("no credential store".into()))
    }
}

#[test]
fn unavailable_secure_storage_is_reported_and_nothing_is_silently_unprotected() {
    let server = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let (runtime, inputs) = Runtime::start(RuntimeConfig {
        secure_storage: SecureStorageChoice::Custom(Arc::new(BrokenStorage)),
        ..fast_config(&server.url, &profile, MemoryStorage::new())
    })
    .unwrap();
    let mut client = Client {
        runtime: Some(runtime),
        inputs,
        profile,
        storage: MemoryStorage::new(),
        url: server.url.clone(),
        skipped: Vec::new(),
    };
    let Input::Started(state) = client.expect("Started", |i| matches!(i, Input::Started(_))) else {
        unreachable!()
    };
    assert!(!state.problems.is_empty());
    assert!(!state.session_restored);

    // Without a key there is no encrypted journal, so a send must not be allowed to proceed.
    let entry = journal_entry();
    client.execute(vec![Effect::PersistSubmission(entry.clone())]);
    let Input::SubmissionPersistFailed { operation, .. } = client.expect("failure", |i| {
        matches!(i, Input::SubmissionPersistFailed { .. })
    }) else {
        unreachable!()
    };
    assert_eq!(operation, entry.operation_id);
    let files: Vec<_> = std::fs::read_dir(client.profile.path()).unwrap().collect();
    assert!(files.is_empty(), "nothing was written unencrypted");
    client.shutdown();
}

// ----- Conversation management -----

fn last_call(server: &Server) -> Call {
    server
        .stub
        .calls
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("the stub saw a request")
}

fn call_count(server: &Server) -> usize {
    server.stub.calls.lock().unwrap().len()
}

/// Runs one effect and returns the input it produced and the request the stub saw.
fn run_effect(
    client: &mut Client,
    server: &Server,
    effect: Effect,
    matches: impl Fn(&Input) -> bool,
) -> (Input, Call) {
    client.execute(vec![effect]);
    let input = client.expect("the effect's result", matches);
    (input, last_call(server))
}

fn action_result(kind: ActionKind) -> impl Fn(&Input) -> bool {
    move |input| matches!(input, Input::ActionResult { kind: k, .. } if *k == kind)
}

#[test]
fn every_conversation_request_uses_its_documented_route_method_and_body() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let conversation = ConversationId::generate();
    let user = UserId::generate();
    let ok = |input: &Input| -> bool {
        match input {
            Input::ActionResult { result, .. } => result.is_ok(),
            Input::ConversationCommandResult { result, .. } => result.is_ok(),
            Input::MarkReadResult { result, .. } => result.is_ok(),
            _ => false,
        }
    };

    // Create a channel.
    let operation = OperationId::generate();
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::CreateChannel {
            workspace,
            request: CreateChannelRequest {
                operation_id: operation,
                name: ChannelName::parse("design").unwrap(),
                visibility: ChannelVisibility::Private,
                topic: Some("pixels".into()),
                description: None,
            },
        },
        |i| matches!(i, Input::ConversationCommandResult { .. }),
    );
    assert!(ok(&input));
    assert!(
        matches!(&input, Input::ConversationCommandResult { operation: o, .. } if *o == operation)
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/workspaces/{workspace}/channels").as_str()
        )
    );
    assert_eq!(call.body["operation_id"], operation.to_string());
    assert_eq!(call.body["name"], "design");
    assert_eq!(call.body["visibility"], "private");
    assert_eq!(call.body["topic"], "pixels");
    assert!(call.body["description"].is_null());

    // Open a conversation.
    let operation = OperationId::generate();
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::OpenConversation {
            workspace,
            request: OpenConversationRequest {
                operation_id: operation,
                participants: vec![user],
            },
        },
        |i| matches!(i, Input::ConversationCommandResult { .. }),
    );
    assert!(ok(&input));
    assert_eq!(call.method, "POST");
    assert_eq!(
        call.path,
        format!("/v1/workspaces/{workspace}/conversations")
    );
    assert_eq!(
        call.body["participants"],
        serde_json::json!([user.to_string()])
    );

    // Browse.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::BrowseChannels { workspace },
        action_result(ActionKind::Browse),
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::Browse(channels)),
        ..
    } = input
    else {
        panic!("expected the directory");
    };
    assert_eq!(channels.len(), 1);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "GET",
            format!("/v1/workspaces/{workspace}/channels/browse").as_str()
        )
    );

    // Join, archive, unarchive: POST with no body, answered with the conversation.
    for (effect, kind, action) in [
        (
            Effect::JoinChannel { conversation },
            ActionKind::Join(conversation),
            "join",
        ),
        (
            Effect::ArchiveConversation { conversation },
            ActionKind::Archive(conversation),
            "archive",
        ),
        (
            Effect::UnarchiveConversation { conversation },
            ActionKind::Unarchive(conversation),
            "unarchive",
        ),
    ] {
        let (input, call) = run_effect(&mut client, &server, effect, action_result(kind));
        assert!(
            matches!(
                input,
                Input::ActionResult {
                    result: Ok(ActionOutcome::Conversation(_)),
                    ..
                }
            ),
            "{action}"
        );
        assert_eq!(call.method, "POST", "{action}");
        assert_eq!(
            call.path,
            format!("/v1/conversations/{conversation}/{action}")
        );
        assert!(call.body.is_null(), "{action} sends no body");
    }

    // Leave: POST, answered with no content.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LeaveChannel { conversation },
        action_result(ActionKind::Leave(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Done),
            ..
        }
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/conversations/{conversation}/leave").as_str()
        )
    );

    // Add members.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::AddMembers {
            conversation,
            request: AddMembersRequest {
                user_ids: vec![user],
            },
        },
        action_result(ActionKind::AddMembers(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Conversation(_)),
            ..
        }
    ));
    assert_eq!(
        call.path,
        format!("/v1/conversations/{conversation}/members")
    );
    assert_eq!(call.body["user_ids"], serde_json::json!([user.to_string()]));

    // Remove a member: DELETE.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::RemoveMember { conversation, user },
        action_result(ActionKind::RemoveMember(conversation, user)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Done),
            ..
        }
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "DELETE",
            format!("/v1/conversations/{conversation}/members/{user}").as_str()
        )
    );

    // Update: PATCH, an empty string clears, an absent field is left alone.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::UpdateConversation {
            conversation,
            request: UpdateConversationRequest {
                name: Some(ChannelName::parse("renamed").unwrap()),
                topic: Some(String::new()),
                description: None,
            },
        },
        action_result(ActionKind::UpdateConversation(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Conversation(_)),
            ..
        }
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PATCH",
            format!("/v1/conversations/{conversation}").as_str()
        )
    );
    assert_eq!(call.body["name"], "renamed");
    assert_eq!(call.body["topic"], "");
    assert!(call.body["description"].is_null());

    // Favorite: PUT.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetFavorite {
            conversation,
            favorite: true,
        },
        action_result(ActionKind::SetFavorite(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::UserState(_)),
            ..
        }
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PUT",
            format!("/v1/conversations/{conversation}/favorite").as_str()
        )
    );
    assert_eq!(call.body, serde_json::json!({"favorite": true}));

    // Mark read: POST with the position.
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::MarkRead {
            conversation,
            up_to: EventSeq::new(7).unwrap(),
        },
        |i| matches!(i, Input::MarkReadResult { .. }),
    );
    let Input::MarkReadResult {
        conversation: c,
        up_to,
        result: Ok(state),
    } = input
    else {
        panic!("expected the resulting state");
    };
    assert_eq!(
        (c, up_to.get(), state.last_read.get()),
        (conversation, 7, 7)
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/conversations/{conversation}/read").as_str()
        )
    );
    assert_eq!(call.body, serde_json::json!({"up_to": 7}));

    // Every request carried the session.
    assert!(
        server
            .stub
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with("/v1/workspaces")
                || path.starts_with("/v1/conversations"))
            .all(|(_, auth)| auth.as_deref() == Some("Bearer tok-1"))
    );
    client.shutdown();
}

#[test]
fn conversation_failures_are_classified_and_mutations_are_never_repeated() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let conversation = ConversationId::generate();
    let create = |name: &str| Effect::CreateChannel {
        workspace,
        request: CreateChannelRequest {
            operation_id: OperationId::generate(),
            name: ChannelName::parse(name).unwrap(),
            visibility: ChannelVisibility::Public,
            topic: None,
            description: None,
        },
    };
    let command_result = |i: &Input| matches!(i, Input::ConversationCommandResult { .. });

    // A name already in use is a definitive refusal, naming the code the core turns into words.
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::NameTaken);
    let before = call_count(&server);
    let (input, _) = run_effect(&mut client, &server, create("taken"), command_result);
    let Input::ConversationCommandResult {
        result: Err(RequestError::Rejected(error)),
        ..
    } = input
    else {
        panic!("expected a rejection");
    };
    assert_eq!(error.code, ErrorCode::NameTaken);

    // A server failure leaves the outcome of a create unknown, and is not tried again by the
    // runtime: the core asks the server instead.
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::ServerError);
    let before_failure = call_count(&server);
    let (input, _) = run_effect(&mut client, &server, create("flaky"), command_result);
    assert!(matches!(
        input,
        Input::ConversationCommandResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(
        call_count(&server),
        before_failure + 1,
        "exactly one attempt"
    );
    assert!(call_count(&server) > before);

    // Management mutations are classified the same way and also attempted once.
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::Forbidden);
    let before_join = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::JoinChannel { conversation },
        action_result(ActionKind::Join(conversation)),
    );
    assert!(
        matches!(input, Input::ActionResult { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::Forbidden)
    );
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::ServerError);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::ArchiveConversation { conversation },
        action_result(ActionKind::Archive(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before_join + 2, "one attempt each");

    // Reading the directory is safe to repeat, so a failure is retried.
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .extend([ApiBehavior::ServerError, ApiBehavior::ServerError]);
    let before_browse = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::BrowseChannels { workspace },
        action_result(ActionKind::Browse),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Browse(_)),
            ..
        }
    ));
    assert_eq!(
        call_count(&server),
        before_browse + 3,
        "two failures, then success"
    );

    // Leaving and removing succeed whatever the body, as long as the status is a success.
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::OkWithJson);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::LeaveChannel { conversation },
        action_result(ActionKind::Leave(conversation)),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Done),
            ..
        }
    ));
    client.shutdown();
}

#[test]
fn mark_read_failures_are_reported_for_the_core_to_handle() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let conversation = ConversationId::generate();
    server
        .stub
        .api_plan
        .lock()
        .unwrap()
        .push_back(ApiBehavior::ServerError);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::MarkRead {
            conversation,
            up_to: EventSeq::new(3).unwrap(),
        },
        |i| matches!(i, Input::MarkReadResult { .. }),
    );
    assert!(matches!(
        input,
        Input::MarkReadResult { up_to, result: Err(RequestError::Unknown(_)), .. } if up_to.get() == 3
    ));
    client.shutdown();
}
