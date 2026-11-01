//! The HTTP API: routes, request extractors, and handlers. Handlers only translate between HTTP
//! and the service modules; the rules live in those modules.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::header::{AUTHORIZATION, RETRY_AFTER};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use futures_util::FutureExt;
use lazy_domain::{
    AttachmentId, ConversationId, EmojiId, GroupId, InvitationId, MAX_WEBHOOK_BODY_BYTES,
    MessageId, OperationId, UserId, WebhookId, WorkspaceId,
};
use lazy_protocol::api::paths;
use lazy_protocol::{
    AcceptInvitationRequest, CreateWorkspaceRequest, InvitationPreview, JoinedWorkspace,
    PreviewInvitationRequest, ProfileResponse, ServerInfoResponse, UpdateProfileRequest,
};
use lazy_protocol::{
    AddMembersRequest, AttachmentResponse, BootstrapResponse, BrowseChannelsQuery,
    BrowseChannelsResponse, ConversationMembersResponse, ConversationNotificationPrefsResponse,
    ConversationResponse, CreateChannelRequest, CreateGroupRequest, CreateInvitationRequest,
    CreateWebhookRequest, CreatedInvitation, CreatedWebhook, CustomEmojiResponse,
    EditMessageRequest, ErrorCode, GroupResponse, GroupsResponse, HealthResponse, HistoryPage,
    HistoryQuery, InboxPage, InboxQuery, InboxReadResponse, IncomingWebhookResponse,
    InvitationList, InvitationResponse, LoginRequest, MarkReadRequest, MarkThreadReadRequest,
    MessageResponse, NotificationSettingsResponse, OpenConversationRequest, OperationStatus,
    PROTOCOL_VERSION, PeoplePage, PeopleQuery, PersonResponse, RegisterRequest, RenameEmojiRequest,
    SavedPage, SavedQuery, SavedResponse, SearchParams, SearchResponse, SendMessageRequest,
    SendMessageResponse, SessionResponse, SetConversationNotificationPrefsRequest,
    SetDeactivatedRequest, SetFavoriteRequest, SetReactionRequest, SetRoleRequest, SetSavedRequest,
    SetStatusRequest, SetThreadFollowRequest, StatusResponse, ThreadStateResponse,
    UpdateConversationRequest, UpdateGroupRequest, UpdateWorkspaceRequest, UserStateResponse,
    WebhookDeliveriesResponse, WebhookDeliveryOutcome, WebhooksResponse, WorkspaceResponse,
    WorkspaceWebhooksResponse,
};
use serde::de::DeserializeOwned;
use tower_http::timeout::TimeoutLayer;

use crate::accounts::{self, Principal};
use crate::app::AppState;
use crate::error::AppError;
use crate::{
    admin, attachments, bootstrap, conversations, discovery, emoji, groups, logging, messages,
    notifications, onboarding, profile, realtime, search, threads, webhooks, workspaces,
};

/// Largest accepted request body.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest a request may take before the server gives up on it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest accepted WebSocket message from a client.
const MAX_SOCKET_MESSAGE_BYTES: usize = 64 * 1024;

/// Longest an upload or download may take: a file of the largest size over a slow connection.
const FILE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub fn router(state: AppState) -> Router {
    // Files get their own, much longer, timeout; everything else has `REQUEST_TIMEOUT`.
    let files = Router::new()
        .route(paths::ATTACHMENT_UPLOAD_ROUTE, put(upload_attachment))
        .route(paths::EMOJI_UPLOAD_ROUTE, put(upload_emoji))
        .route(
            paths::ME_AVATAR_ROUTE,
            put(upload_avatar).delete(delete_avatar),
        )
        .route(paths::USER_AVATAR_ROUTE, get(download_avatar))
        .route(paths::ATTACHMENT_ROUTE, get(download_attachment))
        .route(
            paths::ATTACHMENT_THUMBNAIL_ROUTE,
            get(download_attachment_thumbnail),
        )
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            FILE_TRANSFER_TIMEOUT,
        ));
    let api = Router::new()
        .route(paths::HEALTH, get(health))
        .route(paths::SERVER_INFO, get(server_info))
        .route(paths::PREVIEW_INVITATION, post(preview_invitation))
        .route(paths::REGISTER, post(register))
        .route(paths::LOGIN, post(login))
        .route(paths::LOGOUT, post(logout))
        .route(paths::BOOTSTRAP, get(bootstrap_snapshot))
        .route(paths::REALTIME, get(realtime_upgrade))
        .route(paths::MESSAGES_ROUTE, get(history).post(send_message))
        .route(paths::OPERATION_ROUTE, get(operation_status))
        .route(
            paths::MESSAGE_ROUTE,
            get(get_message).patch(edit_message).delete(delete_message),
        )
        .route(paths::REACTIONS_ROUTE, put(set_reaction))
        .route(
            paths::WEBHOOKS_ROUTE,
            post(create_webhook).get(list_webhooks),
        )
        .route(paths::WEBHOOK_ROUTE, delete(delete_webhook))
        .route(
            paths::WORKSPACE_WEBHOOKS_ROUTE,
            get(list_workspace_webhooks),
        )
        .route(paths::WEBHOOK_DELIVERIES_ROUTE, get(webhook_deliveries))
        .route(paths::WEBHOOK_ADDRESS_ROUTE, post(new_webhook_address))
        .route(
            paths::GITHUB_ROUTE,
            get(github_status).delete(github_disconnect),
        )
        .route(paths::GITHUB_SETUP_ROUTE, post(github_setup))
        .route(paths::GITHUB_SUBSCRIPTIONS_ROUTE, post(github_subscribe))
        .route(paths::GITHUB_SUBSCRIPTION_ROUTE, delete(github_unsubscribe))
        .route(paths::GITHUB_DELIVERY_RETRY_ROUTE, post(github_retry))
        .route(paths::GITHUB_SETUP_PAGE_ROUTE, get(github_setup_page))
        .route(paths::GITHUB_CALLBACK_ROUTE, get(github_callback))
        .route(paths::GITHUB_INSTALLED_ROUTE, get(github_installed))
        .route(paths::GITHUB_WEBHOOK_ROUTE, post(github_webhook))
        .route(paths::HOOK_ROUTE, post(incoming_webhook))
        .route(
            paths::EMOJI_ROUTE,
            get(download_emoji).patch(rename_emoji).delete(delete_emoji),
        )
        .route(paths::SEARCH_ROUTE, get(search_workspace))
        .route(
            paths::WORKSPACE_ROUTE,
            patch(update_workspace).put(create_workspace),
        )
        .route(paths::ME_ROUTE, patch(update_profile))
        .route(paths::ACCEPT_INVITATION_ROUTE, post(accept_invitation))
        .route(paths::PERSON_ROLE_ROUTE, put(put_person_role))
        .route(paths::PERSON_DEACTIVATED_ROUTE, put(put_person_deactivated))
        .route(
            paths::INVITATIONS_ROUTE,
            get(list_invitations).post(create_invitation),
        )
        .route(paths::INVITATION_ROUTE, delete(revoke_invitation))
        .route(
            paths::NOTIFICATION_SETTINGS_ROUTE,
            put(put_notification_settings),
        )
        .route(
            paths::CONVERSATION_NOTIFICATION_ROUTE,
            put(put_conversation_notification_prefs),
        )
        .route(paths::STATUS_ROUTE, put(put_status))
        .route(paths::INBOX_ROUTE, get(get_inbox))
        .route(paths::INBOX_READ_ROUTE, post(post_inbox_read))
        .route(paths::REPLIES_ROUTE, get(replies))
        .route(paths::THREAD_FOLLOW_ROUTE, put(set_thread_follow))
        .route(paths::THREAD_READ_ROUTE, post(mark_thread_read))
        .route(paths::GROUPS_ROUTE, post(create_group).get(list_groups))
        .route(paths::GROUP_ROUTE, patch(update_group).delete(delete_group))
        .route(paths::CHANNELS_ROUTE, post(create_channel))
        .route(paths::BROWSE_CHANNELS_ROUTE, get(browse_channels))
        .route(paths::PEOPLE_ROUTE, get(list_people))
        .route(paths::PERSON_ROUTE, get(get_person))
        .route(paths::SAVED_ROUTE, get(list_saved))
        .route(paths::MESSAGE_SAVED_ROUTE, put(put_message_saved))
        .route(paths::OPEN_CONVERSATION_ROUTE, post(open_conversation))
        .route(paths::CONVERSATION_ROUTE, patch(update_conversation))
        .route(paths::JOIN_ROUTE, post(join_channel))
        .route(paths::LEAVE_ROUTE, post(leave_channel))
        .route(paths::ARCHIVE_ROUTE, post(archive_channel))
        .route(paths::UNARCHIVE_ROUTE, post(unarchive_channel))
        .route(
            paths::MEMBERS_ROUTE,
            post(add_members).get(conversation_members),
        )
        .route(paths::MEMBER_ROUTE, delete(remove_member))
        .route(paths::FAVORITE_ROUTE, put(set_favorite))
        .route(paths::READ_ROUTE, post(mark_read))
        .fallback(unknown_route)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ));
    api.merge(files)
        // Outermost: every request is logged by its matched route pattern (never its path, which
        // can hold a webhook secret), given an id, and a panic in a handler becomes a logged 500.
        .layer(axum::middleware::from_fn(logging::log_requests))
        .with_state(state)
}

async fn unknown_route() -> AppError {
    AppError::not_found("no such route")
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        protocol_version: PROTOCOL_VERSION,
    })
}

async fn server_info(State(state): State<AppState>) -> Result<Json<ServerInfoResponse>, AppError> {
    onboarding::server_info(&state).await.map(Json)
}

async fn preview_invitation(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<PreviewInvitationRequest>,
) -> Result<Json<InvitationPreview>, AppError> {
    onboarding::preview_invitation(&state, request)
        .await
        .map(Json)
}

async fn register(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<RegisterRequest>,
) -> Result<Json<SessionResponse>, AppError> {
    accounts::register(&state, request).await.map(Json)
}

async fn login(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<LoginRequest>,
) -> Result<Json<SessionResponse>, AppError> {
    accounts::login(&state, request).await.map(Json)
}

async fn logout(
    State(state): State<AppState>,
    Auth(principal): Auth,
) -> Result<StatusCode, AppError> {
    accounts::logout(&state.pool, principal).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn bootstrap_snapshot(
    State(state): State<AppState>,
    Auth(principal): Auth,
) -> Result<Json<BootstrapResponse>, AppError> {
    bootstrap::bootstrap(&state.pool, principal.user_id)
        .await
        .map(Json)
}

async fn send_message(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>, AppError> {
    let message =
        messages::send_message(&state.pool, principal.user_id, conversation_id, request).await?;
    Ok(Json(SendMessageResponse { message }))
}

async fn history(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<HistoryPage>, AppError> {
    messages::history(&state.pool, principal.user_id, conversation_id, query)
        .await
        .map(Json)
}

async fn get_message(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
) -> Result<Json<MessageResponse>, AppError> {
    let message = messages::get_message(&state.pool, principal.user_id, message_id).await?;
    Ok(Json(MessageResponse { message }))
}

async fn edit_message(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiJson(request): ApiJson<EditMessageRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let message =
        messages::edit_message(&state.pool, principal.user_id, message_id, request).await?;
    Ok(Json(MessageResponse { message }))
}

async fn create_group(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<CreateGroupRequest>,
) -> Result<Json<GroupResponse>, AppError> {
    let group = groups::create_group(&state.pool, principal.user_id, workspace_id, request).await?;
    Ok(Json(GroupResponse { group }))
}

async fn list_groups(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<Json<GroupsResponse>, AppError> {
    groups::list_groups(&state.pool, principal.user_id, workspace_id)
        .await
        .map(Json)
}

async fn update_group(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(group_id): ApiPath<GroupId>,
    ApiJson(request): ApiJson<UpdateGroupRequest>,
) -> Result<Json<GroupResponse>, AppError> {
    let group = groups::update_group(&state.pool, principal.user_id, group_id, request).await?;
    Ok(Json(GroupResponse { group }))
}

async fn delete_group(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(group_id): ApiPath<GroupId>,
) -> Result<StatusCode, AppError> {
    groups::delete_group(&state.pool, principal.user_id, group_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn conversation_members(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<Json<ConversationMembersResponse>, AppError> {
    conversations::conversation_members(&state.pool, principal.user_id, conversation_id)
        .await
        .map(Json)
}

async fn create_webhook(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<CreateWebhookRequest>,
) -> Result<Json<CreatedWebhook>, AppError> {
    webhooks::create(&state, principal.user_id, conversation_id, request)
        .await
        .map(Json)
}

async fn list_webhooks(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<Json<WebhooksResponse>, AppError> {
    let webhooks = webhooks::list(&state.pool, principal.user_id, conversation_id).await?;
    Ok(Json(WebhooksResponse { webhooks }))
}

async fn delete_webhook(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(webhook_id): ApiPath<WebhookId>,
) -> Result<StatusCode, AppError> {
    webhooks::delete(&state.pool, principal.user_id, webhook_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_workspace_webhooks(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<Json<WorkspaceWebhooksResponse>, AppError> {
    let webhooks = webhooks::list_workspace(&state.pool, principal.user_id, workspace_id).await?;
    Ok(Json(WorkspaceWebhooksResponse { webhooks }))
}

async fn webhook_deliveries(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(webhook_id): ApiPath<WebhookId>,
) -> Result<Json<WebhookDeliveriesResponse>, AppError> {
    let deliveries = webhooks::deliveries(&state.pool, principal.user_id, webhook_id).await?;
    Ok(Json(WebhookDeliveriesResponse { deliveries }))
}

async fn new_webhook_address(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(webhook_id): ApiPath<WebhookId>,
) -> Result<Json<CreatedWebhook>, AppError> {
    webhooks::new_address(&state, principal.user_id, webhook_id)
        .await
        .map(Json)
}

// ----- GitHub (D32) -----

async fn github_status(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<Json<lazy_protocol::GithubStatusResponse>, AppError> {
    crate::github::status(&state, principal.user_id, workspace_id)
        .await
        .map(Json)
}

async fn github_disconnect(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<StatusCode, AppError> {
    crate::github::disconnect(&state, principal.user_id, workspace_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn github_setup(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<lazy_protocol::StartGithubSetupRequest>,
) -> Result<Json<lazy_protocol::GithubSetupResponse>, AppError> {
    let url = crate::github::start_setup(
        &state,
        principal.user_id,
        workspace_id,
        &request.organization,
    )
    .await?;
    Ok(Json(lazy_protocol::GithubSetupResponse { url }))
}

async fn github_subscribe(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<lazy_protocol::CreateGithubSubscriptionRequest>,
) -> Result<Json<lazy_protocol::GithubSubscriptionResponse>, AppError> {
    let subscription =
        crate::github::subscribe(&state, principal.user_id, conversation_id, request).await?;
    Ok(Json(lazy_protocol::GithubSubscriptionResponse {
        subscription,
    }))
}

async fn github_unsubscribe(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(subscription_id): ApiPath<lazy_domain::GithubSubscriptionId>,
) -> Result<StatusCode, AppError> {
    crate::github::unsubscribe(&state, principal.user_id, subscription_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn github_retry(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(delivery_id): ApiPath<i64>,
) -> Result<StatusCode, AppError> {
    crate::github::retry_delivery(&state, principal.user_id, delivery_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The admin's browser, from the address `github_setup` answered with. Public: the state in the
/// path is the credential.
async fn github_setup_page(
    State(state): State<AppState>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let token = path.map(|Path(token)| token).unwrap_or_default();
    crate::github::setup_page(&state, &token).await
}

#[derive(serde::Deserialize)]
struct GithubCallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

/// GitHub sends the admin's browser here after creating the app.
async fn github_callback(
    State(state): State<AppState>,
    query: Result<Query<GithubCallbackQuery>, QueryRejection>,
) -> Response {
    let query = query.map(|Query(q)| q).unwrap_or(GithubCallbackQuery {
        code: None,
        state: None,
    });
    crate::github::callback(&state, query.code.as_deref(), query.state.as_deref()).await
}

async fn github_installed() -> Response {
    crate::github::installed_page()
}

/// GitHub's webhook deliveries. Public: each is checked against the app's signing secret.
async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Response {
    match axum::body::to_bytes(body, crate::github::MAX_BODY_BYTES).await {
        Ok(bytes) => crate::github::receive(&state, &headers, &bytes).await,
        Err(_) => {
            AppError::new(ErrorCode::PayloadTooLarge, "the delivery is too large").into_response()
        }
    }
}

/// Why a delivery was not made: a refusal like any other, or the webhook is posting too fast.
enum HookFailure {
    Refused(AppError),
    RateLimited(Duration),
}

impl From<AppError> for HookFailure {
    fn from(error: AppError) -> Self {
        Self::Refused(error)
    }
}

impl IntoResponse for HookFailure {
    fn into_response(self) -> Response {
        match self {
            Self::Refused(error) => error.into_response(),
            Self::RateLimited(wait) => {
                let mut response = AppError::new(
                    ErrorCode::RateLimited,
                    "this webhook is posting too fast; retry after the time in Retry-After",
                )
                .into_response();
                // Whole seconds, rounded up, never zero.
                let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
                if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
                    response.headers_mut().insert(RETRY_AFTER, value);
                }
                response
            }
        }
    }
}

/// `POST /hooks/{id}/{secret}`: public, no `Authorization`; the address is the credential. The
/// address is checked first (a wrong one is `not_found` whatever else is wrong with the request),
/// then the webhook's rate, then the body, which is read only once it is known to be wanted.
///
/// Every request whose address was right goes into the webhook's delivery log
/// ([`webhooks::record`]) with what became of it, except that a run of rate-limited requests is
/// one entry and a webhook revoked during the request has nothing to record under.
async fn incoming_webhook(
    State(state): State<AppState>,
    path: Result<Path<(String, String)>, PathRejection>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<IncomingWebhookResponse>, HookFailure> {
    // A path that cannot be read is as unknown as any other; its text is never echoed.
    let (id, secret) = match path {
        Ok(Path(parts)) => parts,
        Err(_) => (String::new(), String::new()),
    };
    let webhook = webhooks::authenticate(&state.pool, &id, &secret).await?;
    let has_key = headers.contains_key("idempotency-key");
    if let Err(refused) = state.integrations.limiter.take(webhook.id) {
        if refused.first {
            let entry = webhooks::DeliveryRecord {
                outcome: WebhookDeliveryOutcome::RateLimited,
                message_id: None,
                body_bytes: 0,
                idempotency_key: has_key,
            };
            webhooks::record(&state.pool, webhook.id, entry).await;
        }
        return Err(HookFailure::RateLimited(refused.wait));
    }

    let mut body_bytes = 0;
    let result = receive(&state, webhook, &headers, body, &mut body_bytes).await;
    let (outcome, message_id) = match &result {
        Ok(delivered) if delivered.duplicate => (
            Some(WebhookDeliveryOutcome::Duplicate),
            Some(delivered.message_id),
        ),
        Ok(delivered) => (
            Some(WebhookDeliveryOutcome::Posted),
            Some(delivered.message_id),
        ),
        Err(undelivered) => (undelivered.outcome, None),
    };
    if let Some(outcome) = outcome {
        let entry = webhooks::DeliveryRecord {
            outcome,
            message_id,
            body_bytes,
            idempotency_key: has_key,
        };
        webhooks::record(&state.pool, webhook.id, entry).await;
    }
    match result {
        Ok(delivered) => Ok(Json(IncomingWebhookResponse {
            message_id: delivered.message_id,
        })),
        Err(undelivered) => Err(HookFailure::Refused(undelivered.error)),
    }
}

/// The part of a delivery after the address and the rate: the key, the body, and the post.
/// `body_bytes` is set to the size of the body once it is known.
async fn receive(
    state: &AppState,
    webhook: webhooks::ActiveWebhook,
    headers: &HeaderMap,
    body: axum::body::Body,
    body_bytes: &mut usize,
) -> Result<webhooks::Delivered, webhooks::Undelivered> {
    let key = match headers.get("idempotency-key") {
        Some(value) => Some(webhooks::check_idempotency_key(value.as_bytes())?),
        None => None,
    };
    let too_large = || {
        AppError::new(
            ErrorCode::PayloadTooLarge,
            format!("the body can be at most {MAX_WEBHOOK_BODY_BYTES} bytes"),
        )
    };
    let declared = headers
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if let Some(length) = declared.filter(|length| *length > MAX_WEBHOOK_BODY_BYTES) {
        *body_bytes = length;
        return Err(too_large().into());
    }
    let bytes = axum::body::to_bytes(body, MAX_WEBHOOK_BODY_BYTES)
        .await
        .map_err(|_| {
            *body_bytes = MAX_WEBHOOK_BODY_BYTES + 1;
            too_large()
        })?;
    *body_bytes = bytes.len();
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    // Lazy's own `{"text": ...}` and Slack's webhook body (D33) are read the same way.
    let body = crate::slack_compat::parse_body(content_type, &bytes).ok_or_else(|| {
        AppError::invalid("the body must be JSON like {\"text\": \"...\"}, or Slack's webhook body")
    })?;
    let text = crate::slack_compat::to_markdown(&body).ok_or_else(|| {
        AppError::invalid("there is nothing to post: send text, blocks, or attachments")
    })?;
    webhooks::deliver(&state.pool, webhook, key, &text).await
}

async fn upload_attachment(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((conversation_id, attachment_id)): ApiPath<(ConversationId, AttachmentId)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<AttachmentResponse>, AppError> {
    let attachment = attachments::upload(
        &state,
        principal.user_id,
        conversation_id,
        attachment_id,
        &headers,
        body,
    )
    .await?;
    Ok(Json(AttachmentResponse { attachment }))
}

async fn upload_emoji(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((workspace_id, emoji_id)): ApiPath<(WorkspaceId, EmojiId)>,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<CustomEmojiResponse>, AppError> {
    let emoji = emoji::upload(
        &state,
        principal.user_id,
        workspace_id,
        emoji_id,
        &headers,
        body,
    )
    .await?;
    Ok(Json(CustomEmojiResponse { emoji }))
}

async fn put_notification_settings(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(settings): ApiJson<lazy_domain::notification::NotificationSettings>,
) -> Result<Json<NotificationSettingsResponse>, AppError> {
    let settings =
        notifications::set_settings(&state.pool, principal.user_id, workspace_id, settings).await?;
    Ok(Json(NotificationSettingsResponse { settings }))
}

async fn put_conversation_notification_prefs(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<SetConversationNotificationPrefsRequest>,
) -> Result<Json<ConversationNotificationPrefsResponse>, AppError> {
    let prefs = notifications::set_prefs(
        &state.pool,
        principal.user_id,
        conversation_id,
        request.level,
        request.muted,
    )
    .await?;
    Ok(Json(ConversationNotificationPrefsResponse { prefs }))
}

async fn put_status(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<SetStatusRequest>,
) -> Result<Json<StatusResponse>, AppError> {
    let user =
        notifications::set_status(&state.pool, principal.user_id, workspace_id, request.status)
            .await?;
    Ok(Json(StatusResponse { user }))
}

async fn get_inbox(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiQuery(query): ApiQuery<InboxQuery>,
) -> Result<Json<InboxPage>, AppError> {
    Ok(Json(
        notifications::inbox(&state.pool, principal.user_id, workspace_id, query).await?,
    ))
}

async fn post_inbox_read(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<Json<InboxReadResponse>, AppError> {
    Ok(Json(
        notifications::mark_inbox_read(&state.pool, principal.user_id, workspace_id).await?,
    ))
}

async fn update_workspace(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<UpdateWorkspaceRequest>,
) -> Result<Json<WorkspaceResponse>, AppError> {
    let workspace =
        admin::update_workspace(&state.pool, principal.user_id, workspace_id, request).await?;
    Ok(Json(WorkspaceResponse { workspace }))
}

async fn create_workspace(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<CreateWorkspaceRequest>,
) -> Result<Json<JoinedWorkspace>, AppError> {
    workspaces::create(&state, principal.user_id, workspace_id, request)
        .await
        .map(Json)
}

async fn accept_invitation(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiJson(request): ApiJson<AcceptInvitationRequest>,
) -> Result<Json<JoinedWorkspace>, AppError> {
    workspaces::accept_invitation(&state, principal.user_id, request)
        .await
        .map(Json)
}

async fn update_profile(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiJson(request): ApiJson<UpdateProfileRequest>,
) -> Result<Json<ProfileResponse>, AppError> {
    let user = profile::update_profile(&state.pool, principal.user_id, request).await?;
    Ok(Json(ProfileResponse { user }))
}

async fn upload_avatar(
    State(state): State<AppState>,
    Auth(principal): Auth,
    headers: HeaderMap,
    body: axum::body::Body,
) -> Result<Json<ProfileResponse>, AppError> {
    let user = profile::set_avatar(&state, principal.user_id, &headers, body).await?;
    Ok(Json(ProfileResponse { user }))
}

async fn delete_avatar(
    State(state): State<AppState>,
    Auth(principal): Auth,
) -> Result<Json<ProfileResponse>, AppError> {
    let user = profile::remove_avatar(&state.pool, principal.user_id).await?;
    Ok(Json(ProfileResponse { user }))
}

/// `?v=` on an avatar address: the version the client wants, which decides how long the answer may
/// be cached (it is always the person's current picture).
#[derive(serde::Deserialize)]
struct AvatarQuery {
    v: Option<u32>,
}

async fn download_avatar(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(user_id): ApiPath<UserId>,
    ApiQuery(query): ApiQuery<AvatarQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    profile::download_avatar(&state.pool, principal.user_id, user_id, query.v, &headers).await
}

async fn put_person_role(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((workspace_id, user_id)): ApiPath<(WorkspaceId, UserId)>,
    ApiJson(request): ApiJson<SetRoleRequest>,
) -> Result<Json<PersonResponse>, AppError> {
    let person = admin::set_role(
        &state.pool,
        principal.user_id,
        workspace_id,
        user_id,
        request.role,
    )
    .await?;
    Ok(Json(PersonResponse { person }))
}

async fn put_person_deactivated(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((workspace_id, user_id)): ApiPath<(WorkspaceId, UserId)>,
    ApiJson(request): ApiJson<SetDeactivatedRequest>,
) -> Result<Json<PersonResponse>, AppError> {
    let person = admin::set_deactivated(
        &state.pool,
        principal.user_id,
        workspace_id,
        user_id,
        request.deactivated,
    )
    .await?;
    Ok(Json(PersonResponse { person }))
}

async fn list_invitations(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
) -> Result<Json<InvitationList>, AppError> {
    admin::list_invitations(&state.pool, principal.user_id, workspace_id)
        .await
        .map(Json)
}

async fn create_invitation(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<CreateInvitationRequest>,
) -> Result<Json<CreatedInvitation>, AppError> {
    admin::create_invitation(&state.pool, principal.user_id, workspace_id, request)
        .await
        .map(Json)
}

async fn revoke_invitation(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(invitation_id): ApiPath<InvitationId>,
) -> Result<Json<InvitationResponse>, AppError> {
    let invitation =
        admin::revoke_invitation(&state.pool, principal.user_id, invitation_id).await?;
    Ok(Json(InvitationResponse { invitation }))
}

async fn search_workspace(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiQuery(params): ApiQuery<SearchParams>,
) -> Result<Json<SearchResponse>, AppError> {
    let found = search::search(
        &state.pool,
        &state.search,
        principal.user_id,
        workspace_id,
        params,
    )
    .await?;
    Ok(Json(found))
}

async fn download_emoji(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(emoji_id): ApiPath<EmojiId>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    emoji::download(&state, principal.user_id, emoji_id, &headers).await
}

async fn rename_emoji(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(emoji_id): ApiPath<EmojiId>,
    ApiJson(request): ApiJson<RenameEmojiRequest>,
) -> Result<Json<CustomEmojiResponse>, AppError> {
    let emoji = emoji::rename(&state.pool, principal.user_id, emoji_id, request.name).await?;
    Ok(Json(CustomEmojiResponse { emoji }))
}

async fn delete_emoji(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(emoji_id): ApiPath<EmojiId>,
) -> Result<StatusCode, AppError> {
    emoji::delete(&state.pool, principal.user_id, emoji_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn download_attachment(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(attachment_id): ApiPath<AttachmentId>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    attachments::download(&state, principal.user_id, attachment_id, false, &headers).await
}

async fn download_attachment_thumbnail(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(attachment_id): ApiPath<AttachmentId>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    attachments::download(&state, principal.user_id, attachment_id, true, &headers).await
}

async fn replies(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiQuery(query): ApiQuery<HistoryQuery>,
) -> Result<Json<HistoryPage>, AppError> {
    messages::replies(&state.pool, principal.user_id, message_id, query)
        .await
        .map(Json)
}

async fn set_thread_follow(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiJson(request): ApiJson<SetThreadFollowRequest>,
) -> Result<Json<ThreadStateResponse>, AppError> {
    let state = threads::set_follow(
        &state.pool,
        principal.user_id,
        message_id,
        request.following,
    )
    .await?;
    Ok(Json(ThreadStateResponse { state }))
}

async fn mark_thread_read(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiJson(request): ApiJson<MarkThreadReadRequest>,
) -> Result<Json<ThreadStateResponse>, AppError> {
    let state =
        threads::mark_read(&state.pool, principal.user_id, message_id, request.up_to).await?;
    Ok(Json(ThreadStateResponse { state }))
}

async fn set_reaction(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiJson(request): ApiJson<SetReactionRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    let message =
        messages::set_reaction(&state.pool, principal.user_id, message_id, request).await?;
    Ok(Json(MessageResponse { message }))
}

async fn delete_message(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
) -> Result<Json<MessageResponse>, AppError> {
    let message = messages::delete_message(&state.pool, principal.user_id, message_id).await?;
    Ok(Json(MessageResponse { message }))
}

async fn operation_status(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(operation_id): ApiPath<OperationId>,
) -> Result<Json<OperationStatus>, AppError> {
    messages::operation_status(&state.pool, principal.user_id, operation_id)
        .await
        .map(Json)
}

async fn create_channel(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<CreateChannelRequest>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation =
        conversations::create_channel(&state.pool, principal.user_id, workspace_id, request)
            .await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn browse_channels(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiQuery(query): ApiQuery<BrowseChannelsQuery>,
) -> Result<Json<BrowseChannelsResponse>, AppError> {
    conversations::browse_channels(&state.pool, principal.user_id, workspace_id, query)
        .await
        .map(Json)
}

async fn list_people(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiQuery(query): ApiQuery<PeopleQuery>,
) -> Result<Json<PeoplePage>, AppError> {
    discovery::people(&state.pool, principal.user_id, workspace_id, query)
        .await
        .map(Json)
}

async fn get_person(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((workspace_id, user_id)): ApiPath<(WorkspaceId, UserId)>,
) -> Result<Json<PersonResponse>, AppError> {
    let person = discovery::person(&state.pool, principal.user_id, workspace_id, user_id).await?;
    Ok(Json(PersonResponse { person }))
}

async fn list_saved(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiQuery(query): ApiQuery<SavedQuery>,
) -> Result<Json<SavedPage>, AppError> {
    discovery::saved(&state.pool, principal.user_id, workspace_id, query)
        .await
        .map(Json)
}

async fn put_message_saved(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(message_id): ApiPath<MessageId>,
    ApiJson(request): ApiJson<SetSavedRequest>,
) -> Result<Json<SavedResponse>, AppError> {
    discovery::set_saved(&state.pool, principal.user_id, message_id, request.saved)
        .await
        .map(Json)
}

async fn open_conversation(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(workspace_id): ApiPath<WorkspaceId>,
    ApiJson(request): ApiJson<OpenConversationRequest>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation =
        conversations::open_conversation(&state.pool, principal.user_id, workspace_id, request)
            .await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn update_conversation(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<UpdateConversationRequest>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation = conversations::update_conversation(
        &state.pool,
        principal.user_id,
        conversation_id,
        request,
    )
    .await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn join_channel(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation = conversations::join(&state.pool, principal.user_id, conversation_id).await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn leave_channel(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<StatusCode, AppError> {
    conversations::leave(&state.pool, principal.user_id, conversation_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn archive_channel(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation =
        conversations::archive(&state.pool, principal.user_id, conversation_id).await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn unarchive_channel(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation =
        conversations::unarchive(&state.pool, principal.user_id, conversation_id).await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn add_members(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<AddMembersRequest>,
) -> Result<Json<ConversationResponse>, AppError> {
    let conversation = conversations::add_members_command(
        &state.pool,
        principal.user_id,
        conversation_id,
        request,
    )
    .await?;
    Ok(Json(ConversationResponse { conversation }))
}

async fn remove_member(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath((conversation_id, user_id)): ApiPath<(ConversationId, UserId)>,
) -> Result<StatusCode, AppError> {
    conversations::remove_member_command(&state.pool, principal.user_id, conversation_id, user_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn set_favorite(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<SetFavoriteRequest>,
) -> Result<Json<UserStateResponse>, AppError> {
    let state = conversations::set_favorite(
        &state.pool,
        principal.user_id,
        conversation_id,
        request.favorite,
    )
    .await?;
    Ok(Json(UserStateResponse { state }))
}

async fn mark_read(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(conversation_id): ApiPath<ConversationId>,
    ApiJson(request): ApiJson<MarkReadRequest>,
) -> Result<Json<UserStateResponse>, AppError> {
    let state = conversations::mark_read(
        &state.pool,
        principal.user_id,
        conversation_id,
        request.up_to,
    )
    .await?;
    Ok(Json(UserStateResponse { state }))
}

async fn realtime_upgrade(
    State(state): State<AppState>,
    Auth(principal): Auth,
    upgrade: WebSocketUpgrade,
) -> Response {
    upgrade
        .max_message_size(MAX_SOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let connection = realtime::serve_connection(
                state.pool.clone(),
                state.dispatcher.clone(),
                principal,
                socket,
                state.shutdown.child_token(),
            );
            // Tracked, so shutdown waits for the connection to say goodbye; supervised, so a panic is
            // reported instead of vanishing with the task.
            state.tasks.spawn(async move {
                if std::panic::AssertUnwindSafe(connection)
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    tracing::error!("realtime connection task panicked");
                }
            });
        })
}

/// The authenticated caller, from `Authorization: Bearer <token>`.
pub struct Auth(pub Principal);

impl FromRequestParts<AppState> for Auth {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(AppError::unauthenticated)?;
        let (scheme, token) = header
            .split_once(' ')
            .ok_or_else(AppError::unauthenticated)?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err(AppError::unauthenticated());
        }
        let principal = accounts::authenticate(&state.pool, token.trim()).await?;
        // For the request line: who made the request (their id, nothing else).
        if let Some(info) = parts.extensions.get::<logging::RequestInfo>() {
            info.note_user(principal.user_id);
        }
        Ok(Auth(principal))
    }
}

/// `Json` that reports failures as an [`ApiError`](lazy_protocol::ApiError) body.
pub struct ApiJson<T>(pub T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request(request: Request, state: &S) -> Result<Self, AppError> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(rejection) => Err(json_rejection(&rejection)),
        }
    }
}

fn json_rejection(rejection: &JsonRejection) -> AppError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::new(ErrorCode::PayloadTooLarge, "request body is too large")
    } else {
        AppError::invalid(rejection.body_text())
    }
}

/// `Query` that reports failures as an [`ApiError`](lazy_protocol::ApiError) body.
pub struct ApiQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        Query::<T>::from_request_parts(parts, state)
            .await
            .map(|Query(value)| Self(value))
            .map_err(|rejection: QueryRejection| AppError::invalid(rejection.body_text()))
    }
}

/// `Path` that reports failures as an [`ApiError`](lazy_protocol::ApiError) body.
pub struct ApiPath<T>(pub T);

impl<S, T> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, AppError> {
        Path::<T>::from_request_parts(parts, state)
            .await
            .map(|Path(value)| Self(value))
            .map_err(|rejection: PathRejection| AppError::invalid(rejection.body_text()))
    }
}
