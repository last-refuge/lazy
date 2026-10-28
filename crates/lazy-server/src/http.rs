//! The HTTP API: routes, request extractors, and handlers. Handlers only translate between HTTP
//! and the service modules; the rules live in those modules.

use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::rejection::{JsonRejection, PathRejection, QueryRejection};
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Path, Query, Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use axum::response::Response;
use axum::routing::{get, post};
use futures_util::FutureExt;
use lazy_domain::{ConversationId, OperationId};
use lazy_protocol::api::paths;
use lazy_protocol::{
    BootstrapResponse, ErrorCode, HealthResponse, HistoryPage, HistoryQuery, LoginRequest,
    OperationStatus, PROTOCOL_VERSION, RegisterRequest, SendMessageRequest, SendMessageResponse,
    SessionResponse,
};
use serde::de::DeserializeOwned;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::accounts::{self, Principal};
use crate::app::AppState;
use crate::error::AppError;
use crate::{bootstrap, messages, realtime};

/// Largest accepted request body.
const MAX_BODY_BYTES: usize = 256 * 1024;
/// Longest a request may take before the server gives up on it.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest accepted WebSocket message from a client.
const MAX_SOCKET_MESSAGE_BYTES: usize = 64 * 1024;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route(paths::HEALTH, get(health))
        .route(paths::REGISTER, post(register))
        .route(paths::LOGIN, post(login))
        .route(paths::LOGOUT, post(logout))
        .route(paths::BOOTSTRAP, get(bootstrap_snapshot))
        .route(paths::REALTIME, get(realtime_upgrade))
        .route(paths::MESSAGES_ROUTE, get(history).post(send_message))
        .route(paths::OPERATION_ROUTE, get(operation_status))
        .fallback(unknown_route)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(TraceLayer::new_for_http())
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

async fn operation_status(
    State(state): State<AppState>,
    Auth(principal): Auth,
    ApiPath(operation_id): ApiPath<OperationId>,
) -> Result<Json<OperationStatus>, AppError> {
    messages::operation_status(&state.pool, principal.user_id, operation_id)
        .await
        .map(Json)
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
        accounts::authenticate(&state.pool, token.trim())
            .await
            .map(Auth)
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
