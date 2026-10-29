//! Files through the runtime (slice 3A, D17): copies in the outbox, uploads, downloads, and image
//! previews, against an in-process stub of the server's attachment routes.

use std::collections::{HashMap, VecDeque};
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use futures_util::StreamExt;
use lazy_client_core::{ActionKind, ActionOutcome};
use lazy_client_core::{
    Effect, FileBytes, FileSource, ImageData, Input, JournalAttachment, JournalEntry, JournalKind,
    RequestError, TransferError,
};
use lazy_client_runtime::{
    BackoffPolicy, MemoryStorage, Runtime, RuntimeConfig, SecretVault, SecureStorage,
    SecureStorageChoice, async_channel::Receiver, inspect_state,
};
use lazy_domain::{
    AttachmentId, ConversationId, DisplayName, MessageDocument, OperationId, Timestamp, UserId,
    Username, WorkspaceId,
};
use lazy_domain::{EmojiId, EmojiName};
use lazy_protocol::{
    ApiError, Attachment, AttachmentResponse, ErrorCode, LoginRequest, SessionResponse,
    SessionToken, User,
};
use lazy_protocol::{CustomEmoji, CustomEmojiResponse, RenameEmojiRequest};
use lazy_protocol::{
    SearchHit, SearchHitKind, SearchParams, SearchResponse, SearchSort, Snippet, SnippetRange,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

// ----- Stub server -----

#[derive(Clone, Copy, Debug)]
enum UploadBehavior {
    /// Read the whole body and answer with the attachment.
    Ok,
    /// Answer at once with this error, reading nothing.
    Refuse(ErrorCode),
    /// Read the whole body, then fail with a server error.
    ServerErrorAfterBody,
    /// Read this many bytes, then fail with a server error.
    ServerErrorAfter(usize),
    /// Read the body slowly: this long after each chunk.
    Slow(Duration),
}

#[derive(Clone, Copy, Debug)]
enum DownloadBehavior {
    Ok,
    /// Serve other bytes of the right length.
    Corrupt,
    /// Serve slowly, so a download can be cancelled.
    Slow(Duration),
    ServerError,
}

#[derive(Clone, Debug)]
struct Upload {
    conversation: ConversationId,
    attachment: AttachmentId,
    name_header: Option<String>,
    content_type: Option<String>,
    content_length: Option<u64>,
    authorized: bool,
    /// Bytes read before the handler finished or the client went away.
    received: usize,
    sha256: String,
    /// The client hung up before the body was complete.
    aborted: bool,
}

/// What the custom emoji routes of the stub do with the next request.
#[derive(Clone, Copy, Debug)]
enum EmojiBehavior {
    Ok,
    Refuse(ErrorCode),
    ServerError,
}

#[derive(Clone, Debug)]
struct EmojiUpload {
    workspace: WorkspaceId,
    emoji: EmojiId,
    name_header: Option<String>,
    content_type: Option<String>,
    authorized: bool,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct EmojiStub {
    uploads: Mutex<Vec<EmojiUpload>>,
    /// What the next upload, rename, or delete does.
    plan: Mutex<VecDeque<EmojiBehavior>>,
    /// What the next image fetch does.
    image_plan: Mutex<VecDeque<EmojiBehavior>>,
    images: Mutex<HashMap<EmojiId, Vec<u8>>>,
    image_requests: AtomicUsize,
    /// Every rename and delete: the method, the emoji, and the body.
    calls: Mutex<Vec<(String, EmojiId, serde_json::Value)>>,
}

/// What the search route of the stub does with the next request.
#[derive(Clone, Copy, Debug)]
enum SearchBehavior {
    Ok,
    Refuse(ErrorCode),
    /// Answer with a server failure.
    ServerError,
    /// Answer after this long (or never, if the client hangs up first).
    Slow(Duration),
}

#[derive(Clone, Debug)]
struct SearchCall {
    workspace: WorkspaceId,
    /// The query string exactly as it was sent.
    raw_query: String,
    /// The query string as the server reads it.
    params: HashMap<String, String>,
    authorized: bool,
}

#[derive(Default)]
struct SearchStub {
    calls: Mutex<Vec<SearchCall>>,
    plan: Mutex<VecDeque<SearchBehavior>>,
    /// Requests whose client hung up before they were answered.
    aborted: AtomicUsize,
}

struct Stub {
    emoji: EmojiStub,
    search: SearchStub,
    user: User,
    uploads: Mutex<Vec<Upload>>,
    upload_plan: Mutex<VecDeque<UploadBehavior>>,
    download_plan: Mutex<VecDeque<DownloadBehavior>>,
    thumbnail_plan: Mutex<VecDeque<DownloadBehavior>>,
    files: Mutex<HashMap<AttachmentId, Vec<u8>>>,
    thumbnails: Mutex<HashMap<AttachmentId, Vec<u8>>>,
    download_requests: AtomicUsize,
    thumbnail_requests: AtomicUsize,
}

impl Stub {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            emoji: EmojiStub::default(),
            search: SearchStub::default(),
            user: User {
                id: UserId::generate(),
                username: Username::parse("stubby").unwrap(),
                display_name: DisplayName::parse("Stubby").unwrap(),
                is_bot: false,
                status: None,
                deactivated: false,
                avatar_version: 0,
            },
            uploads: Mutex::new(Vec::new()),
            upload_plan: Mutex::new(VecDeque::new()),
            download_plan: Mutex::new(VecDeque::new()),
            thumbnail_plan: Mutex::new(VecDeque::new()),
            files: Mutex::new(HashMap::new()),
            thumbnails: Mutex::new(HashMap::new()),
            download_requests: AtomicUsize::new(0),
            thumbnail_requests: AtomicUsize::new(0),
        })
    }
}

fn authorized(headers: &HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "Bearer stub-token")
}

fn refusal(code: ErrorCode) -> Response {
    let status = StatusCode::from_u16(code.http_status()).unwrap();
    (status, Json(ApiError::new(code, "refused"))).into_response()
}

async fn login(State(s): State<Arc<Stub>>, Json(_): Json<LoginRequest>) -> Json<SessionResponse> {
    Json(SessionResponse {
        token: SessionToken::new("stub-token"),
        user: s.user.clone(),
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(value) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(value);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn upload(
    State(s): State<Arc<Stub>>,
    Path((conversation, attachment)): Path<(ConversationId, AttachmentId)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let behavior = s
        .upload_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(UploadBehavior::Ok);
    let mut record = Upload {
        conversation,
        attachment,
        name_header: headers
            .get("x-attachment-name")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        content_length: headers
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok()),
        authorized: authorized(&headers),
        received: 0,
        sha256: String::new(),
        aborted: false,
    };
    if !record.authorized {
        s.uploads.lock().unwrap().push(record);
        return refusal(ErrorCode::Unauthenticated);
    }
    if let UploadBehavior::Refuse(code) = behavior {
        s.uploads.lock().unwrap().push(record);
        return refusal(code);
    }
    // Pushed up front so a test can see a transfer that is still going.
    let index = {
        let mut uploads = s.uploads.lock().unwrap();
        uploads.push(record.clone());
        uploads.len() - 1
    };
    let mut hasher = Sha256::new();
    let mut stream = body.into_data_stream();
    let mut finished = true;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            finished = false;
            break;
        };
        hasher.update(&chunk);
        record.received += chunk.len();
        s.uploads.lock().unwrap()[index].received = record.received;
        match behavior {
            UploadBehavior::Slow(delay) => tokio::time::sleep(delay).await,
            UploadBehavior::ServerErrorAfter(limit) if record.received >= limit => break,
            _ => {}
        }
    }
    record.aborted = !finished;
    record.sha256 = hex(&hasher.finalize());
    s.uploads.lock().unwrap()[index] = record.clone();
    if !finished {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match behavior {
        UploadBehavior::ServerErrorAfterBody | UploadBehavior::ServerErrorAfter(_) => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        _ => Json(AttachmentResponse {
            attachment: Attachment {
                id: attachment,
                name: record
                    .name_header
                    .as_deref()
                    .map(percent_decode)
                    .unwrap_or_default(),
                media_type: record.content_type.clone().unwrap_or_default(),
                size: record.received as u64,
                sha256: record.sha256.clone(),
                width: None,
                height: None,
                has_thumbnail: false,
            },
        })
        .into_response(),
    }
}

async fn download(
    State(s): State<Arc<Stub>>,
    Path(attachment): Path<AttachmentId>,
    headers: HeaderMap,
) -> Response {
    s.download_requests.fetch_add(1, Ordering::SeqCst);
    serve(
        &headers,
        s.download_plan.lock().unwrap().pop_front(),
        s.files.lock().unwrap().get(&attachment).cloned(),
    )
}

async fn thumbnail(
    State(s): State<Arc<Stub>>,
    Path(attachment): Path<AttachmentId>,
    headers: HeaderMap,
) -> Response {
    s.thumbnail_requests.fetch_add(1, Ordering::SeqCst);
    serve(
        &headers,
        s.thumbnail_plan.lock().unwrap().pop_front(),
        s.thumbnails.lock().unwrap().get(&attachment).cloned(),
    )
}

fn serve(headers: &HeaderMap, plan: Option<DownloadBehavior>, bytes: Option<Vec<u8>>) -> Response {
    if !authorized(headers) {
        return refusal(ErrorCode::Unauthenticated);
    }
    let behavior = plan.unwrap_or(DownloadBehavior::Ok);
    let Some(mut bytes) = bytes else {
        return refusal(ErrorCode::NotFound);
    };
    match behavior {
        DownloadBehavior::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        DownloadBehavior::Corrupt => {
            for byte in bytes.iter_mut() {
                *byte ^= 0x5a;
            }
            bytes.into_response()
        }
        DownloadBehavior::Ok => bytes.into_response(),
        DownloadBehavior::Slow(delay) => {
            let length = bytes.len();
            let stream = futures_util::stream::unfold(0usize, move |sent| {
                let bytes = bytes.clone();
                async move {
                    if sent >= bytes.len() {
                        return None;
                    }
                    tokio::time::sleep(delay).await;
                    let end = (sent + 16 * 1024).min(bytes.len());
                    Some((Ok::<_, std::io::Error>(bytes[sent..end].to_vec()), end))
                }
            });
            Response::builder()
                .header("content-length", length)
                .body(Body::from_stream(stream))
                .unwrap()
        }
    }
}

fn emoji_for(stub: &Stub, workspace: WorkspaceId, id: EmojiId, name: &str) -> CustomEmoji {
    CustomEmoji {
        id,
        workspace_id: workspace,
        name: name.to_owned(),
        created_by: stub.user.id,
        created_at: Timestamp::now(),
        deleted: false,
    }
}

async fn emoji_upload(
    State(s): State<Arc<Stub>>,
    Path((workspace, emoji)): Path<(WorkspaceId, EmojiId)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let name = headers
        .get("x-emoji-name")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let upload = EmojiUpload {
        workspace,
        emoji,
        name_header: name.clone(),
        content_type: headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        authorized: authorized(&headers),
        bytes: body.to_vec(),
    };
    let ok = upload.authorized;
    s.emoji.uploads.lock().unwrap().push(upload);
    if !ok {
        return refusal(ErrorCode::Unauthenticated);
    }
    match s
        .emoji
        .plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(EmojiBehavior::Ok)
    {
        EmojiBehavior::Refuse(code) => refusal(code),
        EmojiBehavior::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        EmojiBehavior::Ok => {
            s.emoji.images.lock().unwrap().insert(emoji, body.to_vec());
            Json(CustomEmojiResponse {
                emoji: emoji_for(&s, workspace, emoji, name.as_deref().unwrap_or("x")),
            })
            .into_response()
        }
    }
}

async fn emoji_image(
    State(s): State<Arc<Stub>>,
    Path(emoji): Path<EmojiId>,
    headers: HeaderMap,
) -> Response {
    s.emoji.image_requests.fetch_add(1, Ordering::SeqCst);
    let plan = s.emoji.image_plan.lock().unwrap().pop_front();
    serve(
        &headers,
        match plan {
            Some(EmojiBehavior::ServerError) => Some(DownloadBehavior::ServerError),
            _ => None,
        },
        s.emoji.images.lock().unwrap().get(&emoji).cloned(),
    )
}

async fn emoji_rename(
    State(s): State<Arc<Stub>>,
    Path(emoji): Path<EmojiId>,
    headers: HeaderMap,
    Json(request): Json<RenameEmojiRequest>,
) -> Response {
    s.emoji.calls.lock().unwrap().push((
        "PATCH".to_owned(),
        emoji,
        serde_json::to_value(&request).unwrap(),
    ));
    if !authorized(&headers) {
        return refusal(ErrorCode::Unauthenticated);
    }
    match s
        .emoji
        .plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(EmojiBehavior::Ok)
    {
        EmojiBehavior::Refuse(code) => refusal(code),
        EmojiBehavior::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        EmojiBehavior::Ok => Json(CustomEmojiResponse {
            emoji: emoji_for(&s, WorkspaceId::generate(), emoji, request.name.as_str()),
        })
        .into_response(),
    }
}

async fn emoji_delete(
    State(s): State<Arc<Stub>>,
    Path(emoji): Path<EmojiId>,
    headers: HeaderMap,
) -> Response {
    s.emoji
        .calls
        .lock()
        .unwrap()
        .push(("DELETE".to_owned(), emoji, serde_json::Value::Null));
    if !authorized(&headers) {
        return refusal(ErrorCode::Unauthenticated);
    }
    match s
        .emoji
        .plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(EmojiBehavior::Ok)
    {
        EmojiBehavior::Refuse(code) => refusal(code),
        EmojiBehavior::ServerError => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        EmojiBehavior::Ok => StatusCode::NO_CONTENT.into_response(),
    }
}

/// Counts the request as abandoned unless it is answered.
struct SearchGuard {
    stub: Arc<Stub>,
    answered: bool,
}

impl Drop for SearchGuard {
    fn drop(&mut self) {
        if !self.answered {
            self.stub.search.aborted.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// The one result every successful search answers with: a hit whose snippet is the query as the
/// server read it, so a test sees what arrived.
fn search_answer(workspace: WorkspaceId, query: &str) -> SearchResponse {
    let conversation = lazy_protocol::Conversation {
        id: ConversationId::generate(),
        workspace_id: workspace,
        kind: lazy_domain::ConversationKind::Channel,
        name: Some(lazy_domain::ChannelName::parse("design").unwrap()),
        topic: None,
        description: None,
        visibility: lazy_domain::ChannelVisibility::Public,
        archived: false,
        is_default: false,
        created_by: None,
        participants: Vec::new(),
        member_count: 3,
    };
    let message = lazy_protocol::Message {
        id: lazy_domain::MessageId::generate(),
        conversation_id: conversation.id,
        author_id: UserId::generate(),
        document: MessageDocument::from_plain_text("the words of the message").unwrap(),
        created_at: Timestamp::now(),
        revision: 1,
        seq: lazy_domain::EventSeq::new(7).unwrap(),
        edited_at: None,
        thread_root: None,
        in_channel: false,
        thread: None,
        reactions: Vec::new(),
        reactions_as_of: lazy_domain::EventSeq::ZERO,
        mentions: lazy_protocol::Mentions::default(),
        deleted: false,
        attachments: Vec::new(),
    };
    SearchResponse {
        hits: vec![SearchHit {
            message,
            conversation,
            kind: SearchHitKind::Message,
            snippet: Snippet {
                text: query.to_owned(),
                highlights: vec![SnippetRange { start: 0, end: 1 }],
            },
        }],
        has_more: true,
        truncated: false,
    }
}

async fn search(
    State(s): State<Arc<Stub>>,
    Path(workspace): Path<WorkspaceId>,
    RawQuery(raw): RawQuery,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = params.get("q").cloned().unwrap_or_default();
    let authorized = authorized(&headers);
    s.search.calls.lock().unwrap().push(SearchCall {
        workspace,
        raw_query: raw.unwrap_or_default(),
        params,
        authorized,
    });
    let plan = s.search.plan.lock().unwrap().pop_front();
    if !authorized {
        return refusal(ErrorCode::Unauthenticated);
    }
    match plan.unwrap_or(SearchBehavior::Ok) {
        SearchBehavior::Ok => Json(search_answer(workspace, &query)).into_response(),
        SearchBehavior::Refuse(code) => refusal(code),
        SearchBehavior::ServerError => refusal(ErrorCode::Internal),
        SearchBehavior::Slow(delay) => {
            let mut guard = SearchGuard {
                stub: s.clone(),
                answered: false,
            };
            tokio::time::sleep(delay).await;
            guard.answered = true;
            Json(search_answer(workspace, &query)).into_response()
        }
    }
}

fn router(stub: Arc<Stub>) -> Router {
    Router::new()
        .route("/v1/auth/login", post(login))
        .route("/v1/workspaces/{workspace_id}/search", get(search))
        .route(
            "/v1/conversations/{conversation_id}/attachments/{attachment_id}",
            put(upload),
        )
        .route("/v1/attachments/{attachment_id}", get(download))
        .route("/v1/attachments/{attachment_id}/thumbnail", get(thumbnail))
        .route(
            "/v1/workspaces/{workspace_id}/emoji/{emoji_id}",
            put(emoji_upload),
        )
        .route(
            "/v1/emoji/{emoji_id}",
            get(emoji_image).patch(emoji_rename).delete(emoji_delete),
        )
        .with_state(stub)
}

// ----- Fixture -----

struct Server {
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

    fn uploads(&self) -> Vec<Upload> {
        self.stub.uploads.lock().unwrap().clone()
    }
}

fn config(url: &str, profile: &TempDir, storage: Arc<dyn SecureStorage>) -> RuntimeConfig {
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
    skipped: Vec<Input>,
}

impl Client {
    fn start(server: &Server) -> Self {
        Self::start_with(server, tempfile::tempdir().unwrap(), MemoryStorage::new())
    }

    fn start_with(server: &Server, profile: TempDir, storage: Arc<MemoryStorage>) -> Self {
        let (runtime, inputs) =
            Runtime::start(config(&server.url, &profile, storage.clone())).expect("runtime starts");
        let mut client = Self {
            runtime: Some(runtime),
            inputs,
            profile,
            storage,
            url: server.url.clone(),
            skipped: Vec::new(),
        };
        client.expect("Started", |i| matches!(i, Input::Started(_)));
        client
    }

    fn logged_in(server: &Server) -> Self {
        let mut client = Self::start(server);
        client.login();
        client
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

    fn restart(mut self, server: &Server) -> Self {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
        Self::start_with(server, self.profile, self.storage)
    }

    fn execute(&self, effects: Vec<Effect>) {
        self.runtime.as_ref().unwrap().execute(effects);
    }

    fn vault(&self) -> SecretVault {
        SecretVault::new(self.storage.clone(), self.profile.path(), &self.url)
    }

    fn dir(&self, name: &str) -> PathBuf {
        self.profile.path().join(name)
    }

    fn expect(&mut self, what: &str, matches: impl Fn(&Input) -> bool) -> Input {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let received =
                block_on(async { tokio::time::timeout(remaining, self.inputs.recv()).await });
            match received {
                Ok(Ok(input)) if matches(&input) => return input,
                Ok(Ok(other)) => self.skipped.push(other),
                Ok(Err(_)) => panic!("input channel closed while waiting for {what}"),
                Err(_) => panic!("timed out waiting for {what}; skipped: {:?}", self.skipped),
            }
        }
    }

    fn expect_quiet(&mut self, window: Duration, ignoring_progress: bool) {
        let deadline = Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let received =
                block_on(async { tokio::time::timeout(remaining, self.inputs.recv()).await });
            match received {
                Ok(Ok(Input::UploadProgress { .. } | Input::DownloadProgress { .. }))
                    if ignoring_progress => {}
                Ok(Ok(input)) => panic!("unexpected input: {input:?}"),
                _ => return,
            }
        }
    }

    /// Copies `bytes` into the outbox as `attachment`, waiting until it is there.
    fn stage_bytes(&mut self, attachment: AttachmentId, bytes: Vec<u8>) {
        self.execute(vec![Effect::StageFile {
            attachment,
            generation: 1,
            source: FileSource::Bytes(FileBytes::new(bytes)),
        }]);
        let staged = self.expect(
            "FileStaged",
            |i| matches!(i, Input::FileStaged { attachment: a, .. } if *a == attachment),
        );
        assert!(
            matches!(staged, Input::FileStaged { result: Ok(_), .. }),
            "{staged:?}"
        );
    }

    fn upload(
        &mut self,
        attachment: AttachmentId,
        generation: u64,
        size: u64,
    ) -> Result<AttachmentResponse, TransferError> {
        self.execute(vec![Effect::UploadFile {
            attachment,
            conversation: ConversationId::generate(),
            generation,
            name: "my photo (1).png".to_owned(),
            media_type: "image/png".to_owned(),
            size,
        }]);
        let result = self.expect("UploadResult", |i| {
            matches!(i, Input::UploadResult { attachment: a, generation: g, .. } if *a == attachment && *g == generation)
        });
        let Input::UploadResult { result, .. } = result else {
            unreachable!()
        };
        result
    }

    fn shutdown(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown();
        }
    }

    /// Asks for a page of search results and waits for the report of that request.
    fn search(
        &mut self,
        workspace: WorkspaceId,
        generation: u64,
        params: SearchParams,
    ) -> Result<SearchResponse, RequestError> {
        self.execute(vec![Effect::Search {
            workspace,
            generation,
            params,
        }]);
        self.search_report(generation)
    }

    fn search_report(&mut self, generation: u64) -> Result<SearchResponse, RequestError> {
        let report = self.expect(
            "SearchResult",
            |i| matches!(i, Input::SearchResult { generation: g, .. } if *g == generation),
        );
        let Input::SearchResult { result, .. } = report else {
            unreachable!()
        };
        result
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Like [`wait_until`] with a shorter limit, for what must happen promptly rather than
/// eventually (a request that would otherwise time out on its own after seconds).
fn wait_within(what: &str, limit: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen within {limit:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn sample(len: usize) -> Vec<u8> {
    let mut state = 0x9e3779b97f4a7c15u64;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn sha(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn attachment_for(id: AttachmentId, name: &str, bytes: &[u8]) -> Attachment {
    Attachment {
        id,
        name: name.to_owned(),
        media_type: "application/octet-stream".to_owned(),
        size: bytes.len() as u64,
        sha256: sha(bytes),
        width: None,
        height: None,
        has_thumbnail: true,
    }
}

fn files_in(dir: &FsPath) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn rejected_code(error: &TransferError) -> Option<ErrorCode> {
    match error {
        TransferError::Request(RequestError::Rejected(api)) => Some(api.code),
        _ => None,
    }
}

// ----- Copies in the outbox -----

#[test]
fn a_chosen_file_is_copied_encrypted_into_the_outbox_and_reported_with_its_size() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(300_000);
    let source = client.profile.path().join("holiday.png");
    std::fs::write(&source, &data).unwrap();
    let id = AttachmentId::generate();
    client.execute(vec![Effect::StageFile {
        attachment: id,
        generation: 1,
        source: FileSource::Path(source.clone()),
    }]);
    let staged = client.expect("FileStaged", |i| matches!(i, Input::FileStaged { .. }));
    let Input::FileStaged {
        result: Ok(file), ..
    } = staged
    else {
        panic!("expected a copy");
    };
    assert_eq!(file.size, 300_000);
    let copy = client.dir("outbox").join(format!("{id}.bin"));
    let on_disk = std::fs::read(&copy).unwrap();
    assert!(!on_disk.windows(64).any(|w| w == &data[5000..5064]));
    // The original can go; the copy still uploads.
    std::fs::remove_file(source).unwrap();
    let response = client.upload(id, 1, 300_000).expect("uploaded");
    assert_eq!(response.attachment.size, 300_000);
    assert_eq!(response.attachment.sha256, sha(&data));
}

#[test]
fn a_file_over_the_limit_a_directory_and_a_missing_file_are_refused_before_any_copy() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let big = client.profile.path().join("big.bin");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(26 * 1024 * 1024).unwrap();
    let cases = [
        (big, "TooLarge"),
        (client.profile.path().to_owned(), "Unreadable"),
        (client.profile.path().join("nope.txt"), "Unreadable"),
    ];
    for (path, kind) in cases {
        let id = AttachmentId::generate();
        client.execute(vec![Effect::StageFile {
            attachment: id,
            generation: 1,
            source: FileSource::Path(path),
        }]);
        let staged = client.expect(
            "FileStaged",
            |i| matches!(i, Input::FileStaged { attachment, .. } if *attachment == id),
        );
        let Input::FileStaged {
            result: Err(error), ..
        } = staged
        else {
            panic!("expected a refusal");
        };
        match (&error, kind) {
            (TransferError::TooLarge { size }, "TooLarge") => {
                assert_eq!(*size, 26 * 1024 * 1024)
            }
            (TransferError::Unreadable(_), "Unreadable") => {}
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(files_in(&client.dir("outbox")).is_empty());
}

#[test]
fn releasing_deletes_the_copy_and_an_upload_of_it_is_missing() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(1000));
    assert_eq!(files_in(&client.dir("outbox")).len(), 1);
    client.execute(vec![Effect::ReleaseFiles {
        attachments: vec![id],
    }]);
    wait_until("the copy to go", || {
        files_in(&client.dir("outbox")).is_empty()
    });
    let error = client.upload(id, 1, 1000).unwrap_err();
    assert_eq!(error, TransferError::Missing);
    assert!(server.uploads().is_empty());
}

#[test]
fn copies_made_in_order_and_released_in_order_end_up_as_asked() {
    // Stage, release, and stage again at once, as cancelling and retrying a chip does.
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.execute(vec![
        Effect::StageFile {
            attachment: id,
            generation: 1,
            source: FileSource::Bytes(FileBytes::new(sample(500))),
        },
        Effect::ReleaseFiles {
            attachments: vec![id],
        },
        Effect::StageFile {
            attachment: id,
            generation: 1,
            source: FileSource::Bytes(FileBytes::new(sample(700))),
        },
    ]);
    // Whatever came of the first, the last copy is the one that is there.
    client.expect(
        "FileStaged for the last copy",
        |i| matches!(i, Input::FileStaged { result: Ok(file), .. } if file.size == 700),
    );
    wait_until("one copy", || files_in(&client.dir("outbox")).len() == 1);
    let response = client.upload(id, 1, 700).expect("uploaded");
    assert_eq!(response.attachment.size, 700);
}

// ----- Uploads -----

#[test]
fn an_upload_is_one_authenticated_put_of_the_decrypted_bytes_with_the_documented_headers() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(200_001);
    let id = AttachmentId::generate();
    client.stage_bytes(id, data.clone());
    let conversation = ConversationId::generate();
    client.execute(vec![Effect::UploadFile {
        attachment: id,
        conversation,
        generation: 7,
        name: "my photo (1).png".to_owned(),
        media_type: "image/png".to_owned(),
        size: 200_001,
    }]);
    let result = client.expect("UploadResult", |i| matches!(i, Input::UploadResult { .. }));
    let Input::UploadResult {
        generation, result, ..
    } = result
    else {
        unreachable!()
    };
    assert_eq!(generation, 7);
    let response = result.expect("uploaded");
    assert_eq!(response.attachment.id, id);
    let uploads = server.uploads();
    assert_eq!(uploads.len(), 1);
    let seen = &uploads[0];
    assert_eq!(seen.conversation, conversation);
    assert_eq!(seen.attachment, id);
    assert!(seen.authorized);
    assert_eq!(
        seen.name_header.as_deref(),
        Some("my%20photo%20%281%29.png")
    );
    assert_eq!(seen.content_type.as_deref(), Some("image/png"));
    assert_eq!(seen.content_length, Some(200_001));
    assert_eq!(seen.received, 200_001);
    assert_eq!(seen.sha256, sha(&data));
    // The copy stays until the core releases it.
    assert_eq!(files_in(&client.dir("outbox")).len(), 1);
}

#[test]
fn an_upload_that_is_slow_reports_progress_that_only_moves_forward() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(12_000_000));
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::Slow(Duration::from_millis(5)));
    client.execute(vec![Effect::UploadFile {
        attachment: id,
        conversation: ConversationId::generate(),
        generation: 3,
        name: "a.bin".into(),
        media_type: "application/octet-stream".into(),
        size: 12_000_000,
    }]);
    client.expect("UploadResult", |i| matches!(i, Input::UploadResult { .. }));
    let mut last = 0;
    let mut reports = 0;
    for input in &client.skipped {
        if let Input::UploadProgress {
            generation,
            sent,
            total,
            ..
        } = input
        {
            assert_eq!(*generation, 3);
            assert_eq!(*total, 12_000_000);
            assert!(*sent >= last);
            assert!(*sent <= *total);
            last = *sent;
            reports += 1;
        }
    }
    assert!(reports >= 2, "saw {reports} progress reports");
    // Throttled: far fewer than one per chunk (about 180 chunks of 64 KiB were read).
    assert!(reports < 200);
}

#[test]
fn refusals_are_definitive_and_are_not_repeated_by_the_runtime() {
    for code in [
        ErrorCode::PayloadTooLarge,
        ErrorCode::UnsupportedMedia,
        ErrorCode::Conflict,
        ErrorCode::LimitReached,
        ErrorCode::Forbidden,
    ] {
        let server = Server::start();
        let mut client = Client::logged_in(&server);
        let id = AttachmentId::generate();
        client.stage_bytes(id, sample(2000));
        server
            .stub
            .upload_plan
            .lock()
            .unwrap()
            .push_back(UploadBehavior::Refuse(code));
        let error = client.upload(id, 1, 2000).unwrap_err();
        assert_eq!(rejected_code(&error), Some(code), "{code:?}: {error:?}");
        client.expect_quiet(Duration::from_millis(150), false);
        assert_eq!(server.uploads().len(), 1, "{code:?}");
    }
}

#[test]
fn a_large_upload_refused_before_its_body_is_still_reported_as_the_refusal() {
    // The server answers at once and reads nothing of 20 MiB of body.
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(20 * 1024 * 1024));
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::Refuse(ErrorCode::PayloadTooLarge));
    let error = client.upload(id, 1, 20 * 1024 * 1024).unwrap_err();
    // Either the refusal got through, or the connection was cut while it was still being sent;
    // the second is an unknown outcome that the core repeats and then reports.
    assert!(
        rejected_code(&error) == Some(ErrorCode::PayloadTooLarge)
            || matches!(error, TransferError::Request(RequestError::Unknown(_))),
        "{error:?}"
    );
}

#[test]
fn a_server_failure_is_an_unknown_outcome_and_the_same_upload_can_be_repeated() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(5000);
    let id = AttachmentId::generate();
    client.stage_bytes(id, data.clone());
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::ServerErrorAfterBody);
    let error = client.upload(id, 1, 5000).unwrap_err();
    assert!(
        matches!(error, TransferError::Request(RequestError::Unknown(_))),
        "{error:?}"
    );
    // One attempt per effect: the runtime did not repeat it.
    client.expect_quiet(Duration::from_millis(150), false);
    assert_eq!(server.uploads().len(), 1);
    // The core repeats it with the same id; the copy was kept for that.
    let response = client.upload(id, 2, 5000).expect("the repeat succeeds");
    assert_eq!(response.attachment.sha256, sha(&data));
    let uploads = server.uploads();
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0].attachment, uploads[1].attachment);
    assert_eq!(uploads[0].sha256, uploads[1].sha256);
}

#[test]
fn a_server_that_stops_reading_part_way_is_an_unknown_outcome() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(600_000));
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::ServerErrorAfter(100_000));
    let error = client.upload(id, 1, 600_000).unwrap_err();
    assert!(
        matches!(error, TransferError::Request(RequestError::Unknown(_))),
        "{error:?}"
    );
    let seen = &server.uploads()[0];
    assert!(seen.received < 600_000, "received {}", seen.received);
}

#[test]
fn cancelling_stops_the_transfer_and_no_result_is_reported() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(2 * 1024 * 1024));
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::Slow(Duration::from_millis(100)));
    client.execute(vec![Effect::UploadFile {
        attachment: id,
        conversation: ConversationId::generate(),
        generation: 1,
        name: "big.bin".into(),
        media_type: "application/octet-stream".into(),
        size: 2 * 1024 * 1024,
    }]);
    wait_until("the upload to start", || {
        server.uploads().first().is_some_and(|u| u.received > 0)
    });
    client.execute(vec![Effect::CancelUpload { attachment: id }]);
    client.expect_quiet(Duration::from_millis(600), true);
    wait_until("the server to see the hang-up", || {
        server.uploads()[0].aborted
    });
    assert!(server.uploads()[0].received < 2 * 1024 * 1024);
}

#[test]
fn an_upload_of_a_copy_that_is_missing_or_damaged_is_the_files_fault_not_the_networks() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let missing = AttachmentId::generate();
    assert_eq!(
        client.upload(missing, 1, 10).unwrap_err(),
        TransferError::Missing
    );

    // Damage one chunk of a copy of several, so the failure comes after the upload began.
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(300_000));
    let path = client.dir("outbox").join(format!("{id}.bin"));
    let mut bytes = std::fs::read(&path).unwrap();
    let at = bytes.len() - 100;
    bytes[at] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let error = client.upload(id, 1, 300_000).unwrap_err();
    assert!(matches!(error, TransferError::Unreadable(_)), "{error:?}");
}

#[test]
fn a_request_without_a_session_is_the_unauthenticated_refusal() {
    let server = Server::start();
    let mut client = Client::start(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(100));
    let error = client.upload(id, 1, 100).unwrap_err();
    assert_eq!(rejected_code(&error), Some(ErrorCode::Unauthenticated));
}

// ----- Downloads -----

fn start_download(
    client: &mut Client,
    attachment: &Attachment,
    destination: &FsPath,
) -> Result<PathBuf, TransferError> {
    client.execute(vec![Effect::DownloadAttachment {
        attachment: attachment.clone(),
        destination: destination.to_owned(),
    }]);
    let result = client.expect(
        "DownloadResult",
        |i| matches!(i, Input::DownloadResult { attachment: a, .. } if *a == attachment.id),
    );
    let Input::DownloadResult { result, .. } = result else {
        unreachable!()
    };
    result
}

#[test]
fn a_download_is_checked_against_its_sha256_and_renamed_into_place() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(250_000);
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    let attachment = attachment_for(id, "report.pdf", &data);
    let folder = tempfile::tempdir().unwrap();
    let destination = folder.path().join("report.pdf");
    // Saving over a file that is there replaces it.
    std::fs::write(&destination, b"old").unwrap();
    let saved = start_download(&mut client, &attachment, &destination).expect("saved");
    assert_eq!(saved, destination);
    assert_eq!(std::fs::read(&destination).unwrap(), data);
    // Nothing else is left beside it.
    assert_eq!(files_in(folder.path()), vec!["report.pdf".to_owned()]);
}

#[test]
fn a_download_that_does_not_match_its_checksum_leaves_nothing_behind() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(50_000);
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    server
        .stub
        .download_plan
        .lock()
        .unwrap()
        .push_back(DownloadBehavior::Corrupt);
    let attachment = attachment_for(id, "x.bin", &data);
    let folder = tempfile::tempdir().unwrap();
    let destination = folder.path().join("x.bin");
    let error = start_download(&mut client, &attachment, &destination).unwrap_err();
    assert_eq!(error, TransferError::ChecksumMismatch);
    assert!(files_in(folder.path()).is_empty());
    // And a size that is not the one promised is the same failure.
    let mut wrong = attachment.clone();
    wrong.size += 10;
    let error = start_download(&mut client, &wrong, &destination).unwrap_err();
    assert_eq!(error, TransferError::ChecksumMismatch);
    assert!(files_in(folder.path()).is_empty());
}

#[test]
fn a_download_refused_or_failing_reports_it_and_leaves_nothing_behind() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(1000);
    let folder = tempfile::tempdir().unwrap();
    // Not there at all.
    let gone = attachment_for(AttachmentId::generate(), "gone.bin", &data);
    let error = start_download(&mut client, &gone, &folder.path().join("gone.bin")).unwrap_err();
    assert_eq!(rejected_code(&error), Some(ErrorCode::NotFound));
    // A server failure is not a refusal.
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    server
        .stub
        .download_plan
        .lock()
        .unwrap()
        .push_back(DownloadBehavior::ServerError);
    let attachment = attachment_for(id, "x.bin", &data);
    let error = start_download(&mut client, &attachment, &folder.path().join("x.bin")).unwrap_err();
    assert!(matches!(
        error,
        TransferError::Request(RequestError::Unknown(_))
    ));
    assert!(files_in(folder.path()).is_empty());
}

#[test]
fn a_destination_that_cannot_be_written_is_a_disk_failure_and_the_download_stops() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(1000);
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    let attachment = attachment_for(id, "x.bin", &data);
    // A folder that is not there.
    let destination = client.profile.path().join("no such folder").join("x.bin");
    let error = start_download(&mut client, &attachment, &destination).unwrap_err();
    assert!(matches!(error, TransferError::Disk(_)), "{error:?}");
    assert!(!destination.exists());
    // A device that accepts no bytes: the same, and the temporary file is gone. (Linux only.)
    #[cfg(target_os = "linux")]
    {
        let folder = tempfile::tempdir().unwrap();
        let temp_name = format!(".lazy-{id}.part");
        std::os::unix::fs::symlink("/dev/full", folder.path().join(&temp_name)).unwrap();
        let error =
            start_download(&mut client, &attachment, &folder.path().join("x.bin")).unwrap_err();
        assert!(matches!(error, TransferError::Disk(_)), "{error:?}");
        assert!(!folder.path().join("x.bin").exists());
    }
}

#[test]
fn cancelling_a_download_stops_it_and_removes_the_temporary_file() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(400_000);
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    server
        .stub
        .download_plan
        .lock()
        .unwrap()
        .push_back(DownloadBehavior::Slow(Duration::from_millis(80)));
    let attachment = attachment_for(id, "slow.bin", &data);
    let folder = tempfile::tempdir().unwrap();
    client.execute(vec![Effect::DownloadAttachment {
        attachment: attachment.clone(),
        destination: folder.path().join("slow.bin"),
    }]);
    client.expect("some progress", |i| {
        matches!(i, Input::DownloadProgress { .. })
    });
    assert!(
        !files_in(folder.path()).is_empty(),
        "a temporary file is there while it runs"
    );
    client.execute(vec![Effect::CancelDownload { attachment: id }]);
    client.expect_quiet(Duration::from_millis(400), true);
    wait_until("the temporary file to go", || {
        files_in(folder.path()).is_empty()
    });
}

#[test]
fn download_progress_only_moves_forward_and_is_throttled() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let data = sample(300_000);
    let id = AttachmentId::generate();
    server.stub.files.lock().unwrap().insert(id, data.clone());
    server
        .stub
        .download_plan
        .lock()
        .unwrap()
        .push_back(DownloadBehavior::Slow(Duration::from_millis(40)));
    let attachment = attachment_for(id, "p.bin", &data);
    let folder = tempfile::tempdir().unwrap();
    start_download(&mut client, &attachment, &folder.path().join("p.bin")).expect("saved");
    let mut last = 0;
    let mut reports = 0;
    for input in &client.skipped {
        if let Input::DownloadProgress {
            received, total, ..
        } = input
        {
            assert_eq!(*total, 300_000);
            assert!(*received >= last && *received <= *total);
            last = *received;
            reports += 1;
        }
    }
    assert!(reports >= 2, "saw {reports} reports");
}

// ----- Images -----

fn load_image(
    client: &mut Client,
    id: AttachmentId,
    full: bool,
) -> Result<ImageData, TransferError> {
    client.execute(vec![Effect::FetchImage {
        attachment: id,
        full,
    }]);
    let loaded = client.expect("ImageLoaded", |i| {
        matches!(i, Input::ImageLoaded { attachment, full: f, .. } if *attachment == id && *f == full)
    });
    let Input::ImageLoaded { result, .. } = loaded else {
        unreachable!()
    };
    result
}

#[test]
fn a_thumbnail_is_fetched_once_then_served_from_the_encrypted_cache_even_after_a_restart() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    let jpeg = sample(30_000);
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(id, jpeg.clone());
    let first = load_image(&mut client, id, false).expect("fetched");
    assert_eq!(first.bytes(), jpeg.as_slice());
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 1);
    // Asked for again, it is answered locally.
    let again = load_image(&mut client, id, false).expect("cached");
    assert_eq!(again.bytes(), jpeg.as_slice());
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 1);
    // On disk it is not the picture.
    let names = files_in(&client.dir("images"));
    assert_eq!(names.len(), 1, "{names:?}");
    let stored = std::fs::read(client.dir("images").join(&names[0])).unwrap();
    assert!(!stored.windows(64).any(|w| w == &jpeg[1000..1064]));
    // And a restart still has it.
    let mut client = client.restart(&server);
    let after = load_image(&mut client, id, false).expect("cached across a restart");
    assert_eq!(after.bytes(), jpeg.as_slice());
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 1);
}

#[test]
fn the_thumbnail_and_the_whole_image_are_cached_apart() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(id, sample(1000));
    server.stub.files.lock().unwrap().insert(id, sample(5000));
    let thumb = load_image(&mut client, id, false).unwrap();
    let full = load_image(&mut client, id, true).unwrap();
    assert_eq!(thumb.len(), 1000);
    assert_eq!(full.len(), 5000);
    assert_eq!(files_in(&client.dir("images")).len(), 2);
}

#[test]
fn a_missing_image_is_a_refusal_and_a_flaky_read_is_retried() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let gone = AttachmentId::generate();
    let error = load_image(&mut client, gone, false).unwrap_err();
    assert_eq!(rejected_code(&error), Some(ErrorCode::NotFound));

    let id = AttachmentId::generate();
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(id, sample(2000));
    server
        .stub
        .thumbnail_plan
        .lock()
        .unwrap()
        .push_back(DownloadBehavior::ServerError);
    let image = load_image(&mut client, id, false).expect("the second attempt works");
    assert_eq!(image.len(), 2000);
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 3);
}

#[test]
fn a_damaged_cache_entry_is_fetched_again_without_a_word() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    let jpeg = sample(8000);
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(id, jpeg.clone());
    load_image(&mut client, id, false).unwrap();
    let mut client = client.restart(&server);
    let names = files_in(&client.dir("images"));
    let path = client.dir("images").join(&names[0]);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
    let after = load_image(&mut client, id, false).expect("fetched again");
    assert_eq!(after.bytes(), jpeg.as_slice());
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 2);
    client.expect_quiet(Duration::from_millis(100), false);
}

#[test]
fn forgetting_attachments_drops_their_cached_images() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(id, sample(500));
    load_image(&mut client, id, false).unwrap();
    assert_eq!(files_in(&client.dir("images")).len(), 1);
    client.execute(vec![Effect::ForgetAttachments {
        attachments: vec![id],
    }]);
    wait_until("the cached image to go", || {
        files_in(&client.dir("images")).is_empty()
    });
    load_image(&mut client, id, false).unwrap();
    assert_eq!(server.stub.thumbnail_requests.load(Ordering::SeqCst), 2);
}

// ----- Restart and sign-out -----

fn journal_naming(files: &[(AttachmentId, u64)]) -> JournalEntry {
    JournalEntry {
        operation_id: OperationId::generate(),
        user_id: UserId::generate(),
        conversation_id: ConversationId::generate(),
        document: MessageDocument::empty(),
        submitted_at: Timestamp::now(),
        kind: JournalKind::Send,
        thread_root: None,
        also_in_channel: false,
        attachments: files
            .iter()
            .map(|(id, size)| JournalAttachment {
                id: *id,
                name: "f.bin".to_owned(),
                size: *size,
            })
            .collect(),
    }
}

#[test]
fn a_restart_keeps_the_copies_the_journal_names_and_sweeps_the_rest() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let kept = AttachmentId::generate();
    let orphan = AttachmentId::generate();
    client.stage_bytes(kept, sample(2000));
    client.stage_bytes(orphan, sample(3000));
    let entry = journal_naming(&[(kept, 2000)]);
    let operation = entry.operation_id;
    client.execute(vec![Effect::PersistSubmission(entry)]);
    client.expect(
        "SubmissionPersisted",
        |i| matches!(i, Input::SubmissionPersisted { operation: o } if *o == operation),
    );
    // A half-made copy from a crash is swept too.
    std::fs::write(
        client
            .dir("outbox")
            .join(format!("{}.part", AttachmentId::generate())),
        b"half",
    )
    .unwrap();
    assert_eq!(files_in(&client.dir("outbox")).len(), 3);

    let mut client = client.restart(&server);
    assert_eq!(files_in(&client.dir("outbox")), vec![format!("{kept}.bin")]);
    // The journal came back with its file, and the kept copy still uploads.
    let started = client.skipped.iter().find_map(|i| match i {
        Input::Started(state) => Some(state.clone()),
        _ => None,
    });
    let _ = started;
    let response = client.upload(kept, 1, 2000);
    // (Not signed in on this restart: the point is that the copy was found.)
    assert!(
        !matches!(response, Err(TransferError::Missing)),
        "{response:?}"
    );
}

#[test]
fn the_journal_with_files_survives_a_restart_in_the_state_the_core_reads() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(100));
    let entry = journal_naming(&[(id, 100)]);
    let operation = entry.operation_id;
    client.execute(vec![Effect::PersistSubmission(entry.clone())]);
    client.expect(
        "SubmissionPersisted",
        |i| matches!(i, Input::SubmissionPersisted { operation: o } if *o == operation),
    );
    let key = client.vault().state_key().unwrap();
    client.shutdown();
    let state = inspect_state(client.profile.path(), &key);
    assert_eq!(state.journal, vec![entry]);
}

#[test]
fn signing_out_deletes_the_outbox_and_the_image_cache_and_stops_transfers() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let id = AttachmentId::generate();
    client.stage_bytes(id, sample(2 * 1024 * 1024));
    let thumb = AttachmentId::generate();
    server
        .stub
        .thumbnails
        .lock()
        .unwrap()
        .insert(thumb, sample(500));
    load_image(&mut client, thumb, false).unwrap();
    assert!(client.dir("outbox").exists() && client.dir("images").exists());
    server
        .stub
        .upload_plan
        .lock()
        .unwrap()
        .push_back(UploadBehavior::Slow(Duration::from_millis(200)));
    client.execute(vec![Effect::UploadFile {
        attachment: id,
        conversation: ConversationId::generate(),
        generation: 1,
        name: "a.bin".into(),
        media_type: "application/octet-stream".into(),
        size: 2 * 1024 * 1024,
    }]);
    wait_until("the upload to start", || {
        server.uploads().first().is_some_and(|u| u.received > 0)
    });
    client.execute(vec![Effect::SignOut]);
    wait_until("the outbox and images to go", || {
        !client.dir("outbox").exists() && !client.dir("images").exists()
    });
    client.expect_quiet(Duration::from_millis(300), true);
    wait_until("the upload to be hung up on", || {
        server.uploads()[0].aborted
    });
}

#[test]
fn a_profile_without_secure_storage_says_so_instead_of_keeping_an_unprotected_copy() {
    let server = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let config = RuntimeConfig {
        secure_storage: SecureStorageChoice::Custom(Arc::new(Unavailable)),
        ..config(&server.url, &profile, MemoryStorage::new())
    };
    let (runtime, inputs) = Runtime::start(config).unwrap();
    let mut client = Client {
        runtime: Some(runtime),
        inputs,
        profile,
        storage: MemoryStorage::new(),
        url: server.url.clone(),
        skipped: Vec::new(),
    };
    client.expect("Started", |i| matches!(i, Input::Started(_)));
    let id = AttachmentId::generate();
    client.execute(vec![Effect::StageFile {
        attachment: id,
        generation: 1,
        source: FileSource::Bytes(FileBytes::new(vec![1; 10])),
    }]);
    let staged = client.expect("FileStaged", |i| matches!(i, Input::FileStaged { .. }));
    let Input::FileStaged {
        result: Err(TransferError::Disk(why)),
        ..
    } = staged
    else {
        panic!("expected a refusal to keep a copy");
    };
    assert!(why.contains("secure storage"));
    assert!(!client.dir("outbox").exists());
}

struct Unavailable;

impl SecureStorage for Unavailable {
    fn get(&self, _: &str) -> Result<Option<Vec<u8>>, lazy_client_runtime::StorageError> {
        Err(lazy_client_runtime::StorageError::Unavailable(
            "no keyring".to_owned(),
        ))
    }
    fn set(&self, _: &str, _: &[u8]) -> Result<(), lazy_client_runtime::StorageError> {
        Err(lazy_client_runtime::StorageError::Unavailable(
            "no keyring".to_owned(),
        ))
    }
    fn delete(&self, _: &str) -> Result<(), lazy_client_runtime::StorageError> {
        Ok(())
    }
}

// ----- Custom emoji -----

/// A PNG's first bytes and some more, so the upload reads as an image.
fn png(len: usize) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend(sample(len));
    bytes
}

fn upload_emoji(
    client: &mut Client,
    workspace: WorkspaceId,
    emoji: EmojiId,
    name: &str,
    source: FileSource,
) -> Result<CustomEmojiResponse, TransferError> {
    client.execute(vec![Effect::UploadEmoji {
        workspace,
        emoji,
        name: EmojiName::parse(name).unwrap(),
        source,
    }]);
    let done = client.expect(
        "CustomEmojiUploaded",
        |i| matches!(i, Input::CustomEmojiUploaded { emoji: e, .. } if *e == emoji),
    );
    let Input::CustomEmojiUploaded { result, .. } = done else {
        unreachable!()
    };
    result
}

fn action_result(client: &mut Client, kind: ActionKind) -> Result<ActionOutcome, RequestError> {
    let done = client.expect(
        "ActionResult",
        |i| matches!(i, Input::ActionResult { kind: k, .. } if *k == kind),
    );
    let Input::ActionResult { result, .. } = done else {
        unreachable!()
    };
    result
}

#[test]
fn an_emoji_upload_is_one_put_with_the_name_header_and_the_whole_image() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let (workspace, emoji) = (WorkspaceId::generate(), EmojiId::generate());
    let image = png(3000);
    let response = upload_emoji(
        &mut client,
        workspace,
        emoji,
        "party_time",
        FileSource::Bytes(FileBytes::new(image.clone())),
    )
    .expect("uploaded");
    assert_eq!(response.emoji.id, emoji);
    assert_eq!(response.emoji.name, "party_time");
    let uploads = server.stub.emoji.uploads.lock().unwrap().clone();
    assert_eq!(uploads.len(), 1);
    let seen = &uploads[0];
    assert_eq!((seen.workspace, seen.emoji), (workspace, emoji));
    assert_eq!(seen.name_header.as_deref(), Some("party_time"));
    assert_eq!(seen.content_type.as_deref(), Some("image/png"));
    assert!(seen.authorized);
    assert_eq!(seen.bytes, image);
}

#[test]
fn an_emoji_image_is_read_from_a_path_and_one_over_the_limit_is_refused_before_it_is_read() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let workspace = WorkspaceId::generate();
    let good = client.profile.path().join("party.png");
    std::fs::write(&good, png(1000)).unwrap();
    let ok = upload_emoji(
        &mut client,
        workspace,
        EmojiId::generate(),
        "party_time",
        FileSource::Path(good),
    );
    assert!(ok.is_ok());
    // Over 512 KiB: refused from the file's size, nothing sent.
    let big = client.profile.path().join("big.png");
    let file = std::fs::File::create(&big).unwrap();
    file.set_len(513 * 1024).unwrap();
    let error = upload_emoji(
        &mut client,
        workspace,
        EmojiId::generate(),
        "big_one",
        FileSource::Path(big),
    )
    .unwrap_err();
    assert_eq!(error, TransferError::TooLarge { size: 513 * 1024 });
    // Pasted bytes over the limit too, a missing file, and a directory.
    let error = upload_emoji(
        &mut client,
        workspace,
        EmojiId::generate(),
        "big_two",
        FileSource::Bytes(FileBytes::new(vec![1; 513 * 1024])),
    )
    .unwrap_err();
    assert!(matches!(error, TransferError::TooLarge { .. }));
    for path in [
        client.profile.path().join("nope.png"),
        client.profile.path().to_owned(),
    ] {
        let error = upload_emoji(
            &mut client,
            workspace,
            EmojiId::generate(),
            "gone_one",
            FileSource::Path(path),
        )
        .unwrap_err();
        assert!(matches!(error, TransferError::Unreadable(_)), "{error:?}");
    }
    assert_eq!(server.stub.emoji.uploads.lock().unwrap().len(), 1);
}

#[test]
fn emoji_refusals_are_final_and_an_unknown_outcome_can_be_repeated_with_the_same_id() {
    for code in [
        ErrorCode::NameTaken,
        ErrorCode::UnsupportedMedia,
        ErrorCode::PayloadTooLarge,
        ErrorCode::LimitReached,
        ErrorCode::Forbidden,
    ] {
        let server = Server::start();
        let mut client = Client::logged_in(&server);
        server
            .stub
            .emoji
            .plan
            .lock()
            .unwrap()
            .push_back(EmojiBehavior::Refuse(code));
        let error = upload_emoji(
            &mut client,
            WorkspaceId::generate(),
            EmojiId::generate(),
            "party_time",
            FileSource::Bytes(FileBytes::new(png(10))),
        )
        .unwrap_err();
        assert_eq!(rejected_code(&error), Some(code), "{code:?}");
        client.expect_quiet(Duration::from_millis(100), false);
        assert_eq!(
            server.stub.emoji.uploads.lock().unwrap().len(),
            1,
            "{code:?}"
        );
    }
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let (workspace, emoji) = (WorkspaceId::generate(), EmojiId::generate());
    server
        .stub
        .emoji
        .plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::ServerError);
    let source = FileSource::Bytes(FileBytes::new(png(10)));
    let error =
        upload_emoji(&mut client, workspace, emoji, "party_time", source.clone()).unwrap_err();
    assert!(matches!(
        error,
        TransferError::Request(RequestError::Unknown(_))
    ));
    client.expect_quiet(Duration::from_millis(100), false);
    assert_eq!(
        server.stub.emoji.uploads.lock().unwrap().len(),
        1,
        "one attempt per effect"
    );
    // The core repeats the same upload; the server sees the same emoji id twice.
    upload_emoji(&mut client, workspace, emoji, "party_time", source).expect("the repeat works");
    let uploads = server.stub.emoji.uploads.lock().unwrap().clone();
    assert_eq!(uploads.len(), 2);
    assert_eq!(uploads[0].emoji, uploads[1].emoji);
}

#[test]
fn a_rename_is_one_patch_with_the_name_and_a_delete_is_repeated_after_a_lost_answer() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let emoji = EmojiId::generate();
    client.execute(vec![Effect::RenameEmoji {
        emoji,
        request: RenameEmojiRequest {
            name: EmojiName::parse("new_name").unwrap(),
        },
    }]);
    let kind = ActionKind::RenameCustomEmoji(emoji);
    let Ok(ActionOutcome::CustomEmoji(renamed)) = action_result(&mut client, kind) else {
        panic!("expected the renamed emoji");
    };
    assert_eq!((renamed.id, renamed.name.as_str()), (emoji, "new_name"));
    let calls = server.stub.emoji.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "PATCH");
    assert_eq!(calls[0].2, serde_json::json!({ "name": "new_name" }));

    // A refusal is final; a server failure is not repeated for a rename.
    server
        .stub
        .emoji
        .plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::Refuse(ErrorCode::NameTaken));
    client.execute(vec![Effect::RenameEmoji {
        emoji,
        request: RenameEmojiRequest {
            name: EmojiName::parse("taken_one").unwrap(),
        },
    }]);
    let error = action_result(&mut client, kind).unwrap_err();
    assert!(matches!(error, RequestError::Rejected(api) if api.code == ErrorCode::NameTaken));
    server
        .stub
        .emoji
        .plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::ServerError);
    client.execute(vec![Effect::RenameEmoji {
        emoji,
        request: RenameEmojiRequest {
            name: EmojiName::parse("flaky_one").unwrap(),
        },
    }]);
    assert!(matches!(
        action_result(&mut client, kind).unwrap_err(),
        RequestError::Unknown(_)
    ));
    assert_eq!(server.stub.emoji.calls.lock().unwrap().len(), 3);

    // Deleting is safe to repeat, so a failure is asked again.
    let delete = ActionKind::DeleteCustomEmoji(emoji);
    server
        .stub
        .emoji
        .plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::ServerError);
    client.execute(vec![Effect::DeleteEmoji { emoji }]);
    assert_eq!(action_result(&mut client, delete), Ok(ActionOutcome::Done));
    let deletes = server
        .stub
        .emoji
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, ..)| method == "DELETE")
        .count();
    assert_eq!(deletes, 2);
    // A refusal is final and not repeated.
    server
        .stub
        .emoji
        .plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::Refuse(ErrorCode::NotFound));
    client.execute(vec![Effect::DeleteEmoji { emoji }]);
    assert!(matches!(
        action_result(&mut client, delete).unwrap_err(),
        RequestError::Rejected(api) if api.code == ErrorCode::NotFound
    ));
}

fn load_emoji(client: &mut Client, emoji: EmojiId) -> Result<ImageData, TransferError> {
    client.execute(vec![Effect::FetchEmojiImage { emoji }]);
    let loaded = client.expect(
        "EmojiImageLoaded",
        |i| matches!(i, Input::EmojiImageLoaded { emoji: e, .. } if *e == emoji),
    );
    let Input::EmojiImageLoaded { result, .. } = loaded else {
        unreachable!()
    };
    result
}

#[test]
fn an_emoji_image_is_fetched_once_then_served_from_the_encrypted_cache_across_a_restart() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let emoji = EmojiId::generate();
    let image = png(4000);
    server
        .stub
        .emoji
        .images
        .lock()
        .unwrap()
        .insert(emoji, image.clone());
    assert_eq!(
        load_emoji(&mut client, emoji).unwrap().bytes(),
        image.as_slice()
    );
    assert_eq!(
        load_emoji(&mut client, emoji).unwrap().bytes(),
        image.as_slice()
    );
    assert_eq!(server.stub.emoji.image_requests.load(Ordering::SeqCst), 1);
    // On disk it is in a cache of its own, and not the picture.
    let names = files_in(&client.dir("emoji"));
    assert_eq!(names.len(), 1, "{names:?}");
    let stored = std::fs::read(client.dir("emoji").join(&names[0])).unwrap();
    assert!(!stored.windows(64).any(|w| w == &image[100..164]));
    assert!(!client.dir("images").exists() || files_in(&client.dir("images")).is_empty());
    let mut client = client.restart(&server);
    assert_eq!(
        load_emoji(&mut client, emoji).unwrap().bytes(),
        image.as_slice()
    );
    assert_eq!(server.stub.emoji.image_requests.load(Ordering::SeqCst), 1);
}

#[test]
fn a_flaky_emoji_read_is_retried_a_missing_one_is_refused_and_a_damaged_entry_is_fetched_again() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let emoji = EmojiId::generate();
    server
        .stub
        .emoji
        .images
        .lock()
        .unwrap()
        .insert(emoji, png(500));
    server
        .stub
        .emoji
        .image_plan
        .lock()
        .unwrap()
        .push_back(EmojiBehavior::ServerError);
    assert!(load_emoji(&mut client, emoji).is_ok());
    assert_eq!(server.stub.emoji.image_requests.load(Ordering::SeqCst), 2);
    let gone = load_emoji(&mut client, EmojiId::generate()).unwrap_err();
    assert_eq!(rejected_code(&gone), Some(ErrorCode::NotFound));
    // Damage the cached copy, restart, and it is fetched again without a word.
    let mut client = client.restart(&server);
    let names = files_in(&client.dir("emoji"));
    let path = client.dir("emoji").join(&names[0]);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&path, bytes).unwrap();
    let before = server.stub.emoji.image_requests.load(Ordering::SeqCst);
    assert!(load_emoji(&mut client, emoji).is_ok());
    assert_eq!(
        server.stub.emoji.image_requests.load(Ordering::SeqCst),
        before + 1
    );
    client.expect_quiet(Duration::from_millis(100), false);
}

#[test]
fn forgetting_a_deleted_emojis_images_and_signing_out_clear_the_emoji_cache() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let emoji = EmojiId::generate();
    server
        .stub
        .emoji
        .images
        .lock()
        .unwrap()
        .insert(emoji, png(300));
    load_emoji(&mut client, emoji).unwrap();
    assert_eq!(files_in(&client.dir("emoji")).len(), 1);
    client.execute(vec![Effect::ForgetEmojiImages {
        emojis: vec![emoji],
    }]);
    wait_until("the cached emoji image to go", || {
        files_in(&client.dir("emoji")).is_empty()
    });
    load_emoji(&mut client, emoji).unwrap();
    assert_eq!(server.stub.emoji.image_requests.load(Ordering::SeqCst), 2);
    client.execute(vec![Effect::SignOut]);
    wait_until("the emoji cache to go", || !client.dir("emoji").exists());
}

#[allow(dead_code)]
fn unused(_: WorkspaceId, _: AtomicUsize) {}

// ----- Search (slice 3C, D19) -----

fn params(q: &str) -> SearchParams {
    SearchParams {
        q: q.to_owned(),
        limit: Some(20),
        offset: Some(0),
        sort: Some(SearchSort::Relevance),
    }
}

fn search_calls(server: &Server) -> Vec<SearchCall> {
    server.stub.search.calls.lock().unwrap().clone()
}

fn plan_search(server: &Server, behavior: SearchBehavior) {
    server.stub.search.plan.lock().unwrap().push_back(behavior);
}

#[test]
fn a_search_is_one_authenticated_get_with_every_character_of_the_query_encoded() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let workspace = WorkspaceId::generate();
    let queries = [
        "deploy in:ops",
        "\"quoted phrase\" -draft",
        "fish & chips = 100% #1 +2",
        "日本語 😀 café",
        "a/b?c=d;e",
        "  padded  ",
        "x=1&limit=50",
    ];
    for (n, text) in queries.iter().enumerate() {
        let request = SearchParams {
            q: (*text).to_owned(),
            limit: Some(20),
            offset: Some(40),
            sort: Some(SearchSort::Recent),
        };
        let response = client.search(workspace, n as u64 + 1, request).unwrap();
        assert_eq!(
            response.hits[0].snippet.text, *text,
            "the server read what was typed"
        );
        assert!(response.has_more, "the answer decoded");

        let call = search_calls(&server).pop().unwrap();
        assert_eq!(call.workspace, workspace);
        assert!(call.authorized, "carries the session");
        assert_eq!(call.params["q"], *text);
        assert_eq!(call.params["limit"], "20");
        assert_eq!(call.params["offset"], "40");
        assert_eq!(call.params["sort"], "recent");
        assert_eq!(call.params.len(), 4, "{:?}", call.params);
        // Nothing outside the unreserved set travels as itself.
        assert!(
            call.raw_query
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "%+*-._=&".contains(c)),
            "{}",
            call.raw_query
        );
        assert_eq!(call.raw_query.split('&').count(), 4, "{}", call.raw_query);
    }
    let call = search_calls(&server)[2].clone();
    for encoded in ["%26", "%3D", "%25", "%23", "%2B"] {
        assert!(call.raw_query.contains(encoded), "{}", call.raw_query);
    }
    assert_eq!(
        search_calls(&server).len(),
        queries.len(),
        "one request each"
    );
}

#[test]
fn what_the_core_does_not_set_is_not_sent() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let request = SearchParams {
        q: "hello world".to_owned(),
        limit: None,
        offset: None,
        sort: None,
    };
    client.search(WorkspaceId::generate(), 1, request).unwrap();
    assert_eq!(search_calls(&server)[0].raw_query, "q=hello+world");
}

#[test]
fn a_search_that_took_too_long_is_the_servers_refusal_and_is_not_asked_again() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    plan_search(&server, SearchBehavior::Refuse(ErrorCode::Unavailable));
    let error = client
        .search(WorkspaceId::generate(), 1, params("slow"))
        .unwrap_err();
    assert!(
        matches!(&error, RequestError::Rejected(e) if e.code == ErrorCode::Unavailable),
        "{error:?}"
    );
    assert_eq!(
        search_calls(&server).len(),
        1,
        "it would only time out again"
    );
}

#[test]
fn refusals_of_a_search_are_final_and_a_failure_is_asked_again() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let workspace = WorkspaceId::generate();
    for code in [
        ErrorCode::InvalidRequest,
        ErrorCode::Forbidden,
        ErrorCode::NotFound,
        ErrorCode::RateLimited,
        ErrorCode::Unauthenticated,
    ] {
        let before = search_calls(&server).len();
        plan_search(&server, SearchBehavior::Refuse(code));
        let error = client.search(workspace, 1, params("x")).unwrap_err();
        assert!(
            matches!(&error, RequestError::Rejected(e) if e.code == code),
            "{code:?}: {error:?}"
        );
        assert_eq!(search_calls(&server).len(), before + 1, "{code:?}");
    }

    // A server failure is read again, as any read is.
    let before = search_calls(&server).len();
    plan_search(&server, SearchBehavior::ServerError);
    plan_search(&server, SearchBehavior::ServerError);
    let response = client.search(workspace, 2, params("again")).unwrap();
    assert_eq!(response.hits[0].snippet.text, "again");
    assert_eq!(search_calls(&server).len(), before + 3);

    // Until the attempts run out.
    let before = search_calls(&server).len();
    for _ in 0..3 {
        plan_search(&server, SearchBehavior::ServerError);
    }
    let error = client.search(workspace, 3, params("never")).unwrap_err();
    assert!(matches!(error, RequestError::Unknown(_)), "{error:?}");
    assert_eq!(search_calls(&server).len(), before + 3);
}

#[test]
fn a_search_without_a_session_is_the_unauthenticated_refusal() {
    let server = Server::start();
    let mut client = Client::start(&server);
    let error = client
        .search(WorkspaceId::generate(), 1, params("x"))
        .unwrap_err();
    assert!(
        matches!(&error, RequestError::Rejected(e) if e.code == ErrorCode::Unauthenticated),
        "{error:?}"
    );
    assert!(!search_calls(&server)[0].authorized);
}

#[test]
fn a_newer_search_hangs_up_on_the_older_and_only_the_newer_is_reported() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let workspace = WorkspaceId::generate();
    plan_search(&server, SearchBehavior::Slow(Duration::from_secs(10)));
    client.execute(vec![Effect::Search {
        workspace,
        generation: 1,
        params: params("old"),
    }]);
    wait_until("the first search to arrive", || {
        search_calls(&server).len() == 1
    });

    let response = client.search(workspace, 2, params("new")).unwrap();
    assert_eq!(response.hits[0].snippet.text, "new");
    // Promptly: the client's own timeout is seconds away.
    wait_within(
        "the hang-up on the older search",
        Duration::from_secs(2),
        || server.stub.search.aborted.load(Ordering::SeqCst) == 1,
    );
    // Nothing is reported for the older one.
    client.expect_quiet(Duration::from_millis(300), true);
}

#[test]
fn cancelling_and_signing_out_stop_a_search_without_a_report() {
    let server = Server::start();
    let mut client = Client::logged_in(&server);
    let workspace = WorkspaceId::generate();

    plan_search(&server, SearchBehavior::Slow(Duration::from_secs(10)));
    client.execute(vec![Effect::Search {
        workspace,
        generation: 1,
        params: params("a"),
    }]);
    wait_until("the search to arrive", || search_calls(&server).len() == 1);
    client.execute(vec![Effect::CancelSearch]);
    wait_within("the hang-up", Duration::from_secs(2), || {
        server.stub.search.aborted.load(Ordering::SeqCst) == 1
    });
    client.expect_quiet(Duration::from_millis(300), true);

    // The runtime is not left stuck: the next search is answered.
    let response = client.search(workspace, 2, params("b")).unwrap();
    assert_eq!(response.hits[0].snippet.text, "b");

    // Cancelling when nothing is out is harmless.
    client.execute(vec![Effect::CancelSearch]);

    plan_search(&server, SearchBehavior::Slow(Duration::from_secs(10)));
    client.execute(vec![Effect::Search {
        workspace,
        generation: 3,
        params: params("c"),
    }]);
    wait_until("the third search to arrive", || {
        search_calls(&server).len() == 3
    });
    client.execute(vec![Effect::SignOut]);
    wait_within("the hang-up at sign-out", Duration::from_secs(2), || {
        server.stub.search.aborted.load(Ordering::SeqCst) == 2
    });
    client.expect_quiet(Duration::from_millis(300), true);
}
