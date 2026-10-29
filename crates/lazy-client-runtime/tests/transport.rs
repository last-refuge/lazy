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
    ActionKind, ActionOutcome, CachedTimeline, CachedWorkspace, ContentCache, Effect, FileSource,
    Input, JournalEntry, JournalKind, RequestError, SocketEvent, TransferError,
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
    ErrorCode, Event, EventEnvelope, HistoryPage, LoginRequest, Message, OpenConversationRequest,
    OperationStatus, PROTOCOL_VERSION, SendMessageRequest, SendMessageResponse, ServerFrame,
    SessionResponse, SessionToken, ThreadStateResponse, ThreadUserState, UpdateConversationRequest,
    User, UserStateResponse, Webhook, WebhooksResponse, Workspace, WorkspaceBootstrap,
};
use tempfile::TempDir;

// ----- Stub server -----

#[derive(Clone, Copy)]
enum SendBehavior {
    Ok,
    Conflict,
    ServerError,
    Malformed,
    /// A well-formed JSON body of the wrong shape that quotes a string, as a misbehaving server
    /// or proxy might: the parser's own complaint would repeat the string.
    MalformedEcho,
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

/// What `/v1/messages/{id}` does with the next request.
#[derive(Clone, Copy)]
enum MessageBehavior {
    Ok,
    Stale,
    NotFound,
    LimitReached,
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
    message_plan: Mutex<VecDeque<MessageBehavior>>,
    /// The body of the latest send, as the server received it.
    last_send: Mutex<Option<serde_json::Value>>,
    /// When set, the operation-status route answers `edit_committed` with this message.
    edit_status: Mutex<Option<Message>>,
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
    /// The body of every registration, as the server received it.
    registrations: Mutex<Vec<serde_json::Value>>,
    /// A text frame (that is not a frame) the realtime route sends after the catch-up.
    bad_frame: Mutex<Option<String>>,
    /// What `/healthz` answers.
    health: Mutex<HealthMode>,
}

/// What the stub's `/healthz` does.
#[derive(Clone)]
enum HealthMode {
    /// Answers with this protocol version.
    Version(u32),
    /// Answers 200 with something that is not JSON.
    NotJson,
    /// Answers 500.
    Error,
    /// Redirects to this address.
    RedirectTo(String),
}

impl Stub {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            user: User {
                id: UserId::generate(),
                username: Username::parse("stubby").unwrap(),
                display_name: DisplayName::parse("Stubby").unwrap(),
                is_bot: false,
                status: None,
                deactivated: false,
                avatar_version: 0,
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
            registrations: Mutex::new(Vec::new()),
            bad_frame: Mutex::new(None),
            health: Mutex::new(HealthMode::Version(PROTOCOL_VERSION)),
            calls: Mutex::new(Vec::new()),
            api_plan: Mutex::new(VecDeque::new()),
            message_plan: Mutex::new(VecDeque::new()),
            last_send: Mutex::new(None),
            edit_status: Mutex::new(None),
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
            edited_at: None,
            thread_root: None,
            in_channel: false,
            thread: None,
            reactions: Vec::new(),
            reactions_as_of: EventSeq::ZERO,
            mentions: lazy_protocol::Mentions::default(),
            deleted: false,
            attachments: Vec::new(),
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

async fn register(
    State(s): State<Arc<Stub>>,
    Json(body): Json<serde_json::Value>,
) -> Json<SessionResponse> {
    s.registrations.lock().unwrap().push(body);
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
                settings: Default::default(),
            },
            role: WorkspaceRole::Member,
            conversations: vec![conversation(s.workspace, "general")],
            user_state: Vec::new(),
            groups: Vec::new(),
            custom_emoji: Vec::new(),
            threads: Vec::new(),
            notification_settings: Default::default(),
            conversation_notification_prefs: Vec::new(),
            saved: Vec::new(),
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
    *s.last_send.lock().unwrap() = serde_json::to_value(&request).ok();
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
        SendBehavior::MalformedEcho => {
            Json(serde_json::json!({ "message": LEAKY_WORDS })).into_response()
        }
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
    if let Some(message) = s.edit_status.lock().unwrap().clone() {
        return Json(OperationStatus::EditCommitted { message }).into_response();
    }
    Json(OperationStatus::NotFound).into_response()
}

/// `GET`, `PATCH`, and `DELETE` on `/v1/messages/{id}`.
async fn message_api(
    State(s): State<Arc<Stub>>,
    Path(id): Path<MessageId>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_ends_with_reactions = uri.path().ends_with("/reactions");
    let path = uri.path().to_owned();
    s.record(&path, &headers);
    let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path,
        body: body.clone(),
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::Stale => return refuse(StatusCode::CONFLICT, ErrorCode::StaleRevision),
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::LimitReached => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::LimitReached);
        }
        MessageBehavior::NameTaken => return refuse(StatusCode::CONFLICT, ErrorCode::NameTaken),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        MessageBehavior::Ok => {}
    }
    let mut message = s.message(ConversationId::generate(), 5, "the message");
    message.id = id;
    if path_ends_with_reactions {
        let request: lazy_protocol::SetReactionRequest = serde_json::from_value(body).unwrap();
        message.reactions = if request.present {
            vec![lazy_protocol::ReactionSummary {
                emoji: request.emoji,
                users: vec![s.user.id],
            }]
        } else {
            Vec::new()
        };
        message.reactions_as_of = EventSeq::new(12).unwrap();
        return Json(lazy_protocol::MessageResponse { message }).into_response();
    }
    match method.as_str() {
        "PATCH" => {
            let request: lazy_protocol::EditMessageRequest = serde_json::from_value(body).unwrap();
            message.document = request.document;
            message.revision = request.base_revision + 1;
            message.edited_at = Some(Timestamp::now());
        }
        "DELETE" => {
            message.document = lazy_domain::message::deleted_placeholder();
            message.revision = 2;
            message.deleted = true;
        }
        _ => {}
    }
    Json(lazy_protocol::MessageResponse { message }).into_response()
}

/// The address the stub puts in a created webhook. A test checks that it never shows up in a
/// debug string.
const STUB_HOOK_SECRET: &str = "stub-secret-value-0123456789";

/// The webhook routes: create, list, and delete.
async fn webhook_api(
    State(s): State<Arc<Stub>>,
    Path(id): Path<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: body.clone(),
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::LimitReached => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::LimitReached);
        }
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    let webhook = |conversation: ConversationId, name: &str| Webhook {
        id: lazy_domain::WebhookId::generate(),
        conversation_id: conversation,
        name: DisplayName::parse(name).unwrap(),
        bot_user_id: UserId::generate(),
        created_by: s.user.id,
        created_at: Timestamp::now(),
    };
    // The path names a conversation for creating and listing, a webhook for deleting.
    let conversation: ConversationId = serde_json::from_value(serde_json::Value::String(id))
        .unwrap_or_else(|_| ConversationId::generate());
    match method {
        Method::POST => {
            let name = body["name"].as_str().unwrap_or("hook");
            let webhook = webhook(conversation, name);
            let url = format!("http://stub/hooks/{}/{STUB_HOOK_SECRET}", webhook.id);
            Json(lazy_protocol::CreatedWebhook { webhook, url }).into_response()
        }
        Method::GET => Json(WebhooksResponse {
            webhooks: vec![webhook(conversation, "Deploys")],
        })
        .into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

/// A code the stub's invitations carry. It must never appear in anything the runtime prints.
const STUB_INVITE_CODE: &str = "stub-invite-code-0123456789ab";

/// The workspace administration routes (slice 3F): the answers are well-formed and repeat what
/// was sent. `message_plan` decides refusals, as for the webhook routes.
async fn admin_api(
    State(s): State<Arc<Stub>>,
    Path(params): Path<std::collections::HashMap<String, String>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: body.clone(),
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::LimitReached => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::LimitReached);
        }
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    let workspace = s.workspace;
    let invitation = |id: lazy_domain::InvitationId, state: lazy_protocol::InvitationState| {
        lazy_protocol::Invitation {
            id,
            workspace_id: workspace,
            created_by: s.user.id,
            created_at: Timestamp::now(),
            expires_at: Timestamp::now(),
            max_uses: body["max_uses"].as_u64().map(|n| n as u32),
            uses: 0,
            state,
        }
    };
    let path = uri.path();
    if path.ends_with("/role") || path.ends_with("/deactivated") {
        let user: UserId = params
            .get("user_id")
            .and_then(|text| serde_json::from_value(serde_json::Value::String(text.clone())).ok())
            .unwrap_or_else(UserId::generate);
        let mut person = lazy_protocol::Person {
            user: s.user.clone(),
            role: WorkspaceRole::Member,
            joined_at: Timestamp::now(),
        };
        person.user.id = user;
        if let Some(role) = body.get("role") {
            person.role = serde_json::from_value(role.clone()).unwrap();
        }
        if let Some(deactivated) = body["deactivated"].as_bool() {
            person.user.deactivated = deactivated;
        }
        return Json(lazy_protocol::PersonResponse { person }).into_response();
    }
    if path.contains("/workspaces/") && path.ends_with("/invitations") {
        return match method {
            Method::POST => Json(lazy_protocol::CreatedInvitation {
                invitation: invitation(
                    lazy_domain::InvitationId::generate(),
                    lazy_protocol::InvitationState::Active,
                ),
                code: lazy_protocol::InviteCode::new(STUB_INVITE_CODE),
            })
            .into_response(),
            _ => Json(lazy_protocol::InvitationList {
                invitations: vec![invitation(
                    lazy_domain::InvitationId::generate(),
                    lazy_protocol::InvitationState::Active,
                )],
            })
            .into_response(),
        };
    }
    if path.starts_with("/v1/invitations/") {
        let id: lazy_domain::InvitationId = params
            .get("invitation_id")
            .and_then(|text| serde_json::from_value(serde_json::Value::String(text.clone())).ok())
            .unwrap_or_else(lazy_domain::InvitationId::generate);
        return Json(lazy_protocol::InvitationResponse {
            invitation: invitation(id, lazy_protocol::InvitationState::Revoked),
        })
        .into_response();
    }
    // `PATCH /v1/workspaces/{id}`.
    let mut workspace = Workspace {
        id: s.workspace,
        name: "Stub".into(),
        settings: Default::default(),
    };
    if let Some(name) = body["name"].as_str() {
        workspace.name = name.into();
    }
    if let Some(settings) = body.get("settings") {
        workspace.settings = serde_json::from_value(settings.clone()).unwrap();
    }
    Json(lazy_protocol::WorkspaceResponse { workspace }).into_response()
}

/// The routes of slice 3G: creating and joining workspaces, the person's profile, and avatars. The
/// answers are well-formed and repeat what was sent; `message_plan` decides refusals.
async fn profile_api(
    State(s): State<Arc<Stub>>,
    Path(params): Path<std::collections::HashMap<String, String>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let is_raw = uri.path() == "/v1/me/avatar" && method == Method::PUT;
    let json: serde_json::Value = if is_raw {
        serde_json::json!({
            "len": body.len(),
            "content_type": headers.get("content-type").and_then(|v| v.to_str().ok()),
        })
    } else {
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
    };
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: format!(
            "{}{}",
            uri.path(),
            uri.query().map(|q| format!("?{q}")).unwrap_or_default()
        ),
        body: json.clone(),
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::LimitReached => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::LimitReached);
        }
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    let path = uri.path();
    let joined = |id: WorkspaceId, name: &str| {
        let workspace = Workspace {
            id,
            name: name.into(),
            settings: Default::default(),
        };
        Json(lazy_protocol::JoinedWorkspace {
            workspace: WorkspaceBootstrap {
                workspace,
                role: WorkspaceRole::Owner,
                conversations: vec![conversation(id, "general")],
                user_state: Vec::new(),
                groups: Vec::new(),
                custom_emoji: Vec::new(),
                notification_settings: Default::default(),
                conversation_notification_prefs: Vec::new(),
                threads: Vec::new(),
                saved: Vec::new(),
                members: vec![s.user.clone()],
                head: EventSeq::new(3).unwrap(),
            },
        })
        .into_response()
    };
    if path.starts_with("/v1/workspaces/") {
        let id: WorkspaceId = params
            .get("workspace_id")
            .and_then(|text| serde_json::from_value(serde_json::Value::String(text.clone())).ok())
            .unwrap_or_else(WorkspaceId::generate);
        return joined(id, json["name"].as_str().unwrap_or("Made"));
    }
    if path == "/v1/invitations/accept" {
        return joined(WorkspaceId::generate(), "Joined");
    }
    if path.starts_with("/v1/users/") {
        let version: u32 = uri
            .query()
            .and_then(|q| q.strip_prefix("v="))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut png = b"\x89PNG-avatar-".to_vec();
        png.extend_from_slice(version.to_string().as_bytes());
        return (StatusCode::OK, png).into_response();
    }
    // `/v1/me`, `/v1/me/avatar`
    let mut user = s.user.clone();
    if let Some(name) = json["display_name"].as_str() {
        user.display_name = DisplayName::parse(name).unwrap();
    }
    user.avatar_version = u32::from(method == Method::PUT && path.ends_with("/avatar"));
    Json(lazy_protocol::ProfileResponse { user }).into_response()
}

/// The notification, status, and inbox routes (slice 3D): the answer repeats what was sent.
async fn notify_api(
    State(s): State<Arc<Stub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: if method == Method::GET {
            serde_json::json!({ "query": uri.query() })
        } else {
            body.clone()
        },
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    let path = uri.path();
    if path.ends_with("/notification-settings") {
        Json(serde_json::json!({ "settings": body })).into_response()
    } else if path.ends_with("/notification-prefs") {
        let conversation = path.split('/').nth(3).unwrap_or_default();
        Json(serde_json::json!({ "prefs": {
            "conversation_id": conversation,
            "level": body["level"],
            "muted": body["muted"],
        } }))
        .into_response()
    } else if path.ends_with("/status") {
        let mut user = serde_json::to_value(&s.user).unwrap();
        user["status"] = body["status"].clone();
        Json(serde_json::json!({ "user": user })).into_response()
    } else if path.ends_with("/saved") && method == Method::PUT {
        let message = path.split('/').nth(3).unwrap_or_default();
        Json(serde_json::json!({ "message_id": message, "saved": body["saved"] })).into_response()
    } else if path.ends_with("/saved") {
        Json(serde_json::json!({ "items": [], "has_more": true })).into_response()
    } else if path.ends_with("/people") {
        Json(serde_json::json!({ "people": [], "has_more": true })).into_response()
    } else if path.contains("/people/") {
        Json(serde_json::json!({ "person": {
            "user": serde_json::to_value(&s.user).unwrap(),
            "role": "admin",
            "joined_at": serde_json::to_value(lazy_domain::Timestamp::now()).unwrap(),
        } }))
        .into_response()
    } else if path.ends_with("/inbox/read") {
        Json(serde_json::json!({ "updated": 7 })).into_response()
    } else {
        Json(serde_json::json!({ "entries": [], "has_more": true })).into_response()
    }
}

/// The thread routes: a page of replies, following, and marking read.
async fn thread_api(
    State(s): State<Arc<Stub>>,
    Path(root): Path<MessageId>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        // A page is asked for with a query; keep it where the tests can see it.
        body: if method == Method::GET {
            serde_json::json!({ "query": uri.query() })
        } else {
            body.clone()
        },
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    let conversation = ConversationId::generate();
    let state = |following: bool, last_read: i64| ThreadStateResponse {
        state: ThreadUserState {
            root_id: root,
            conversation_id: conversation,
            following,
            last_read: EventSeq::new(last_read).unwrap(),
            unread_count: 0,
            mention_count: 0,
        },
    };
    match method {
        Method::GET => {
            let mut reply = s.message(conversation, 21, "a reply");
            reply.thread_root = Some(root);
            Json(HistoryPage {
                messages: vec![reply],
                has_more: true,
            })
            .into_response()
        }
        Method::PUT => Json(state(body["following"].as_bool().unwrap_or(false), 0)).into_response(),
        _ => Json(state(true, body["up_to"].as_i64().unwrap_or(0))).into_response(),
    }
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
                let bad = stub.bad_frame.lock().unwrap().clone();
                if let Some(bad) = bad
                    && socket.send(WsMessage::Text(bad.into())).await.is_err()
                {
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
        // A page is asked for with a query; keep it where the tests can see it.
        body: if method == Method::GET && uri.query().is_some() {
            serde_json::json!({ "query": uri.query() })
        } else {
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null)
        },
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
            has_more: true,
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

/// `POST /v1/workspaces/{id}/groups`, and `PATCH`/`DELETE` on `/v1/groups/{id}`.
async fn group_api(
    State(s): State<Arc<Stub>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    s.record(uri.path(), &headers);
    let body = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: body.clone(),
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    let refuse = |status: StatusCode, code: ErrorCode| {
        (status, Json(ApiError::new(code, "no"))).into_response()
    };
    match behavior {
        MessageBehavior::NameTaken => return refuse(StatusCode::CONFLICT, ErrorCode::NameTaken),
        MessageBehavior::LimitReached => {
            return refuse(StatusCode::UNPROCESSABLE_ENTITY, ErrorCode::LimitReached);
        }
        MessageBehavior::Forbidden => return refuse(StatusCode::FORBIDDEN, ErrorCode::Forbidden),
        MessageBehavior::NotFound => return refuse(StatusCode::NOT_FOUND, ErrorCode::NotFound),
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        MessageBehavior::Ok | MessageBehavior::Stale => {}
    }
    let group = |handle: &str, name: &str, members: &serde_json::Value| lazy_protocol::UserGroup {
        id: lazy_domain::GroupId::generate(),
        workspace_id: s.workspace,
        handle: lazy_domain::GroupHandle::parse(handle).unwrap(),
        name: DisplayName::parse(name).unwrap(),
        members: serde_json::from_value(members.clone()).unwrap_or_default(),
    };
    match method.as_str() {
        "POST" => Json(lazy_protocol::GroupResponse {
            group: group(
                body["handle"].as_str().unwrap_or("group"),
                body["name"].as_str().unwrap_or("Group"),
                &body["members"],
            ),
        })
        .into_response(),
        "PATCH" => Json(lazy_protocol::GroupResponse {
            group: group(
                body["handle"].as_str().unwrap_or("changed"),
                body["name"].as_str().unwrap_or("Changed"),
                &body["members"],
            ),
        })
        .into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

/// `GET /v1/conversations/{id}/members`: who is in the conversation.
async fn members_api(
    State(s): State<Arc<Stub>>,
    Path(conversation): Path<ConversationId>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
) -> Response {
    s.record(uri.path(), &headers);
    s.calls.lock().unwrap().push(Call {
        method: method.to_string(),
        path: uri.path().to_owned(),
        body: serde_json::Value::Null,
    });
    let behavior = s
        .message_plan
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(MessageBehavior::Ok);
    match behavior {
        MessageBehavior::NotFound => {
            return (
                StatusCode::NOT_FOUND,
                Json(ApiError::new(ErrorCode::NotFound, "no")),
            )
                .into_response();
        }
        MessageBehavior::ServerError => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        _ => {}
    }
    Json(lazy_protocol::ConversationMembersResponse {
        conversation_id: conversation,
        user_ids: vec![s.user.id],
    })
    .into_response()
}

async fn health(State(s): State<Arc<Stub>>, headers: HeaderMap) -> Response {
    s.record("GET healthz", &headers);
    let mode = s.health.lock().unwrap().clone();
    match mode {
        HealthMode::Version(protocol_version) => {
            Json(lazy_protocol::HealthResponse { protocol_version }).into_response()
        }
        HealthMode::NotJson => (StatusCode::OK, "<html>welcome</html>").into_response(),
        HealthMode::Error => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        HealthMode::RedirectTo(to) => (StatusCode::FOUND, [("location", to)]).into_response(),
    }
}

/// Sends the client to `/healthz` on the same host.
async fn same_host_redirect() -> Response {
    (StatusCode::FOUND, [("location", "/healthz")]).into_response()
}

fn router(stub: Arc<Stub>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/lazy/healthz", get(health))
        .route("/redirected/healthz", get(same_host_redirect))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/register", post(register))
        .route("/v1/bootstrap", get(bootstrap))
        .route(
            "/v1/conversations/{conversation_id}/messages",
            post(send_message),
        )
        .route("/v1/operations/{operation_id}", get(operation_status))
        .route(
            "/v1/messages/{message_id}",
            get(message_api).patch(message_api).delete(message_api),
        )
        .route(
            "/v1/messages/{message_id}/reactions",
            axum::routing::put(message_api),
        )
        .route(
            "/v1/conversations/{conversation_id}/webhooks",
            get(webhook_api).post(webhook_api),
        )
        .route(
            "/v1/webhooks/{webhook_id}",
            axum::routing::delete(webhook_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}",
            axum::routing::patch(admin_api).put(profile_api),
        )
        .route("/v1/invitations/accept", post(profile_api))
        .route("/v1/me", axum::routing::patch(profile_api))
        .route(
            "/v1/me/avatar",
            axum::routing::put(profile_api).delete(profile_api),
        )
        .route("/v1/users/{user_id}/avatar", get(profile_api))
        .route(
            "/v1/workspaces/{workspace_id}/people/{user_id}/role",
            axum::routing::put(admin_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/people/{user_id}/deactivated",
            axum::routing::put(admin_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/invitations",
            get(admin_api).post(admin_api),
        )
        .route(
            "/v1/invitations/{invitation_id}",
            axum::routing::delete(admin_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/notification-settings",
            axum::routing::put(notify_api),
        )
        .route(
            "/v1/conversations/{conversation_id}/notification-prefs",
            axum::routing::put(notify_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/status",
            axum::routing::put(notify_api),
        )
        .route("/v1/workspaces/{workspace_id}/inbox", get(notify_api))
        .route(
            "/v1/messages/{message_id}/saved",
            axum::routing::put(notify_api),
        )
        .route("/v1/workspaces/{workspace_id}/saved", get(notify_api))
        .route("/v1/workspaces/{workspace_id}/people", get(notify_api))
        .route(
            "/v1/workspaces/{workspace_id}/people/{user_id}",
            get(notify_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/inbox/read",
            axum::routing::post(notify_api),
        )
        .route("/v1/messages/{message_id}/replies", get(thread_api))
        .route(
            "/v1/messages/{message_id}/thread/follow",
            axum::routing::put(thread_api),
        )
        .route(
            "/v1/messages/{message_id}/thread/read",
            axum::routing::post(thread_api),
        )
        .route(
            "/v1/workspaces/{workspace_id}/groups",
            axum::routing::post(group_api),
        )
        .route(
            "/v1/groups/{group_id}",
            axum::routing::patch(group_api).delete(group_api),
        )
        .route(
            "/v1/conversations/{conversation_id}/members",
            get(members_api).fallback(conversation_api),
        )
        .route("/v1/realtime", get(realtime))
        .fallback(conversation_api)
        .with_state(stub)
}

// ----- Log capture -----

/// Words the logging test types everywhere and then looks for in everything that was logged.
const LEAKY_WORDS: &str = "quokka zephyr";

/// A tracing subscriber that keeps every event as one line of text, so a test can read what the
/// runtime logged. Installed for the whole process, so only the logging scenario uses it, in a
/// process of its own.
mod capture {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, OnceLock};

    use tracing::field::{Field, Visit};
    use tracing::span::{Attributes, Id, Record};
    use tracing::{Event, Metadata, Subscriber};

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// Every event so far as `LEVEL target: message field=value ...`.
    pub fn lines() -> Vec<String> {
        LINES.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn install() {
        static ONCE: OnceLock<()> = OnceLock::new();
        ONCE.get_or_init(|| {
            tracing::subscriber::set_global_default(Capture::default())
                .expect("no other tracing subscriber is installed in this process");
        });
    }

    #[derive(Default)]
    struct Capture {
        next_span: AtomicU64,
    }

    struct Fields<'a>(&'a mut String);

    impl Visit for Fields<'_> {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write as _;
            let _ = write!(self.0, " {}={:?}", field.name(), value);
        }
    }

    fn describe(meta: &Metadata<'_>, record: impl FnOnce(&mut Fields<'_>)) -> String {
        let mut line = format!("{} {}:", meta.level(), meta.target());
        record(&mut Fields(&mut line));
        line
    }

    impl Subscriber for Capture {
        fn enabled(&self, _: &Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attributes: &Attributes<'_>) -> Id {
            let line = describe(attributes.metadata(), |fields| attributes.record(fields));
            LINES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("span {line}"));
            Id::from_u64(self.next_span.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let line = describe(event.metadata(), |fields| event.record(fields));
            LINES.lock().unwrap_or_else(|e| e.into_inner()).push(line);
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }
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
        server_locked: true,
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
            if let Ok(Ok(input)) = &received {
                // What a host would log of everything the runtime hands it.
                tracing::trace!(?input, "input received");
            }
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
        kind: JournalKind::Send,
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
    }
}

fn send_effect(operation: OperationId) -> Effect {
    Effect::SendMessage {
        operation,
        conversation: ConversationId::generate(),
        document: MessageDocument::from_plain_text("hi").unwrap(),
        thread_root: None,
        also_in_channel: false,
        attachments: Vec::new(),
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
    // Nothing listens on port 1 (it is outside the range the system hands out to connections, so no
    // other test's socket can ever be there). A port that was bound and then dropped, as this once
    // used, could be taken by another test's server between the drop and the connect.
    let dead_url = "http://127.0.0.1:1".to_owned();
    if std::net::TcpStream::connect(("127.0.0.1", 1)).is_ok() {
        eprintln!("skipped: something listens on 127.0.0.1:1 here");
        return;
    }
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
        is_bot: false,
        status: None,
        deactivated: false,
        avatar_version: 0,
    };
    let workspace = Workspace {
        id: WorkspaceId::generate(),
        name: "Cached workspace".into(),
        settings: Default::default(),
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
                edited_at: None,
                thread_root: None,
                in_channel: false,
                thread: None,
                reactions: Vec::new(),
                reactions_as_of: EventSeq::ZERO,
                mentions: lazy_protocol::Mentions::default(),
                deleted: false,
                attachments: Vec::new(),
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
            groups: Vec::new(),
            custom_emoji: Vec::new(),
            threads: Vec::new(),
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
        Effect::LoadBrowse {
            workspace,
            generation: 7,
            query: lazy_protocol::BrowseChannelsQuery {
                query: Some("des gn".into()),
                joined: Some(false),
                include_archived: None,
                sort: Some(lazy_protocol::ChannelSort::Members),
                offset: Some(50),
                limit: Some(50),
            },
        },
        |i| matches!(i, Input::BrowseLoaded { .. }),
    );
    let Input::BrowseLoaded {
        generation,
        result: Ok(response),
    } = input
    else {
        panic!("expected the directory");
    };
    assert_eq!(generation, 7, "the generation is echoed");
    assert_eq!(response.channels.len(), 1);
    assert!(response.has_more);
    let query = call.body["query"].as_str().unwrap_or_default();
    for part in [
        "query=des+gn",
        "joined=false",
        "sort=members",
        "offset=50",
        "limit=50",
    ] {
        assert!(query.contains(part), "{query}");
    }
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
        Effect::LoadBrowse {
            workspace,
            generation: 1,
            query: Default::default(),
        },
        |i| matches!(i, Input::BrowseLoaded { .. }),
    );
    assert!(matches!(input, Input::BrowseLoaded { result: Ok(_), .. }));
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

// ----- Editing, fetching, and deleting messages -----

fn edit_effect(
    message: MessageId,
    operation: OperationId,
    base_revision: u32,
    text: &str,
) -> Effect {
    Effect::EditMessage {
        message,
        request: lazy_protocol::EditMessageRequest {
            operation_id: operation,
            base_revision,
            document: MessageDocument::from_markdown(text).unwrap(),
        },
    }
}

fn edit_result(operation: OperationId) -> impl Fn(&Input) -> bool {
    move |input| matches!(input, Input::EditResult { operation: o, .. } if *o == operation)
}

#[test]
fn an_edit_is_one_authenticated_patch_with_the_documented_body() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let message = MessageId::generate();
    let operation = OperationId::generate();
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        edit_effect(message, operation, 3, "now *bold*"),
        edit_result(operation),
    );
    let Input::EditResult {
        result: Ok(response),
        ..
    } = input
    else {
        panic!("expected a committed edit, got {input:?}");
    };
    assert_eq!(response.message.id, message);
    assert_eq!(response.message.revision, 4);
    assert_eq!(
        response.message.document,
        MessageDocument::from_markdown("now *bold*").unwrap()
    );
    assert!(response.message.edited_at.is_some());
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PATCH", format!("/v1/messages/{message}").as_str())
    );
    assert_eq!(call.body["operation_id"], operation.to_string());
    assert_eq!(call.body["base_revision"], 3);
    assert_eq!(call.body["document"]["version"], 2);
    assert_eq!(call_count(&server), before + 1);
    let requests = server.stub.requests.lock().unwrap();
    let (_, auth) = requests
        .iter()
        .rev()
        .find(|(path, _)| path.starts_with("/v1/messages/"))
        .unwrap();
    assert!(
        auth.as_deref()
            .is_some_and(|value| value.starts_with("Bearer ")),
        "{auth:?}"
    );
}

#[test]
fn a_stale_edit_is_a_definitive_rejection_and_is_attempted_once() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::Stale);
    let message = MessageId::generate();
    let operation = OperationId::generate();
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        edit_effect(message, operation, 1, "x"),
        edit_result(operation),
    );
    assert!(
        matches!(&input, Input::EditResult { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::StaleRevision),
        "{input:?}"
    );
    assert_eq!(
        call_count(&server),
        before + 1,
        "a mutation is never repeated by the runtime"
    );

    // A missing message is rejected too.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let operation = OperationId::generate();
    let (input, _) = run_effect(
        &mut client,
        &server,
        edit_effect(message, operation, 1, "x"),
        edit_result(operation),
    );
    assert!(
        matches!(&input, Input::EditResult { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::NotFound)
    );
}

#[test]
fn a_server_failure_on_an_edit_is_an_unknown_outcome_and_is_attempted_once() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let operation = OperationId::generate();
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        edit_effect(MessageId::generate(), operation, 1, "x"),
        edit_result(operation),
    );
    assert!(matches!(
        input,
        Input::EditResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1);
}

#[test]
fn the_status_of_an_edit_operation_decodes_as_edit_committed() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let mut committed = server.stub.message(ConversationId::generate(), 5, "edited");
    committed.revision = 2;
    *server.stub.edit_status.lock().unwrap() = Some(committed.clone());
    let operation = OperationId::generate();
    client.execute(vec![Effect::QueryOperationStatus { operation }]);
    let input = client.expect("the status", |i| {
        matches!(i, Input::OperationStatusResult { .. })
    });
    assert!(
        matches!(input, Input::OperationStatusResult { result: Ok(OperationStatus::EditCommitted { message }), .. } if message == committed)
    );
}

#[test]
fn fetching_a_message_is_a_get_that_is_retried_after_a_server_failure() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let message = MessageId::generate();
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::FetchMessage { message },
        |i| matches!(i, Input::MessageFetched { .. }),
    );
    assert!(
        matches!(&input, Input::MessageFetched { message: m, result: Ok(r) } if *m == message && r.message.id == message),
        "{input:?}"
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("GET", format!("/v1/messages/{message}").as_str())
    );
    assert_eq!(call_count(&server), before + 2, "one failure, one retry");

    // A message that is not there is reported as such, without retries.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::FetchMessage { message },
        |i| matches!(i, Input::MessageFetched { .. }),
    );
    assert!(
        matches!(&input, Input::MessageFetched { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::NotFound)
    );
    assert_eq!(call_count(&server), before + 1);
}

#[test]
fn deleting_is_a_delete_that_is_safely_repeated_after_a_lost_answer() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let message = MessageId::generate();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::DeleteMessage { message },
        |i| matches!(i, Input::DeleteResult { .. }),
    );
    let Input::DeleteResult {
        message: m,
        result: Ok(response),
    } = input
    else {
        panic!("expected the tombstone, got {input:?}");
    };
    assert_eq!(m, message);
    assert!(response.message.deleted);
    assert_eq!(
        response.message.document,
        lazy_domain::message::deleted_placeholder()
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("DELETE", format!("/v1/messages/{message}").as_str())
    );
    assert_eq!(
        call_count(&server),
        before + 2,
        "the failed attempt was repeated"
    );

    // A refusal is final.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::DeleteMessage { message },
        |i| matches!(i, Input::DeleteResult { .. }),
    );
    assert!(matches!(
        &input,
        Input::DeleteResult {
            result: Err(RequestError::Rejected(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1);
}

// ----- Reactions and emoji preferences -----

fn reaction_effect(message: MessageId, text: &str, present: bool) -> Effect {
    Effect::SetReaction {
        message,
        request: lazy_protocol::SetReactionRequest {
            emoji: lazy_domain::ReactionEmoji::parse(text).unwrap(),
            present,
        },
    }
}

fn reaction_result(message: MessageId) -> impl Fn(&Input) -> bool {
    move |input| matches!(input, Input::ReactionResult { message: m, .. } if *m == message)
}

#[test]
fn a_reaction_is_one_authenticated_put_with_the_documented_body() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let message = MessageId::generate();
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        reaction_effect(message, "🎉", true),
        reaction_result(message),
    );
    let Input::ReactionResult {
        emoji,
        result: Ok(response),
        ..
    } = input
    else {
        panic!("expected the message, got {input:?}");
    };
    assert_eq!(emoji.glyph(), Some("🎉"));
    assert_eq!(response.message.reactions.len(), 1);
    assert_eq!(
        response.message.reactions[0].users,
        vec![server.stub.user.id]
    );
    assert_eq!(response.message.reactions_as_of, EventSeq::new(12).unwrap());
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PUT", format!("/v1/messages/{message}/reactions").as_str())
    );
    assert_eq!(
        call.body,
        serde_json::json!({ "emoji": "🎉", "present": true })
    );
    assert_eq!(call_count(&server), before + 1);
    let requests = server.stub.requests.lock().unwrap();
    let (_, auth) = requests
        .iter()
        .rev()
        .find(|(path, _)| path.ends_with("/reactions"))
        .unwrap();
    assert!(
        auth.as_deref().is_some_and(|v| v.starts_with("Bearer ")),
        "{auth:?}"
    );
    drop(requests);

    // Removing one is the same route with `present: false`.
    let (_, call) = run_effect(
        &mut client,
        &server,
        reaction_effect(message, "🎉", false),
        reaction_result(message),
    );
    assert_eq!(
        call.body,
        serde_json::json!({ "emoji": "🎉", "present": false })
    );
}

#[test]
fn reaction_refusals_are_definitive_and_server_failures_unknown_and_neither_repeated_here() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let message = MessageId::generate();

    for (behavior, code) in [
        (MessageBehavior::LimitReached, ErrorCode::LimitReached),
        (MessageBehavior::NotFound, ErrorCode::NotFound),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let before = call_count(&server);
        let (input, _) = run_effect(
            &mut client,
            &server,
            reaction_effect(message, "👍", true),
            reaction_result(message),
        );
        assert!(
            matches!(&input, Input::ReactionResult { result: Err(RequestError::Rejected(e)), .. } if e.code == code),
            "{code:?}: {input:?}"
        );
        assert_eq!(call_count(&server), before + 1, "{code:?}");
    }

    // The core repeats an unknown outcome, because only it knows the latest wish.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        reaction_effect(message, "👍", true),
        reaction_result(message),
    );
    assert!(matches!(
        input,
        Input::ReactionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1, "one attempt per effect");
}

fn prefs_file(client: &Client) -> std::path::PathBuf {
    client.profile.path().join("emoji-prefs.json")
}

fn prefs_of(started: Input) -> lazy_client_core::EmojiPrefs {
    let Input::Started(state) = started else {
        unreachable!()
    };
    state.emoji_prefs
}

fn emoji_prefs(skin_tone: u8, recent: &[&str]) -> lazy_client_core::EmojiPrefs {
    lazy_client_core::EmojiPrefs {
        skin_tone,
        recent: recent
            .iter()
            .map(|text| lazy_domain::ReactionEmoji::parse(text).unwrap())
            .collect(),
    }
}

#[test]
fn emoji_preferences_are_saved_after_a_pause_and_loaded_at_the_next_start() {
    let server = Server::start();
    let (client, started) = Client::start(&server);
    assert_eq!(prefs_of(started), lazy_client_core::EmojiPrefs::default());
    assert!(!prefs_file(&client).exists());

    // A burst of changes is saved as the last one.
    let first = emoji_prefs(1, &["🎉"]);
    let last = emoji_prefs(3, &["👍", "🎉", "❤️"]);
    client.execute(vec![
        Effect::PersistEmojiPrefs(first),
        Effect::PersistEmojiPrefs(last.clone()),
    ]);
    wait_until("the preferences file", || prefs_file(&client).exists());
    let text = std::fs::read_to_string(prefs_file(&client)).unwrap();
    assert!(
        text.contains("\"version\"") && text.contains("👍"),
        "plain JSON: {text}"
    );

    let (mut again, started) = client.restart(&server);
    assert_eq!(prefs_of(started), last);
    again.shutdown();
}

#[test]
fn preferences_are_saved_when_the_client_shuts_down_before_the_pause_ends() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    let prefs = emoji_prefs(2, &["🙌"]);
    client.execute(vec![Effect::PersistEmojiPrefs(prefs.clone())]);
    let path = prefs_file(&client);
    client.shutdown();
    let saved = std::fs::read_to_string(path).unwrap();
    assert!(saved.contains("🙌"), "{saved}");
    let (mut again, started) = Client::start_with(&server, client.profile, client.storage);
    assert_eq!(prefs_of(started), prefs);
    again.shutdown();
}

#[test]
fn a_damaged_or_old_preferences_file_starts_as_the_defaults_and_is_rewritten() {
    let server = Server::start();
    for content in [
        "{ truncated",
        // The shape before versions were recorded.
        r#"{"skin_tone": 2, "recent": ["🎉"]}"#,
        // A version this build does not know.
        r#"{"version": 9, "skin_tone": 2, "recent": ["🎉"]}"#,
    ] {
        let profile = tempfile::tempdir().unwrap();
        std::fs::write(profile.path().join("emoji-prefs.json"), content).unwrap();
        let (client, started) = Client::start_with(&server, profile, MemoryStorage::new());
        let Input::Started(state) = started else {
            unreachable!()
        };
        assert_eq!(
            state.emoji_prefs,
            lazy_client_core::EmojiPrefs::default(),
            "{content}"
        );
        assert!(
            state.problems.is_empty(),
            "no alarm for a preference: {:?}",
            state.problems
        );
        // The next save replaces the file, and a restart reads it back.
        let prefs = emoji_prefs(4, &["❤️"]);
        client.execute(vec![Effect::PersistEmojiPrefs(prefs.clone())]);
        wait_until("a fresh file", || {
            std::fs::read_to_string(prefs_file(&client)).is_ok_and(|text| text.contains("❤️"))
        });
        let (mut again, started) = client.restart(&server);
        assert_eq!(prefs_of(started), prefs);
        again.shutdown();
    }
}

#[test]
fn signing_out_deletes_the_emoji_preferences_with_everything_else() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    client.execute(vec![Effect::PersistEmojiPrefs(emoji_prefs(1, &["🎉"]))]);
    wait_until("the preferences file", || prefs_file(&client).exists());
    client.execute(vec![Effect::SignOut]);
    wait_until("the preferences to be deleted", || {
        !prefs_file(&client).exists()
    });
    // A save still waiting at that moment does not bring the file back.
    std::thread::sleep(Duration::from_millis(150));
    assert!(!prefs_file(&client).exists());
    client.shutdown();
}

fn searches_file(client: &Client) -> std::path::PathBuf {
    client.profile.path().join("recent-searches.json")
}

fn searches_of(started: Input) -> Vec<String> {
    let Input::Started(state) = started else {
        unreachable!()
    };
    state.recent_searches
}

fn queries(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

#[test]
fn recent_searches_are_saved_after_a_pause_and_loaded_at_the_next_start() {
    let server = Server::start();
    let (client, started) = Client::start(&server);
    assert!(searches_of(started).is_empty());
    assert!(!searches_file(&client).exists());

    // A burst of changes is saved as the last one.
    let last = queries(&["deploy in:ops", "日本語 😀", "\"a phrase\" -b"]);
    client.execute(vec![
        Effect::PersistRecentSearches(queries(&["first"])),
        Effect::PersistRecentSearches(last.clone()),
    ]);
    wait_until("the recent searches file", || {
        searches_file(&client).exists()
    });
    wait_until("the last of the burst", || {
        std::fs::read_to_string(searches_file(&client)).is_ok_and(|text| text.contains("日本語"))
    });
    let text = std::fs::read_to_string(searches_file(&client)).unwrap();
    assert!(
        text.contains("\"version\"") && !text.contains("first"),
        "plain JSON, last write only: {text}"
    );

    let (mut again, started) = client.restart(&server);
    assert_eq!(searches_of(started), last, "most recent first, as saved");
    again.shutdown();
}

#[test]
fn recent_searches_are_saved_when_the_client_shuts_down_before_the_pause_ends() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.execute(vec![Effect::PersistRecentSearches(queries(&["late"]))]);
    let path = searches_file(&client);
    client.shutdown();
    assert!(std::fs::read_to_string(path).unwrap().contains("late"));
    let (mut again, started) = Client::start_with(&server, client.profile, client.storage);
    assert_eq!(searches_of(started), queries(&["late"]));
    again.shutdown();
}

#[test]
fn a_damaged_or_old_recent_searches_file_starts_empty_without_an_alarm_and_is_rewritten() {
    let server = Server::start();
    for content in [
        "{ truncated",
        // No version recorded, and a version this build does not know.
        r#"{"queries": ["old"]}"#,
        r#"{"version": 9, "queries": ["old"]}"#,
        r#"{"version": 1, "queries": "not a list"}"#,
    ] {
        let profile = tempfile::tempdir().unwrap();
        std::fs::write(profile.path().join("recent-searches.json"), content).unwrap();
        let (client, started) = Client::start_with(&server, profile, MemoryStorage::new());
        let Input::Started(state) = started else {
            unreachable!()
        };
        assert!(state.recent_searches.is_empty(), "{content}");
        assert!(
            state.problems.is_empty(),
            "no alarm for a convenience: {:?}",
            state.problems
        );
        client.execute(vec![Effect::PersistRecentSearches(queries(&["fresh"]))]);
        wait_until("a fresh file", || {
            std::fs::read_to_string(searches_file(&client)).is_ok_and(|text| text.contains("fresh"))
        });
        let (mut again, started) = client.restart(&server);
        assert_eq!(searches_of(started), queries(&["fresh"]));
        again.shutdown();
    }
}

#[test]
fn the_two_preference_files_are_written_together_without_either_losing_its_write() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    let emoji = emoji_prefs(2, &["🙌"]);
    client.execute(vec![
        Effect::PersistEmojiPrefs(emoji.clone()),
        Effect::PersistRecentSearches(queries(&["both"])),
    ]);
    wait_until("both files", || {
        prefs_file(&client).exists() && searches_file(&client).exists()
    });
    client.shutdown();
    let (mut again, started) = Client::start_with(&server, client.profile, client.storage);
    let Input::Started(state) = started else {
        unreachable!()
    };
    assert_eq!(state.emoji_prefs, emoji);
    assert_eq!(state.recent_searches, queries(&["both"]));
    again.shutdown();
}

#[test]
fn signing_out_deletes_the_recent_searches_and_a_save_waiting_does_not_bring_them_back() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    client.execute(vec![Effect::PersistRecentSearches(queries(&["secret"]))]);
    wait_until("the recent searches file", || {
        searches_file(&client).exists()
    });
    client.execute(vec![Effect::SignOut]);
    wait_until("the recent searches to be deleted", || {
        !searches_file(&client).exists()
    });
    // A save still waiting when the person signs out is dropped or deleted with the rest.
    client.execute(vec![
        Effect::PersistRecentSearches(queries(&["late"])),
        Effect::SignOut,
    ]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!searches_file(&client).exists());
    assert!(
        !client
            .profile
            .path()
            .join("recent-searches.json.tmp")
            .exists()
    );
    client.shutdown();
}

// ----- Notifications, status, and the thread inbox (slice 3D) -----

#[test]
fn settings_preferences_and_status_are_one_put_each_with_the_documented_body() {
    use lazy_domain::notification::{NotificationSettings, NotifyLevel, UserStatus};
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let conversation = ConversationId::generate();

    let settings = NotificationSettings {
        channel_level: NotifyLevel::All,
        show_content: false,
        ..Default::default()
    };
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetNotificationSettings {
            workspace,
            settings,
        },
        |i| matches!(i, Input::NotificationSettingsResult { .. }),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PUT",
            format!("/v1/workspaces/{workspace}/notification-settings").as_str()
        )
    );
    assert_eq!(call.body["channel_level"], "all");
    assert_eq!(call.body["show_content"], false);
    assert!(
        matches!(input, Input::NotificationSettingsResult { result: Ok(r), .. } if r.settings == settings)
    );
    assert_eq!(call_count(&server), before + 1);

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetConversationNotificationPrefs {
            conversation,
            request: lazy_protocol::SetConversationNotificationPrefsRequest {
                level: Some(NotifyLevel::Nothing),
                muted: true,
            },
        },
        |i| matches!(i, Input::ConversationPrefsResult { .. }),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PUT",
            format!("/v1/conversations/{conversation}/notification-prefs").as_str()
        )
    );
    assert_eq!(call.body["level"], "nothing");
    assert_eq!(call.body["muted"], true);
    let Input::ConversationPrefsResult {
        conversation: echoed,
        result: Ok(response),
    } = input
    else {
        panic!("expected the preference");
    };
    assert_eq!(echoed, conversation);
    assert_eq!(
        (response.prefs.level, response.prefs.muted),
        (Some(NotifyLevel::Nothing), true)
    );

    let status = UserStatus::new("In a meeting", None, None).unwrap();
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetStatus {
            workspace,
            request: lazy_protocol::SetStatusRequest {
                status: Some(status.clone()),
            },
        },
        |i| matches!(i, Input::StatusResult { .. }),
    );
    assert_eq!(call.path, format!("/v1/workspaces/{workspace}/status"));
    assert_eq!(call.body["status"]["text"], "In a meeting");
    assert!(
        matches!(input, Input::StatusResult { result: Ok(r), .. } if r.user.status == Some(status))
    );
    // Clearing sends a null.
    let (_, call) = run_effect(
        &mut client,
        &server,
        Effect::SetStatus {
            workspace,
            request: lazy_protocol::SetStatusRequest { status: None },
        },
        |i| matches!(i, Input::StatusResult { .. }),
    );
    assert!(call.body["status"].is_null());
}

#[test]
fn saving_settings_is_never_repeated_by_the_runtime_and_failures_are_classified() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let effect = || Effect::SetNotificationSettings {
        workspace,
        settings: Default::default(),
    };
    for (behavior, expected) in [
        (MessageBehavior::Forbidden, Some(ErrorCode::Forbidden)),
        (MessageBehavior::NotFound, Some(ErrorCode::NotFound)),
        (MessageBehavior::ServerError, None),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let before = call_count(&server);
        let (input, _) = run_effect(&mut client, &server, effect(), |i| {
            matches!(i, Input::NotificationSettingsResult { .. })
        });
        match (expected, input) {
            (
                Some(code),
                Input::NotificationSettingsResult {
                    result: Err(RequestError::Rejected(e)),
                    ..
                },
            ) => {
                assert_eq!(e.code, code)
            }
            (
                None,
                Input::NotificationSettingsResult {
                    result: Err(RequestError::Unknown(_)),
                    ..
                },
            ) => {}
            (_, other) => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            call_count(&server),
            before + 1,
            "the core repeats, not the runtime"
        );
    }
}

#[test]
fn the_inbox_is_a_get_with_a_query_that_is_read_again_and_marking_read_is_a_post() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LoadInbox {
            workspace,
            generation: 4,
            query: lazy_protocol::InboxQuery {
                unread: Some(true),
                before: EventSeq::new(77),
                limit: Some(20),
            },
        },
        |i| matches!(i, Input::InboxLoaded { .. }),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("GET", format!("/v1/workspaces/{workspace}/inbox").as_str())
    );
    let query = call.body["query"].as_str().unwrap();
    for part in ["unread=true", "before=77", "limit=20"] {
        assert!(query.contains(part), "{query}");
    }
    let Input::InboxLoaded {
        generation,
        result: Ok(page),
    } = input
    else {
        panic!("expected a page");
    };
    assert_eq!(generation, 4, "the generation is echoed");
    assert!(page.entries.is_empty() && page.has_more);

    // A server failure is read again, as any read is.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::LoadInbox {
            workspace,
            generation: 5,
            query: Default::default(),
        },
        |i| matches!(i, Input::InboxLoaded { generation: 5, .. }),
    );
    assert!(matches!(input, Input::InboxLoaded { result: Ok(_), .. }));
    assert_eq!(call_count(&server), before + 2);

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::MarkInboxRead { workspace },
        |i| matches!(i, Input::InboxReadResult { .. }),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/workspaces/{workspace}/inbox/read").as_str()
        )
    );
    assert!(matches!(input, Input::InboxReadResult { result: Ok(r) } if r.updated == 7));
    // One attempt: the core decides whether to ask again.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::MarkInboxRead { workspace },
        |i| matches!(i, Input::InboxReadResult { .. }),
    );
    assert!(matches!(
        input,
        Input::InboxReadResult {
            result: Err(RequestError::Unknown(_))
        }
    ));
    assert_eq!(call_count(&server), before + 1);
}

#[test]
fn the_core_is_told_the_time_before_the_first_realtime_frames() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    // The clock comes first, so the core knows what time an event arrived in.
    let clock = client.expect("the clock", |i| {
        matches!(i, Input::Clock(_) | Input::Frame(_))
    });
    let Input::Clock(now) = clock else {
        panic!("a frame came before the clock: {clock:?}");
    };
    let drift = (lazy_domain::Timestamp::now()
        .as_offset_date_time()
        .unix_timestamp()
        - now.as_offset_date_time().unix_timestamp())
    .abs();
    assert!(drift < 30, "{drift}");
    client.expect("a frame", |i| matches!(i, Input::Frame(_)));
    client.shutdown();
}

// ----- Discovery (slice 3E) -----

#[test]
fn saving_a_message_is_one_put_with_the_documented_body_and_is_never_repeated_here() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let message = MessageId::generate();
    let effect = |saved| Effect::SetMessageSaved {
        message,
        request: lazy_protocol::SetSavedRequest { saved },
    };
    let (input, call) = run_effect(&mut client, &server, effect(true), |i| {
        matches!(i, Input::MessageSavedResult { .. })
    });
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PUT", format!("/v1/messages/{message}/saved").as_str())
    );
    assert_eq!(call.body["saved"], true);
    let Input::MessageSavedResult {
        message_id,
        result: Ok(response),
    } = input
    else {
        panic!("expected the answer");
    };
    assert_eq!((message_id, response.saved), (message, true));
    let (_, call) = run_effect(&mut client, &server, effect(false), |i| {
        matches!(i, Input::MessageSavedResult { .. })
    });
    assert_eq!(call.body["saved"], false);
    // Failures are classified; the core repeats, not the runtime.
    for (behavior, expected) in [
        (MessageBehavior::NotFound, Some(ErrorCode::NotFound)),
        (MessageBehavior::ServerError, None),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let before = call_count(&server);
        let (input, _) = run_effect(&mut client, &server, effect(true), |i| {
            matches!(i, Input::MessageSavedResult { .. })
        });
        match (expected, input) {
            (
                Some(code),
                Input::MessageSavedResult {
                    result: Err(RequestError::Rejected(e)),
                    ..
                },
            ) => {
                assert_eq!(e.code, code)
            }
            (
                None,
                Input::MessageSavedResult {
                    result: Err(RequestError::Unknown(_)),
                    ..
                },
            ) => {}
            (_, other) => panic!("unexpected {other:?}"),
        }
        assert_eq!(call_count(&server), before + 1);
    }
}

#[test]
fn the_saved_list_the_people_directory_and_a_profile_are_reads_with_their_queries() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LoadSaved {
            workspace,
            generation: 3,
            query: lazy_protocol::SavedQuery {
                before: Some(41),
                limit: Some(20),
            },
        },
        |i| matches!(i, Input::SavedLoaded { .. }),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("GET", format!("/v1/workspaces/{workspace}/saved").as_str())
    );
    let query = call.body["query"].as_str().unwrap();
    assert!(
        query.contains("before=41") && query.contains("limit=20"),
        "{query}"
    );
    assert!(
        matches!(input, Input::SavedLoaded { generation: 3, result: Ok(page) } if page.has_more)
    );

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LoadPeople {
            workspace,
            generation: 4,
            query: lazy_protocol::PeopleQuery {
                query: Some("ad lo".into()),
                include_deactivated: None,
                offset: Some(50),
                limit: Some(50),
            },
        },
        |i| matches!(i, Input::PeopleLoaded { .. }),
    );
    assert_eq!(call.path, format!("/v1/workspaces/{workspace}/people"));
    let query = call.body["query"].as_str().unwrap();
    for part in ["query=ad+lo", "offset=50", "limit=50"] {
        assert!(query.contains(part), "{query}");
    }
    assert!(matches!(
        input,
        Input::PeopleLoaded {
            generation: 4,
            result: Ok(_)
        }
    ));

    let user = UserId::generate();
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LoadProfile {
            workspace,
            generation: 5,
            user,
        },
        |i| matches!(i, Input::ProfileLoaded { .. }),
    );
    assert_eq!(
        call.path,
        format!("/v1/workspaces/{workspace}/people/{user}")
    );
    let Input::ProfileLoaded {
        generation: 5,
        result: Ok(response),
    } = input
    else {
        panic!("expected a profile");
    };
    assert_eq!(response.person.role, lazy_domain::WorkspaceRole::Admin);

    // A server failure is read again, as any read is; a refusal is final.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::LoadPeople {
            workspace,
            generation: 6,
            query: Default::default(),
        },
        |i| matches!(i, Input::PeopleLoaded { generation: 6, .. }),
    );
    assert!(matches!(input, Input::PeopleLoaded { result: Ok(_), .. }));
    assert_eq!(call_count(&server), before + 2);
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::LoadProfile {
            workspace,
            generation: 7,
            user,
        },
        |i| matches!(i, Input::ProfileLoaded { generation: 7, .. }),
    );
    assert!(matches!(
        input,
        Input::ProfileLoaded { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::NotFound
    ));
    assert_eq!(call_count(&server), before + 1);
}

fn conversations_file(client: &Client) -> std::path::PathBuf {
    client.profile.path().join("recent-conversations.json")
}

fn conversations_of(started: Input) -> Vec<ConversationId> {
    let Input::Started(state) = started else {
        unreachable!()
    };
    state.recent_conversations
}

#[test]
fn recent_conversations_are_saved_after_a_pause_loaded_at_the_next_start_and_deleted_at_sign_out() {
    let server = Server::start();
    let (client, started) = Client::start(&server);
    assert!(conversations_of(started).is_empty());
    assert!(!conversations_file(&client).exists());
    let ids: Vec<ConversationId> = (0..3).map(|_| ConversationId::generate()).collect();
    // A burst is saved as the last of it.
    client.execute(vec![
        Effect::PersistRecentConversations(vec![ids[0]]),
        Effect::PersistRecentConversations(ids.clone()),
    ]);
    wait_until("the last of the burst", || {
        std::fs::read_to_string(conversations_file(&client))
            .is_ok_and(|text| text.contains(&ids[2].to_string()))
    });
    let (mut again, started) = client.restart(&server);
    assert_eq!(conversations_of(started), ids);
    // It is saved at shutdown too, and signing out deletes it.
    again.login();
    again.execute(vec![Effect::PersistRecentConversations(vec![ids[1]])]);
    let path = conversations_file(&again);
    again.shutdown();
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains(&ids[1].to_string())
    );
    let (mut third, started) = Client::start_with(&server, again.profile, again.storage);
    assert_eq!(conversations_of(started), vec![ids[1]]);
    third.login();
    third.execute(vec![Effect::SignOut]);
    wait_until("the file to be deleted", || {
        !conversations_file(&third).exists()
    });
    third.execute(vec![
        Effect::PersistRecentConversations(ids.clone()),
        Effect::SignOut,
    ]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !conversations_file(&third).exists(),
        "a save waiting does not bring it back"
    );
    third.shutdown();
}

#[test]
fn a_damaged_recent_conversations_file_starts_empty_without_an_alarm() {
    let server = Server::start();
    for content in [
        "{ truncated",
        r#"{"conversations": []}"#,
        r#"{"version": 9, "conversations": []}"#,
    ] {
        let profile = tempfile::tempdir().unwrap();
        std::fs::write(profile.path().join("recent-conversations.json"), content).unwrap();
        let (mut client, started) = Client::start_with(&server, profile, MemoryStorage::new());
        let Input::Started(state) = started else {
            unreachable!()
        };
        assert!(state.recent_conversations.is_empty(), "{content}");
        assert!(state.problems.is_empty(), "{:?}", state.problems);
        client.shutdown();
    }
}

// ----- Groups and who is in a conversation -----

fn group_result(operation: OperationId) -> impl Fn(&Input) -> bool {
    move |input| matches!(input, Input::GroupCommandResult { operation: o, .. } if *o == operation)
}

#[test]
fn creating_a_group_is_one_post_with_the_documented_body_and_never_repeated_here() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let operation = OperationId::generate();
    let member = UserId::generate();
    let effect = || Effect::CreateGroup {
        workspace,
        request: lazy_protocol::CreateGroupRequest {
            operation_id: operation,
            handle: lazy_domain::GroupHandle::parse("design-team").unwrap(),
            name: DisplayName::parse("Design Team").unwrap(),
            members: vec![member],
        },
    };
    let before = call_count(&server);
    let (input, call) = run_effect(&mut client, &server, effect(), group_result(operation));
    let Input::GroupCommandResult {
        result: Ok(response),
        ..
    } = input
    else {
        panic!("expected the group, got {input:?}");
    };
    assert_eq!(response.group.handle.as_str(), "design-team");
    assert_eq!(response.group.members, vec![member]);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/workspaces/{workspace}/groups").as_str()
        )
    );
    assert_eq!(call.body["operation_id"], operation.to_string());
    assert_eq!(call.body["handle"], "design-team");
    assert_eq!(call.body["name"], "Design Team");
    assert_eq!(call_count(&server), before + 1);

    // A taken handle and a limit are refusals; a server failure is an unknown outcome. Each is
    // one attempt, because the core reconciles with the operation id.
    for (behavior, expected) in [
        (MessageBehavior::NameTaken, Some(ErrorCode::NameTaken)),
        (MessageBehavior::LimitReached, Some(ErrorCode::LimitReached)),
        (MessageBehavior::Forbidden, Some(ErrorCode::Forbidden)),
        (MessageBehavior::ServerError, None),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let before = call_count(&server);
        let (input, _) = run_effect(&mut client, &server, effect(), group_result(operation));
        match (expected, &input) {
            (
                Some(code),
                Input::GroupCommandResult {
                    result: Err(RequestError::Rejected(error)),
                    ..
                },
            ) => assert_eq!(error.code, code),
            (
                None,
                Input::GroupCommandResult {
                    result: Err(RequestError::Unknown(_)),
                    ..
                },
            ) => {}
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(call_count(&server), before + 1);
    }
}

#[test]
fn changing_a_group_is_a_patch_and_deleting_one_is_a_delete_that_is_repeated() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let group = lazy_domain::GroupId::generate();
    let kind = ActionKind::UpdateGroup(group);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::UpdateGroup {
            group,
            request: lazy_protocol::UpdateGroupRequest {
                handle: None,
                name: Some(DisplayName::parse("Designers").unwrap()),
                members: Some(vec![]),
            },
        },
        action_result(kind),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Group(_)),
            ..
        }
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PATCH", format!("/v1/groups/{group}").as_str())
    );
    assert_eq!(call.body["name"], "Designers");
    assert!(call.body["handle"].is_null(), "absent fields are unchanged");
    assert_eq!(call.body["members"], serde_json::json!([]));

    // Deleting is idempotent: a lost answer is asked again, and any 2xx counts.
    let kind = ActionKind::DeleteGroup(group);
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::DeleteGroup { group },
        action_result(kind),
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
        ("DELETE", format!("/v1/groups/{group}").as_str())
    );
    assert_eq!(
        call_count(&server),
        before + 2,
        "the failed attempt was repeated"
    );

    // A refusal is final and is not repeated.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::Forbidden);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::DeleteGroup { group },
        action_result(kind),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Rejected(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1);
}

#[test]
fn the_people_in_a_conversation_are_fetched_with_a_get_that_is_retried() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let conversation = ConversationId::generate();
    let wanted = |input: &Input| matches!(input, Input::MembersFetched { .. });
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::FetchMembers {
            conversation,
            generation: 7,
        },
        wanted,
    );
    let Input::MembersFetched {
        conversation: c,
        generation,
        result: Ok(response),
    } = input
    else {
        panic!("expected the members, got {input:?}");
    };
    assert_eq!(
        (c, generation),
        (conversation, 7),
        "the generation is echoed"
    );
    assert_eq!(response.conversation_id, conversation);
    assert_eq!(response.user_ids, vec![server.stub.user.id]);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "GET",
            format!("/v1/conversations/{conversation}/members").as_str()
        )
    );
    assert_eq!(call_count(&server), before + 2, "one failure, one retry");

    // A conversation that is not there is a refusal, without retries.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::FetchMembers {
            conversation,
            generation: 8,
        },
        wanted,
    );
    assert!(matches!(
        input,
        Input::MembersFetched {
            generation: 8,
            result: Err(RequestError::Rejected(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1);

    // Adding people to a channel is still the POST on the same path.
    let (_, call) = run_effect(
        &mut client,
        &server,
        Effect::AddMembers {
            conversation,
            request: lazy_protocol::AddMembersRequest {
                user_ids: vec![UserId::generate()],
            },
        },
        action_result(ActionKind::AddMembers(conversation)),
    );
    assert_eq!(call.method, "POST");
}

// ----- Threads -----

#[test]
fn a_reply_is_posted_with_its_thread_and_a_plain_send_is_unchanged() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let root = MessageId::generate();
    let operation = OperationId::generate();
    client.execute(vec![Effect::SendMessage {
        operation,
        conversation: ConversationId::generate(),
        document: MessageDocument::from_plain_text("an answer").unwrap(),
        thread_root: Some(root),
        also_in_channel: true,
        attachments: Vec::new(),
    }]);
    client.expect(
        "a send result",
        |i| matches!(i, Input::SendResult { operation: o, .. } if *o == operation),
    );
    let body = server.stub.last_send.lock().unwrap().clone().unwrap();
    assert_eq!(body["thread_root"], serde_json::to_value(root).unwrap());
    assert_eq!(body["also_in_channel"], true);

    let plain = OperationId::generate();
    client.execute(vec![send_effect(plain)]);
    client.expect(
        "a send result",
        |i| matches!(i, Input::SendResult { operation: o, .. } if *o == plain),
    );
    let body = server.stub.last_send.lock().unwrap().clone().unwrap();
    assert!(
        body.get("thread_root").is_none() && body.get("also_in_channel").is_none(),
        "a send to the channel is written as it always was: {body}"
    );
    assert_eq!(server.stub.sends.load(Ordering::SeqCst), 2);
    client.shutdown();
}

#[test]
fn replies_are_fetched_with_a_get_that_is_retried_and_the_generation_is_echoed() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let root = MessageId::generate();
    let wanted = |input: &Input| matches!(input, Input::RepliesLoaded { .. });
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::FetchReplies {
            root,
            before: Some(EventSeq::new(30).unwrap()),
            limit: Some(50),
            generation: 7,
        },
        wanted,
    );
    let Input::RepliesLoaded {
        root: r,
        generation,
        result: Ok(page),
    } = input
    else {
        panic!("expected the replies, got {input:?}");
    };
    assert_eq!((r, generation), (root, 7));
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.messages[0].thread_root, Some(root));
    assert!(page.has_more);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("GET", format!("/v1/messages/{root}/replies").as_str())
    );
    let query = call.body["query"].as_str().unwrap_or_default();
    assert!(
        query.contains("before=30") && query.contains("limit=50"),
        "{query}"
    );
    assert_eq!(call_count(&server), before + 2, "one failure, one retry");

    // A thread that is not there is a refusal, without retries.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::FetchReplies {
            root,
            before: None,
            limit: None,
            generation: 8,
        },
        wanted,
    );
    assert!(matches!(
        input,
        Input::RepliesLoaded { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::NotFound
    ));
    assert_eq!(call_count(&server), before + 1);
    client.shutdown();
}

#[test]
fn following_and_reading_a_thread_are_single_attempts_with_their_documented_bodies() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let root = MessageId::generate();

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetThreadFollow {
            root,
            following: true,
        },
        |i| matches!(i, Input::ThreadFollowResult { .. }),
    );
    assert!(matches!(
        input,
        Input::ThreadFollowResult { result: Ok(ThreadStateResponse { state }), .. } if state.following
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PUT", format!("/v1/messages/{root}/thread/follow").as_str())
    );
    assert_eq!(call.body, serde_json::json!({ "following": true }));

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::MarkThreadRead {
            root,
            up_to: EventSeq::new(21).unwrap(),
        },
        |i| matches!(i, Input::ThreadReadResult { .. }),
    );
    assert!(matches!(
        input,
        Input::ThreadReadResult { up_to, result: Ok(ThreadStateResponse { state }), .. }
            if up_to == EventSeq::new(21).unwrap() && state.last_read == EventSeq::new(21).unwrap()
    ));
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("POST", format!("/v1/messages/{root}/thread/read").as_str())
    );
    assert_eq!(call.body, serde_json::json!({ "up_to": 21 }));

    // A failure is reported once, as an unknown outcome: the core decides about repeating.
    for (effect, wanted) in [
        (
            Effect::SetThreadFollow {
                root,
                following: false,
            },
            (|i: &Input| matches!(i, Input::ThreadFollowResult { .. })) as fn(&Input) -> bool,
        ),
        (
            Effect::MarkThreadRead {
                root,
                up_to: EventSeq::new(22).unwrap(),
            },
            |i: &Input| matches!(i, Input::ThreadReadResult { .. }),
        ),
    ] {
        server
            .stub
            .message_plan
            .lock()
            .unwrap()
            .push_back(MessageBehavior::ServerError);
        let before = call_count(&server);
        let (input, _) = run_effect(&mut client, &server, effect, wanted);
        assert!(matches!(
            input,
            Input::ThreadFollowResult {
                result: Err(RequestError::Unknown(_)),
                ..
            } | Input::ThreadReadResult {
                result: Err(RequestError::Unknown(_)),
                ..
            }
        ));
        assert_eq!(call_count(&server), before + 1, "one attempt");
    }

    // A refusal is a refusal.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::Forbidden);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::SetThreadFollow {
            root,
            following: true,
        },
        |i| matches!(i, Input::ThreadFollowResult { .. }),
    );
    assert!(matches!(
        input,
        Input::ThreadFollowResult { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::Forbidden
    ));
    client.shutdown();
}

#[test]
fn reply_drafts_survive_a_restart_next_to_the_conversation_drafts() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let conversation = ConversationId::generate();
    let thread = lazy_client_core::ThreadDraft {
        root: MessageId::generate(),
        conversation,
        text: "half a reply".into(),
        selection: None,
    };
    client.execute(vec![
        Effect::PersistDrafts(vec![(conversation, "a draft".into())]),
        Effect::PersistThreadDrafts(vec![thread.clone()]),
    ]);
    // The saver writes after a short pause; shutting down flushes it.
    let (client, started) = {
        client.shutdown();
        client.restart(&server)
    };
    let Input::Started(state) = started else {
        panic!("expected Started");
    };
    assert_eq!(
        state.drafts.get(&conversation).map(String::as_str),
        Some("a draft")
    );
    assert_eq!(state.thread_drafts, vec![thread]);
    drop(client);
}

// ----- Webhooks and what the person was looking at -----

#[test]
fn creating_a_webhook_is_one_post_that_is_never_repeated_and_hides_the_address() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let conversation = ConversationId::generate();
    let kind = ActionKind::CreateWebhook(conversation);
    let wanted =
        move |input: &Input| matches!(input, Input::ActionResult { kind: k, .. } if *k == kind);
    let request = lazy_protocol::CreateWebhookRequest {
        name: DisplayName::parse("Deploys").unwrap(),
    };
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::CreateWebhook {
            conversation,
            request: request.clone(),
        },
        wanted,
    );
    // Nothing the input prints, or anything that follows from it, shows the address.
    assert!(
        !format!("{input:?}").contains(STUB_HOOK_SECRET),
        "{input:?}"
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::WebhookCreated(created)),
        ..
    } = input
    else {
        panic!("expected a created webhook, got {input:?}");
    };
    assert!(created.url.as_str().ends_with(STUB_HOOK_SECRET));
    assert_eq!(created.webhook.conversation_id, conversation);
    assert_eq!(created.webhook.name.as_str(), "Deploys");
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/conversations/{conversation}/webhooks").as_str()
        )
    );
    assert_eq!(call.body, serde_json::json!({ "name": "Deploys" }));
    assert_eq!(call_count(&server), before + 1);

    // A failure is reported once, as an unknown outcome: a second post could make a second
    // webhook, so the core decides what to do.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::CreateWebhook {
            conversation,
            request: request.clone(),
        },
        wanted,
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1, "one attempt");

    // Refusals are refusals.
    for (behavior, code) in [
        (MessageBehavior::Forbidden, ErrorCode::Forbidden),
        (MessageBehavior::LimitReached, ErrorCode::LimitReached),
        (MessageBehavior::NotFound, ErrorCode::NotFound),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let (input, _) = run_effect(
            &mut client,
            &server,
            Effect::CreateWebhook {
                conversation,
                request: request.clone(),
            },
            wanted,
        );
        assert!(matches!(
            input,
            Input::ActionResult { result: Err(RequestError::Rejected(e)), .. } if e.code == code
        ));
    }
    client.shutdown();
}

#[test]
fn webhooks_are_listed_with_a_get_that_is_retried_and_deleted_with_a_delete_that_is_repeated() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let conversation = ConversationId::generate();
    let list_kind = ActionKind::LoadWebhooks(conversation);
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::LoadWebhooks { conversation },
        move |i| matches!(i, Input::ActionResult { kind, .. } if *kind == list_kind),
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::Webhooks(list)),
        ..
    } = input
    else {
        panic!("expected the webhooks, got {input:?}");
    };
    assert_eq!(list.len(), 1);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "GET",
            format!("/v1/conversations/{conversation}/webhooks").as_str()
        )
    );
    assert_eq!(call_count(&server), before + 2, "one failure, one retry");

    let webhook = list[0].id;
    let delete_kind = ActionKind::DeleteWebhook(webhook);
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::DeleteWebhook { webhook },
        move |i| matches!(i, Input::ActionResult { kind, .. } if *kind == delete_kind),
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
        ("DELETE", format!("/v1/webhooks/{webhook}").as_str())
    );
    assert_eq!(
        call_count(&server),
        before + 2,
        "a delete is idempotent, so it is repeated"
    );

    // A webhook that is not there is a refusal, without repeats.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::DeleteWebhook { webhook },
        move |i| matches!(i, Input::ActionResult { kind, .. } if *kind == delete_kind),
    );
    assert!(matches!(
        input,
        Input::ActionResult { result: Err(RequestError::Rejected(e)), .. } if e.code == ErrorCode::NotFound
    ));
    assert_eq!(call_count(&server), before + 1);
    client.shutdown();
}

// ----- Workspace administration -----

#[test]
fn changing_the_workspace_a_role_or_a_deactivation_is_one_put_or_patch_with_the_documented_body() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let user = UserId::generate();

    let settings = lazy_domain::workspace::WorkspaceSettings {
        registration: lazy_domain::workspace::RegistrationPolicy::InviteOnly,
        channel_creation: lazy_domain::workspace::ChannelCreationPolicy::AdminsOnly,
    };
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::UpdateWorkspace {
            workspace,
            request: lazy_protocol::UpdateWorkspaceRequest {
                name: Some(lazy_domain::WorkspaceName::parse("Acme").unwrap()),
                settings: Some(settings),
            },
        },
        action_result(ActionKind::UpdateWorkspace),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PATCH", format!("/v1/workspaces/{workspace}").as_str())
    );
    assert_eq!(call.body["name"], "Acme");
    assert_eq!(call.body["settings"]["registration"], "invite_only");
    let Input::ActionResult {
        result: Ok(ActionOutcome::Workspace(answer)),
        ..
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!((answer.name.as_str(), answer.settings), ("Acme", settings));

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetMemberRole {
            workspace,
            user,
            request: lazy_protocol::SetRoleRequest {
                role: WorkspaceRole::Admin,
            },
        },
        action_result(ActionKind::SetMemberRole(user)),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PUT",
            format!("/v1/workspaces/{workspace}/people/{user}/role").as_str()
        )
    );
    assert_eq!(call.body, serde_json::json!({ "role": "admin" }));
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Role { user: u, role: WorkspaceRole::Admin }),
            ..
        } if u == user
    ));

    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetMemberDeactivated {
            workspace,
            user,
            request: lazy_protocol::SetDeactivatedRequest { deactivated: true },
        },
        action_result(ActionKind::SetMemberDeactivated(user)),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "PUT",
            format!("/v1/workspaces/{workspace}/people/{user}/deactivated").as_str()
        )
    );
    assert_eq!(call.body, serde_json::json!({ "deactivated": true }));
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Deactivated { user: u, deactivated: true }),
            ..
        } if u == user
    ));
    client.shutdown();
}

#[test]
fn administration_changes_are_idempotent_so_a_lost_answer_is_asked_again_and_refusals_are_not() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let user = UserId::generate();
    let effect = || Effect::SetMemberRole {
        workspace,
        user,
        request: lazy_protocol::SetRoleRequest {
            role: WorkspaceRole::Admin,
        },
    };
    let kind = ActionKind::SetMemberRole(user);
    // One server failure, then success: the same request is made again.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(&mut client, &server, effect(), action_result(kind));
    assert!(matches!(input, Input::ActionResult { result: Ok(_), .. }));
    assert_eq!(call_count(&server), before + 2);

    // A refusal is final, with its code.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::Forbidden);
    let before = call_count(&server);
    let (input, _) = run_effect(&mut client, &server, effect(), action_result(kind));
    assert!(matches!(
        input,
        Input::ActionResult { result: Err(RequestError::Rejected(e)), .. }
            if e.code == ErrorCode::Forbidden
    ));
    assert_eq!(call_count(&server), before + 1);
    client.shutdown();
}

#[test]
fn creating_an_invitation_is_one_post_that_is_never_repeated_and_hides_the_code() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let kind = ActionKind::CreateInvitation;
    let effect = || Effect::CreateInvitation {
        workspace,
        request: lazy_protocol::CreateInvitationRequest {
            expires_in_secs: Some(7200),
            max_uses: Some(5),
        },
    };
    let before = call_count(&server);
    let (input, call) = run_effect(&mut client, &server, effect(), action_result(kind));
    assert!(
        !format!("{input:?}").contains(STUB_INVITE_CODE),
        "nothing printed shows the code: {input:?}"
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "POST",
            format!("/v1/workspaces/{workspace}/invitations").as_str()
        )
    );
    assert_eq!(
        call.body,
        serde_json::json!({ "expires_in_secs": 7200, "max_uses": 5 })
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::InvitationCreated(created)),
        ..
    } = input
    else {
        panic!("expected a created invitation");
    };
    assert_eq!(created.code.expose(), STUB_INVITE_CODE);
    assert_eq!(created.invitation.max_uses, Some(5));
    assert_eq!(call_count(&server), before + 1);

    // A failed attempt is reported once, as unknown: a second post could make a second one.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(&mut client, &server, effect(), action_result(kind));
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1, "one attempt");

    for (behavior, code) in [
        (MessageBehavior::Forbidden, ErrorCode::Forbidden),
        (MessageBehavior::LimitReached, ErrorCode::LimitReached),
    ] {
        server.stub.message_plan.lock().unwrap().push_back(behavior);
        let (input, _) = run_effect(&mut client, &server, effect(), action_result(kind));
        assert!(matches!(
            input,
            Input::ActionResult { result: Err(RequestError::Rejected(e)), .. } if e.code == code
        ));
    }
    client.shutdown();
}

#[test]
fn invitations_are_listed_with_a_get_that_is_retried_and_revoked_with_a_delete_that_is_repeated() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;

    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    client.execute(vec![Effect::LoadInvitations {
        workspace,
        generation: 7,
    }]);
    let input = client.expect("the list", |i| matches!(i, Input::InvitationsLoaded { .. }));
    let call = last_call(&server);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        (
            "GET",
            format!("/v1/workspaces/{workspace}/invitations").as_str()
        )
    );
    assert_eq!(
        call_count(&server),
        before + 2,
        "read again after a failure"
    );
    let Input::InvitationsLoaded {
        generation,
        result: Ok(list),
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!((generation, list.invitations.len()), (7, 1));

    let id = lazy_domain::InvitationId::generate();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::RevokeInvitation { invitation: id },
        action_result(ActionKind::RevokeInvitation(id)),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("DELETE", format!("/v1/invitations/{id}").as_str())
    );
    assert_eq!(
        call_count(&server),
        before + 2,
        "repeated after a lost answer"
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::Invitation(revoked)),
        ..
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!(revoked.id, id);
    assert_eq!(revoked.state, lazy_protocol::InvitationState::Revoked);

    // A cancelled request reports nothing, and neither does one a newer request replaced.
    client.execute(vec![
        Effect::LoadInvitations {
            workspace,
            generation: 8,
        },
        Effect::CancelInvitations,
    ]);
    client.expect_quiet(Duration::from_millis(300));
    client.execute(vec![
        Effect::LoadInvitations {
            workspace,
            generation: 9,
        },
        Effect::LoadInvitations {
            workspace,
            generation: 10,
        },
    ]);
    let input = client.expect("the newest list", |i| {
        matches!(i, Input::InvitationsLoaded { .. })
    });
    assert!(matches!(
        input,
        Input::InvitationsLoaded { generation: 10, .. }
    ));
    client.expect_quiet(Duration::from_millis(300));
    client.shutdown();
}

#[test]
fn a_registration_carries_its_code_only_when_there_is_one() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    for code in [None, Some(lazy_protocol::InviteCode::new(STUB_INVITE_CODE))] {
        let request = lazy_protocol::RegisterRequest {
            username: Username::parse("newcomer").unwrap(),
            display_name: DisplayName::parse("Newcomer").unwrap(),
            password: lazy_protocol::Password::new("password-1"),
            invite_code: code.clone(),
        };
        assert!(!format!("{request:?}").contains(STUB_INVITE_CODE));
        client.execute(vec![Effect::Register(request)]);
        client.expect("Authenticated", |i| {
            matches!(i, Input::Authenticated { .. })
        });
    }
    let bodies = server.stub.registrations.lock().unwrap().clone();
    assert_eq!(bodies.len(), 2);
    assert!(bodies[0].get("invite_code").is_none(), "{:?}", bodies[0]);
    assert_eq!(bodies[1]["invite_code"], STUB_INVITE_CODE);
    client.shutdown();
}

#[test]
fn unsubscribing_ends_the_connection_for_good_and_a_later_subscribe_starts_again() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = server.stub.workspace;
    let subscribe = || Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    };
    client.execute(vec![subscribe()]);
    client.expect("caught up", |i| {
        matches!(i, Input::Frame(ServerFrame::CaughtUp { .. }))
    });
    assert_eq!(server.stub.active_sessions.load(Ordering::SeqCst), 1);

    client.execute(vec![Effect::Unsubscribe { workspace }]);
    wait_until("the connection to close", || {
        server.stub.active_sessions.load(Ordering::SeqCst) == 0
    });
    let upgrades = server.stub.upgrades.load(Ordering::SeqCst);
    client.expect_quiet(Duration::from_millis(400));
    assert_eq!(
        server.stub.upgrades.load(Ordering::SeqCst),
        upgrades,
        "not reconnected"
    );
    // Unsubscribing a workspace that is not subscribed is nothing.
    client.execute(vec![Effect::Unsubscribe { workspace }]);

    client.execute(vec![subscribe()]);
    client.expect("caught up again", |i| {
        matches!(i, Input::Frame(ServerFrame::CaughtUp { .. }))
    });
    assert_eq!(server.stub.active_sessions.load(Ordering::SeqCst), 1);
    client.shutdown();
}

// ----- Several workspaces, profile, avatars, appearance (slice 3G) -----

#[test]
fn creating_and_joining_a_workspace_are_single_attempts_with_the_documented_bodies() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let workspace = WorkspaceId::generate();
    let request = lazy_protocol::CreateWorkspaceRequest {
        name: lazy_domain::WorkspaceName::parse("Gamma").unwrap(),
    };
    let kind = ActionKind::CreateWorkspace;
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::CreateWorkspace {
            workspace,
            request: request.clone(),
        },
        action_result(kind),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PUT", format!("/v1/workspaces/{workspace}").as_str())
    );
    assert_eq!(call.body, serde_json::json!({ "name": "Gamma" }));
    let Input::ActionResult {
        result: Ok(ActionOutcome::Joined(joined)),
        ..
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!(
        (joined.workspace.id, joined.workspace.name.as_str()),
        (workspace, "Gamma")
    );

    // A failure is reported once: the core decides whether to repeat (the id makes it safe).
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::CreateWorkspace { workspace, request },
        action_result(kind),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1, "one attempt");

    // Joining is one attempt too (it counts a use), and the code is in the body only.
    let code = lazy_protocol::InviteCode::new("join-code-for-the-hidden-place-01");
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::AcceptInvitation {
            request: lazy_protocol::AcceptInvitationRequest { code: code.clone() },
        },
        action_result(ActionKind::AcceptInvitation),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("POST", "/v1/invitations/accept")
    );
    assert_eq!(call.body, serde_json::json!({ "code": code.expose() }));
    assert!(!format!("{input:?}").contains(code.expose()));
    assert_eq!(call_count(&server), before + 1);
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Ok(ActionOutcome::Joined(_)),
            ..
        }
    ));
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, _) = run_effect(
        &mut client,
        &server,
        Effect::AcceptInvitation {
            request: lazy_protocol::AcceptInvitationRequest { code },
        },
        action_result(ActionKind::AcceptInvitation),
    );
    assert!(matches!(
        input,
        Input::ActionResult {
            result: Err(RequestError::Unknown(_)),
            ..
        }
    ));
    assert_eq!(call_count(&server), before + 1, "never repeated here");
    client.shutdown();
}

#[test]
fn a_name_change_is_a_patch_that_is_repeated_and_a_picture_is_one_put_of_the_raw_bytes() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::UpdateProfile {
            request: lazy_protocol::UpdateProfileRequest {
                display_name: Some(DisplayName::parse("Jules R").unwrap()),
            },
        },
        action_result(ActionKind::UpdateProfile),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PATCH", "/v1/me")
    );
    assert_eq!(call.body, serde_json::json!({ "display_name": "Jules R" }));
    assert_eq!(
        call_count(&server),
        before + 2,
        "repeated after a lost answer"
    );
    let Input::ActionResult {
        result: Ok(ActionOutcome::Profile(user)),
        ..
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!(user.display_name.as_str(), "Jules R");

    // A picture from bytes: the type is sniffed (a hint only), the body is the bytes.
    let png = {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0u8; 32]);
        bytes
    };
    let kind = ActionKind::SetAvatar;
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::SetAvatar {
            source: FileSource::Bytes(lazy_client_core::FileBytes::new(png.clone())),
        },
        action_result(kind),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("PUT", "/v1/me/avatar")
    );
    assert_eq!(call.body["len"], png.len());
    assert_eq!(call.body["content_type"], "image/png");
    assert!(matches!(
        input,
        Input::ActionResult { result: Ok(ActionOutcome::Profile(u)), .. } if u.avatar_version == 1
    ));

    // From a path: read here, off the core's thread.
    let file = client.profile.path().join("picture.bin");
    std::fs::write(&file, &png).unwrap();
    let before = call_count(&server);
    let (_, call) = run_effect(
        &mut client,
        &server,
        Effect::SetAvatar {
            source: FileSource::Path(file),
        },
        action_result(kind),
    );
    assert_eq!(call.body["len"], png.len());
    assert_eq!(call_count(&server), before + 1);

    // A picture over the limit is refused before it is read and before any request.
    let big = client.profile.path().join("big.bin");
    let handle = std::fs::File::create(&big).unwrap();
    handle
        .set_len(lazy_domain::workspace::MAX_AVATAR_UPLOAD_BYTES as u64 + 1)
        .unwrap();
    let before = call_count(&server);
    client.execute(vec![Effect::SetAvatar {
        source: FileSource::Path(big),
    }]);
    let input = client.expect("a refusal", action_result(kind));
    assert!(matches!(
        input,
        Input::ActionResult { result: Err(RequestError::Rejected(e)), .. }
            if e.code == ErrorCode::PayloadTooLarge
    ));
    assert_eq!(call_count(&server), before, "no request");
    // A path that is not there is a refusal with the reason.
    client.execute(vec![Effect::SetAvatar {
        source: FileSource::Path(client.profile.path().join("missing.png")),
    }]);
    let input = client.expect("a refusal", action_result(kind));
    assert!(matches!(
        input,
        Input::ActionResult { result: Err(RequestError::Rejected(e)), .. }
            if e.code == ErrorCode::InvalidRequest && e.message.contains("Couldn't read")
    ));

    // Removing is a delete that is asked again after a lost answer.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::ServerError);
    let before = call_count(&server);
    let (input, call) = run_effect(
        &mut client,
        &server,
        Effect::RemoveAvatar,
        action_result(kind),
    );
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("DELETE", "/v1/me/avatar")
    );
    assert_eq!(call_count(&server), before + 2);
    assert!(matches!(
        input,
        Input::ActionResult { result: Ok(ActionOutcome::Profile(u)), .. } if u.avatar_version == 0
    ));
    client.shutdown();
}

#[test]
fn an_avatar_is_fetched_at_its_version_cached_by_version_and_forgotten_on_request() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let person = UserId::generate();
    let fetch = |client: &mut Client, version: u32| {
        client.execute(vec![Effect::FetchAvatar {
            user: person,
            version,
        }]);
        client.expect(
            "an avatar",
            |i| matches!(i, Input::AvatarLoaded { version: v, .. } if *v == version),
        )
    };
    let before = call_count(&server);
    let input = fetch(&mut client, 2);
    let call = last_call(&server);
    assert_eq!(
        (call.method.as_str(), call.path.as_str()),
        ("GET", format!("/v1/users/{person}/avatar?v=2").as_str())
    );
    let Input::AvatarLoaded {
        user,
        version,
        result: Ok(data),
    } = input
    else {
        panic!("{input:?}");
    };
    assert_eq!((user, version), (person, 2));
    assert!(data.bytes().ends_with(b"avatar-2"));
    assert_eq!(call_count(&server), before + 1);

    // The same version again comes from the cache; another version is another file.
    fetch(&mut client, 2);
    assert_eq!(call_count(&server), before + 1, "cached");
    fetch(&mut client, 3);
    assert_eq!(call_count(&server), before + 2);
    // Forgetting a person's pictures asks the server again next time.
    client.execute(vec![Effect::ForgetAvatars {
        users: vec![person],
    }]);
    let mut asked = false;
    for _ in 0..50 {
        fetch(&mut client, 2);
        if call_count(&server) > before + 2 {
            asked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(asked, "forgotten");
    // A failure is a failure the core classifies.
    server
        .stub
        .message_plan
        .lock()
        .unwrap()
        .push_back(MessageBehavior::NotFound);
    client.execute(vec![Effect::FetchAvatar {
        user: person,
        version: 9,
    }]);
    let input = client.expect("a refusal", |i| matches!(i, Input::AvatarLoaded { .. }));
    assert!(matches!(
        input,
        Input::AvatarLoaded { result: Err(TransferError::Request(RequestError::Rejected(e))), .. }
            if e.code == ErrorCode::NotFound
    ));
    client.shutdown();
}

#[test]
fn the_appearance_and_the_active_workspace_are_saved_and_come_back_and_only_one_survives_sign_out()
{
    let server = Server::start();
    let (client, started) = Client::start(&server);
    let Input::Started(state) = started else {
        panic!("{started:?}");
    };
    assert_eq!(state.appearance, lazy_client_core::Appearance::default());
    assert_eq!(state.active_workspace, None);
    let dark = lazy_client_core::Appearance {
        theme: lazy_client_core::ThemeChoice::Dark,
        motion: lazy_client_core::MotionChoice::Reduce,
    };
    let workspace = WorkspaceId::generate();
    client.execute(vec![
        Effect::PersistAppearance(dark),
        Effect::PersistActiveWorkspace(Some(workspace)),
    ]);
    let appearance_file = client.profile.path().join("appearance.json");
    let active_file = client.profile.path().join("active-workspace.json");
    wait_until("both files", || {
        appearance_file.exists() && active_file.exists()
    });
    let (mut again, started) = client.restart(&server);
    let Input::Started(state) = started else {
        panic!("{started:?}");
    };
    assert_eq!(state.appearance, dark);
    assert_eq!(state.active_workspace, Some(workspace));

    // Signing out forgets the workspace (it names the person's) but not how the device looks.
    again.login();
    again.execute(vec![Effect::SignOut]);
    wait_until("the workspace to be forgotten", || !active_file.exists());
    assert!(appearance_file.exists(), "the device's appearance stays");
    again.execute(vec![
        Effect::PersistActiveWorkspace(Some(workspace)),
        Effect::SignOut,
    ]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !active_file.exists(),
        "a save waiting does not bring it back"
    );
    again.shutdown();
}

#[test]
fn what_the_person_was_looking_at_and_their_carets_survive_a_restart() {
    let server = Server::start();
    let (mut client, _) = Client::start(&server);
    client.login();
    let (workspace, conversation) = (server.stub.workspace, ConversationId::generate());
    let draft_conversation = ConversationId::generate();
    let view = lazy_client_core::ViewState {
        last_selected: vec![(workspace, conversation)],
        draft_selections: vec![lazy_client_core::DraftSelection {
            conversation: draft_conversation,
            selection: lazy_client_core::TextSelection { anchor: 2, head: 5 },
        }],
    };
    let thread = lazy_client_core::ThreadDraft {
        root: MessageId::generate(),
        conversation: draft_conversation,
        text: "a reply".into(),
        selection: Some(lazy_client_core::TextSelection { anchor: 1, head: 1 }),
    };
    client.execute(vec![
        Effect::PersistDrafts(vec![(draft_conversation, "a draft".into())]),
        Effect::PersistThreadDrafts(vec![thread.clone()]),
        Effect::PersistView(view.clone()),
    ]);
    // The saver writes after a short pause; shutting down flushes it.
    client.shutdown();
    let (client, started) = client.restart(&server);
    let Input::Started(state) = started else {
        panic!("expected Started");
    };
    assert_eq!(state.view, view);
    assert_eq!(state.thread_drafts, vec![thread]);
    assert_eq!(
        state.drafts.get(&draft_conversation).map(String::as_str),
        Some("a draft")
    );
    drop(client);
}

// ----- What is logged (slice 3I, D24) -----

/// Runs a scenario that types the same unusual words into every kind of field, makes requests that
/// succeed and fail in every way the stub can, and then reads everything the runtime logged (at
/// TRACE, including the `Debug` of every effect and input) for those words, for the session
/// token, and for the server's address.
#[test]
fn nothing_the_runtime_logs_holds_what_was_typed_or_any_secret() {
    // The capturing subscriber is installed for the whole process, and it would slow and
    // reorder every other test running beside this one (a few of them race a cancel against a
    // reply, and a tracing subscriber that formats every event widens that race). So the
    // scenario runs in a process of its own: this test runs the same test binary again, for this
    // test alone.
    const CHILD: &str = "LAZY_LOG_SCENARIO_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "nothing_the_runtime_logs_holds_what_was_typed_or_any_secret",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("the test binary runs again");
        assert!(
            output.status.success(),
            "the logging scenario failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    capture::install();
    let server = Server::start();
    *server.stub.bad_frame.lock().unwrap() = Some(format!(r#"{{"type":"{LEAKY_WORDS}"}}"#));
    let (mut client, _) = Client::start(&server);
    let files = tempfile::Builder::new()
        .prefix("quokka-zephyr-")
        .tempdir()
        .unwrap();
    let report = files.path().join("quokka-zephyr-report.pdf");
    std::fs::write(&report, b"quokka zephyr file body").unwrap();
    let picture = files.path().join("quokka-zephyr-face.png");
    std::fs::write(&picture, b"quokka zephyr picture").unwrap();
    let words = |what: &str| format!("{LEAKY_WORDS} {what}");

    // Register with an invitation code, then sign in, with a password and a username to match.
    let username = Username::parse("quokka_zephyr").unwrap();
    client.execute(vec![Effect::Register(lazy_protocol::RegisterRequest {
        username: username.clone(),
        display_name: DisplayName::parse("Quokka Zephyr").unwrap(),
        password: lazy_protocol::Password::new("quokka-zephyr-password"),
        invite_code: Some(lazy_protocol::InviteCode::new("quokka-zephyr-invitation")),
    })]);
    client.expect("registered", |i| matches!(i, Input::Authenticated { .. }));
    client.execute(vec![Effect::Login(LoginRequest {
        username,
        password: lazy_protocol::Password::new("quokka-zephyr-password"),
    })]);
    client.expect("signed in", |i| matches!(i, Input::Authenticated { .. }));
    client.execute(vec![Effect::LoadBootstrap]);
    client.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)));

    // A live connection, which also receives a frame that is not one (and that quotes the words).
    let workspace = server.stub.workspace;
    client.execute(vec![Effect::Subscribe {
        workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    client.expect("the connection", |i| {
        matches!(i, Input::Socket(SocketEvent::Opened { .. }))
    });
    wait_until("the stray frame to be noticed", || {
        capture::lines()
            .iter()
            .any(|line| line.contains("could not be parsed"))
    });

    // Messages: sent, refused by a broken server, and answered with a body that quotes the words.
    let conversation = ConversationId::generate();
    for behavior in [
        SendBehavior::Ok,
        SendBehavior::ServerError,
        SendBehavior::MalformedEcho,
    ] {
        let failing = !matches!(behavior, SendBehavior::Ok);
        server.stub.send_plan.lock().unwrap().push_back(behavior);
        let operation = OperationId::generate();
        client.execute(vec![Effect::SendMessage {
            operation,
            conversation,
            document: MessageDocument::from_plain_text(&words("message")).unwrap(),
            thread_root: None,
            also_in_channel: false,
            attachments: Vec::new(),
        }]);
        let Input::SendResult { result, .. } = client.expect(
            "the send's result",
            |i| matches!(i, Input::SendResult { operation: o, .. } if *o == operation),
        ) else {
            unreachable!()
        };
        if let Err(error) = result {
            // What the person would be told, and what a log would be told, name nothing.
            let told = error.message().to_lowercase();
            assert!(
                !told.contains("quokka") && !told.contains("zephyr"),
                "{told}"
            );
            assert!(failing, "an ordinary send is answered");
        }
    }
    let message = MessageId::generate();
    let edit = OperationId::generate();
    client.execute(vec![edit_effect(message, edit, 1, &words("edit"))]);
    client.expect("the edit's result", edit_result(edit));
    client.execute(vec![Effect::DeleteMessage { message }]);
    client.expect("the deletion's result", |i| {
        matches!(i, Input::DeleteResult { .. })
    });
    client.execute(vec![Effect::SetMessageSaved {
        message,
        request: lazy_protocol::SetSavedRequest { saved: true },
    }]);
    client.expect("the save's result", |i| {
        matches!(i, Input::MessageSavedResult { .. })
    });

    // A file with a distinctive name and path: copied into the outbox, then uploaded to a stub
    // that answers with something that is not an attachment.
    let attachment = lazy_domain::AttachmentId::generate();
    client.execute(vec![Effect::StageFile {
        attachment,
        generation: 1,
        source: FileSource::Path(report),
    }]);
    client.expect("the copy", |i| matches!(i, Input::FileStaged { .. }));
    client.execute(vec![Effect::UploadFile {
        attachment,
        conversation,
        generation: 1,
        name: "quokka-zephyr-report.pdf".to_owned(),
        media_type: "application/pdf".to_owned(),
        size: 23,
    }]);
    let Input::UploadResult { result, .. } = client.expect("the upload's result", |i| {
        matches!(i, Input::UploadResult { .. })
    }) else {
        unreachable!()
    };
    assert!(result.is_err(), "the stub's answer is not an attachment");

    // A search whose text is in the URL's query string, and which the stub answers with nonsense.
    client.execute(vec![Effect::Search {
        workspace,
        generation: 1,
        params: lazy_protocol::SearchParams {
            q: words("search"),
            limit: None,
            offset: None,
            sort: None,
        },
    }]);
    let Input::SearchResult { result, .. } = client.expect("the search's result", |i| {
        matches!(i, Input::SearchResult { .. })
    }) else {
        unreachable!()
    };
    let Err(error) = result else {
        panic!("the stub's answer is not search results")
    };
    let told = error.message().to_lowercase();
    assert!(
        !told.contains("quokka") && !told.contains("zephyr"),
        "{told}"
    );

    // Status and profile.
    client.execute(vec![Effect::SetStatus {
        workspace,
        request: lazy_protocol::SetStatusRequest {
            status: Some(
                lazy_domain::notification::UserStatus::new(&words("status"), None, None).unwrap(),
            ),
        },
    }]);
    client.expect("the status", |i| matches!(i, Input::StatusResult { .. }));
    client.execute(vec![Effect::UpdateProfile {
        request: lazy_protocol::UpdateProfileRequest {
            display_name: Some(DisplayName::parse("Quokka Zephyr").unwrap()),
        },
    }]);
    client.expect("the profile", action_result(ActionKind::UpdateProfile));
    client.execute(vec![Effect::SetAvatar {
        source: FileSource::Path(picture),
    }]);
    client.expect("the avatar", action_result(ActionKind::SetAvatar));
    client.shutdown();

    let lines = capture::lines();
    let said = |needle: &str| lines.iter().any(|line| line.contains(needle));
    // The test looked at something: the runtime did log what it did, in the ways it should.
    assert!(said("executing an effect"), "effects are traced");
    assert!(said("input received"), "inputs are traced");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("request answered") && l.contains("/v1/auth/register")),
        "requests are logged by route"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("/v1/workspaces/:id/search") && l.contains("status=")),
        "a route is a pattern, with its status"
    );
    assert!(said("realtime connection open"));
    assert!(said("could not be parsed"));
    assert!(said("an attachment upload failed"));
    assert!(said("repeating a read after an unknown outcome"));

    // And none of it holds the words or the token (the libraries underneath included: the HTTP
    // client's own events name the server's host and port, which is no secret, but must never
    // name a query or a header), nor, in the runtime's own lines, an address at all. The stub's
    // own library (axum) is not the runtime's and is not looked at.
    let ours = |line: &str| line.contains(" lazy_client_") || line.starts_with("span lazy_client_");
    let leaks: Vec<&str> = lines
        .iter()
        .map(String::as_str)
        .filter(|line| !line.contains(" axum") && !line.starts_with("span axum"))
        .filter(|line| {
            let lower = line.to_lowercase();
            let secrets = ["quokka", "zephyr", "tok-", "bearer"];
            let addresses = ["http://", "127.0.0.1"];
            secrets.iter().any(|needle| lower.contains(needle))
                || (ours(line) && addresses.iter().any(|needle| lower.contains(needle)))
        })
        .collect();
    assert!(
        leaks.is_empty(),
        "the log holds secrets:\n{}",
        leaks.join("\n")
    );
}

// ----- Choosing the server (dogfood packaging) -----

/// A runtime whose server the person may change: `server_url` is only the default.
fn chooser(default: &Server, profile: TempDir, storage: Arc<MemoryStorage>) -> (Client, Input) {
    let (runtime, inputs) = Runtime::start(RuntimeConfig {
        server_locked: false,
        ..fast_config(&default.url, &profile, storage.clone())
    })
    .expect("runtime starts");
    let mut client = Client {
        runtime: Some(runtime),
        inputs,
        profile,
        storage,
        url: default.url.clone(),
        skipped: Vec::new(),
    };
    let started = client.expect("Started", |i| matches!(i, Input::Started(_)));
    (client, started)
}

fn started_state(input: Input) -> lazy_client_core::StartupState {
    let Input::Started(state) = input else {
        panic!("expected Started")
    };
    state
}

/// Asks a server which protocol it speaks, through the runtime.
fn ask(client: &mut Client, url: &str) -> Result<u32, String> {
    client.execute(vec![Effect::CheckServer {
        url: url.to_owned(),
        generation: 7,
    }]);
    let Input::ServerChecked { generation, result } =
        client.expect("the check", |i| matches!(i, Input::ServerChecked { .. }))
    else {
        unreachable!()
    };
    assert_eq!(generation, 7, "the answer echoes the question");
    result
}

/// Moves the runtime to a server, through the runtime.
fn move_to(client: &mut Client, url: &str) -> Result<Box<lazy_client_core::StartupState>, String> {
    client.execute(vec![Effect::UseServer {
        url: url.to_owned(),
        generation: 8,
    }]);
    let Input::ServerSwitched { generation, result } =
        client.expect("the move", |i| matches!(i, Input::ServerSwitched { .. }))
    else {
        unreachable!()
    };
    assert_eq!(generation, 8);
    result
}

fn auth_of_last_request(server: &Server) -> Option<String> {
    server
        .stub
        .requests
        .lock()
        .unwrap()
        .last()
        .expect("the stub saw a request")
        .1
        .clone()
}

#[test]
fn the_environment_wins_over_a_saved_choice_which_wins_over_the_default() {
    use lazy_client_core::ServerSource;
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let file = profile.path().join("server.json");

    // Nothing chosen: the default.
    let storage = MemoryStorage::new();
    let (mut client, started) = chooser(&a, profile, storage.clone());
    let state = started_state(started);
    assert_eq!(
        (state.server.url.as_str(), state.server.source),
        (a.url.as_str(), ServerSource::Default)
    );
    client.shutdown();
    let profile = client.profile;

    // A choice was saved: it wins over the default.
    std::fs::write(
        &file,
        format!(r#"{{"version": 1, "server_url": "{}"}}"#, b.url),
    )
    .unwrap();
    let (mut client, started) = chooser(&a, profile, storage.clone());
    let state = started_state(started);
    assert_eq!(
        (state.server.url.as_str(), state.server.source),
        (b.url.as_str(), ServerSource::Saved)
    );
    client.shutdown();
    let profile = client.profile;

    // The environment (or whoever started the runtime) wins over both, and leaves the file be.
    let before = std::fs::read(&file).unwrap();
    let (mut client, started) = Client::start_with(&a, profile, storage.clone());
    let state = started_state(started);
    assert_eq!(
        (state.server.url.as_str(), state.server.source),
        (a.url.as_str(), ServerSource::Environment)
    );
    assert_eq!(std::fs::read(&file).unwrap(), before);
    client.shutdown();
}

#[test]
fn a_damaged_or_unknown_choice_is_the_same_as_none() {
    use lazy_client_core::ServerSource;
    let a = Server::start();
    for content in [
        "not json".to_owned(),
        r#"{"version": 2, "server_url": "https://elsewhere.example.com"}"#.to_owned(),
        r#"{"server_url": "https://elsewhere.example.com"}"#.to_owned(),
        r#"{"version": 1, "server_url": "https://user:pw@elsewhere.example.com"}"#.to_owned(),
        String::new(),
    ] {
        let profile = tempfile::tempdir().unwrap();
        std::fs::write(profile.path().join("server.json"), &content).unwrap();
        let (mut client, started) = chooser(&a, profile, MemoryStorage::new());
        let state = started_state(started);
        assert_eq!(
            (state.server.url.as_str(), state.server.source),
            (a.url.as_str(), ServerSource::Default),
            "{content}"
        );
        assert!(
            state.problems.is_empty(),
            "no alarm for {content}: {:?}",
            state.problems
        );
        client.shutdown();
    }
}

#[test]
fn a_profile_from_before_the_choice_existed_keeps_its_session() {
    // A session saved by a build that knew nothing of the choice (the address came from the
    // environment, or the default) is found again by one that does: the secrets were always kept
    // under the profile and the server's address, and still are.
    let a = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = Client::start_with(&a, profile, storage.clone());
    client.login();
    client.shutdown();
    let profile = client.profile;
    assert!(
        !profile.path().join("server.json").exists(),
        "the premise: no file"
    );

    let (mut client, started) = chooser(&a, profile, storage);
    let state = started_state(started);
    assert!(
        state.session_restored,
        "the session is found under the same key"
    );
    client.shutdown();
}

#[test]
fn a_token_for_one_server_is_never_sent_to_another() {
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = Client::start_with(&a, profile, storage.clone());
    client.login();
    client.shutdown();
    let profile = client.profile;

    // The same profile and credential store, another server: no session, no Authorization.
    let (mut other, started) = Client::start_with(&b, profile, storage.clone());
    assert!(!started_state(started).session_restored);
    other.execute(vec![Effect::LoadBootstrap]);
    other.expect("the refusal", |i| matches!(i, Input::BootstrapFailed(_)));
    assert_eq!(
        auth_of_last_request(&b),
        None,
        "no token was offered to the other server"
    );
    other.shutdown();
    let profile = other.profile;

    // Back on the first, the session is still there.
    let (mut again, started) = Client::start_with(&a, profile, storage);
    assert!(started_state(started).session_restored);
    again.execute(vec![Effect::LoadBootstrap]);
    again.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)));
    assert!(auth_of_last_request(&a).is_some_and(|auth| auth.starts_with("Bearer tok-")));
    again.shutdown();
}

#[test]
fn a_server_is_asked_which_protocol_it_speaks() {
    let a = Server::start();
    let (mut client, _) = chooser(&a, tempfile::tempdir().unwrap(), MemoryStorage::new());
    assert_eq!(ask(&mut client, &a.url), Ok(PROTOCOL_VERSION));
    assert_eq!(
        auth_of_last_request(&a),
        None,
        "no session goes with the question"
    );
    // Another protocol is an answer, not a failure: the core decides what it means.
    *a.stub.health.lock().unwrap() = HealthMode::Version(PROTOCOL_VERSION + 3);
    assert_eq!(ask(&mut client, &a.url), Ok(PROTOCOL_VERSION + 3));
    // A reverse proxy's path prefix is part of the address.
    *a.stub.health.lock().unwrap() = HealthMode::Version(PROTOCOL_VERSION);
    assert_eq!(
        ask(&mut client, &format!("{}/lazy", a.url)),
        Ok(PROTOCOL_VERSION)
    );
    client.shutdown();
}

#[test]
fn a_server_that_is_not_there_or_not_lazy_is_told_in_fixed_words() {
    let a = Server::start();
    let (mut client, _) = chooser(&a, tempfile::tempdir().unwrap(), MemoryStorage::new());
    // Nothing listens (port 1 is never handed out to a connection).
    assert_eq!(
        ask(&mut client, "http://127.0.0.1:1"),
        Err("connection failed".to_owned())
    );
    // Something that is not a Lazy server.
    *a.stub.health.lock().unwrap() = HealthMode::NotJson;
    let not_lazy = Err("there is no Lazy server at this address".to_owned());
    assert_eq!(ask(&mut client, &a.url), not_lazy);
    assert_eq!(
        ask(&mut client, &format!("{}/nowhere", a.url)),
        not_lazy,
        "a 404"
    );
    *a.stub.health.lock().unwrap() = HealthMode::Error;
    assert_eq!(
        ask(&mut client, &a.url),
        Err("the server answered with an error".to_owned())
    );
    // None of the phrases quotes the address.
    for url in ["http://127.0.0.1:1", &a.url] {
        let told = ask(&mut client, url).unwrap_err();
        assert!(
            !told.contains("127.0.0.1") && !told.contains("http"),
            "{told}"
        );
    }
    client.shutdown();
}

#[test]
fn a_redirect_is_followed_only_to_the_same_host() {
    let (a, b) = (Server::start(), Server::start());
    let (mut client, _) = chooser(&a, tempfile::tempdir().unwrap(), MemoryStorage::new());
    // Same host: followed.
    assert_eq!(
        ask(&mut client, &format!("{}/redirected", a.url)),
        Ok(PROTOCOL_VERSION)
    );
    // Another host (the same machine under another name): not followed, and b is never asked.
    let elsewhere = b.url.replace("127.0.0.1", "localhost");
    *a.stub.health.lock().unwrap() = HealthMode::RedirectTo(format!("{elsewhere}/healthz"));
    let before = b.stub.requests.lock().unwrap().len();
    assert_eq!(
        ask(&mut client, &a.url),
        Err("it redirects somewhere else".to_owned())
    );
    assert_eq!(b.stub.requests.lock().unwrap().len(), before);
    client.shutdown();
}

#[test]
fn moving_to_a_server_sends_everything_there_and_nothing_to_the_old_one() {
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = chooser(&a, profile, storage.clone());
    assert_eq!(ask(&mut client, &b.url), Ok(PROTOCOL_VERSION));

    let state = move_to(&mut client, &b.url).expect("the move");
    assert_eq!(state.server.url, b.url);
    assert_eq!(state.server.source, lazy_client_core::ServerSource::Saved);
    assert!(!state.session_restored);
    // Saved, and nothing else in the file.
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(client.profile.path().join("server.json")).unwrap())
            .unwrap();
    assert_eq!(
        saved,
        serde_json::json!({ "version": 1, "server_url": b.url })
    );

    // Requests go to the new server: the sign-in, the load, and the realtime connection.
    let a_before = a.stub.requests.lock().unwrap().len();
    client.execute(vec![Effect::Login(LoginRequest {
        username: Username::parse("stubby").unwrap(),
        password: lazy_protocol::Password::new("password-1"),
    })]);
    client.expect("signed in", |i| matches!(i, Input::Authenticated { .. }));
    client.execute(vec![Effect::LoadBootstrap]);
    client.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)));
    client.execute(vec![Effect::Subscribe {
        workspace: b.stub.workspace,
        after: EventSeq::new(10).unwrap(),
    }]);
    client.expect("the connection", |i| {
        matches!(i, Input::Socket(SocketEvent::Opened { .. }))
    });
    assert_eq!(b.stub.logins.load(Ordering::SeqCst), 1);
    assert_eq!(
        b.stub.upgrades.load(Ordering::SeqCst),
        1,
        "the realtime URL moved too"
    );
    assert_eq!(a.stub.logins.load(Ordering::SeqCst), 0);
    assert_eq!(a.stub.upgrades.load(Ordering::SeqCst), 0);
    assert_eq!(
        a.stub.requests.lock().unwrap().len(),
        a_before,
        "nothing more reached the old one"
    );
    client.shutdown();
}

#[test]
fn moving_leaves_no_session_behind_and_sends_none_to_the_new_server() {
    // Even if the core were bypassed while signed in, the old session goes nowhere.
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = chooser(&a, profile, storage.clone());
    client.login();
    let old_vault = SecretVault::new(storage.clone(), client.profile.path(), &a.url);
    assert!(old_vault.session_token().unwrap().is_some());

    move_to(&mut client, &b.url).expect("the move");
    assert!(
        old_vault.session_token().unwrap().is_none(),
        "the old server's session is deleted"
    );
    assert_eq!(
        storage
            .keys()
            .iter()
            .filter(|key| key.contains(old_vault.namespace()))
            .count(),
        0,
        "and so is its key: {:?}",
        storage.keys()
    );
    client.execute(vec![Effect::LoadBootstrap]);
    client.expect("the refusal", |i| matches!(i, Input::BootstrapFailed(_)));
    assert_eq!(
        auth_of_last_request(&b),
        None,
        "no token reached the new server"
    );
    client.shutdown();
}

#[test]
fn the_choice_and_the_new_servers_state_survive_a_restart() {
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = chooser(&a, profile, storage.clone());
    move_to(&mut client, &b.url).expect("the move");
    client.login();
    // State written after the move is kept under the new server's key.
    let conversation = ConversationId::generate();
    client.execute(vec![Effect::PersistDrafts(vec![(
        conversation,
        "a draft".into(),
    )])]);
    client.shutdown();
    let profile = client.profile;

    let (mut client, started) = chooser(&a, profile, storage);
    let state = started_state(started);
    assert_eq!(
        (state.server.url.as_str(), state.server.source),
        (b.url.as_str(), lazy_client_core::ServerSource::Saved),
        "the default (a) is not used: the saved choice wins"
    );
    assert!(state.session_restored, "the session on the new server");
    assert_eq!(
        state.drafts.get(&conversation).map(String::as_str),
        Some("a draft")
    );
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    client.execute(vec![Effect::LoadBootstrap]);
    client.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)));
    assert_eq!(
        a.stub.requests.lock().unwrap().len(),
        0,
        "the old server heard nothing"
    );
    client.shutdown();
}

#[test]
fn moving_deletes_what_the_old_server_left_in_the_profile() {
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = chooser(&a, profile, storage.clone());
    client.login();
    let conversation = ConversationId::generate();
    client.execute(vec![Effect::PersistDrafts(vec![(
        conversation,
        "old".into(),
    )])]);
    let key = SecretVault::new(storage.clone(), client.profile.path(), &a.url)
        .state_key()
        .unwrap();
    let profile_path = client.profile.path().to_owned();
    wait_until("the draft on disk", || {
        inspect_state(&profile_path, &key)
            .drafts
            .contains_key(&conversation)
    });

    let state = move_to(&mut client, &b.url).expect("the move");
    assert!(state.drafts.is_empty() && state.journal.is_empty() && state.cache.is_none());
    assert!(
        inspect_state(&profile_path, &key).drafts.is_empty(),
        "wiped from disk"
    );
    assert!(
        state.problems.is_empty(),
        "no alarm about the old files: {:?}",
        state.problems
    );
    client.shutdown();
}

#[test]
fn a_choice_that_cannot_be_saved_changes_nothing() {
    let (a, b) = (Server::start(), Server::start());
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = chooser(&a, profile, storage.clone());
    client.login();
    // The file cannot be written: something else is in its place.
    std::fs::create_dir(client.profile.path().join("server.json")).unwrap();
    let failed = move_to(&mut client, &b.url).unwrap_err();
    assert_eq!(failed, "the choice could not be saved");
    // Still on the first server, still signed in there.
    client.execute(vec![Effect::LoadBootstrap]);
    client.expect("bootstrap", |i| matches!(i, Input::BootstrapLoaded(_)));
    assert!(auth_of_last_request(&a).is_some());
    assert_eq!(b.stub.requests.lock().unwrap().len(), 0);
    client.shutdown();
}

#[test]
fn a_server_set_by_the_environment_is_not_moved_by_a_stray_effect() {
    let (a, b) = (Server::start(), Server::start());
    let (mut client, _) = Client::start(&a);
    let failed = move_to(&mut client, &b.url).unwrap_err();
    assert!(failed.contains("environment"), "{failed}");
    client.login();
    assert_eq!(a.stub.logins.load(Ordering::SeqCst), 1);
    assert_eq!(b.stub.logins.load(Ordering::SeqCst), 0);
    assert!(!client.profile.path().join("server.json").exists());
    client.shutdown();
}

#[test]
fn what_is_saved_after_signing_out_and_in_again_is_still_readable_after_a_restart() {
    let a = Server::start();
    let profile = tempfile::tempdir().unwrap();
    let storage = MemoryStorage::new();
    let (mut client, _) = Client::start_with(&a, profile, storage.clone());
    client.login();
    client.execute(vec![Effect::SignOut]);
    // Sign-out deletes the secrets, including the key the state is encrypted with.
    let vault = client.vault();
    wait_until("the secrets to be deleted", || {
        vault.session_token().unwrap().is_none()
    });
    client.login();
    let conversation = ConversationId::generate();
    client.execute(vec![Effect::PersistDrafts(vec![(
        conversation,
        "after".into(),
    )])]);
    client.shutdown();
    let profile = client.profile;

    let (mut client, started) = Client::start_with(&a, profile, storage);
    let state = started_state(started);
    assert!(state.problems.is_empty(), "{:?}", state.problems);
    assert_eq!(
        state.drafts.get(&conversation).map(String::as_str),
        Some("after")
    );
    client.shutdown();
}
