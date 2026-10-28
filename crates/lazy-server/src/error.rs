//! Error type shared by services and HTTP handlers.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lazy_protocol::{ApiError, ErrorCode};

/// A failure while serving a request.
///
/// `Api` errors are deliberate, caller-visible outcomes. `Database` and `Internal` errors are
/// logged with their cause and reported to the caller without detail.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Api(#[from] ApiError),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(String),
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Api(ApiError::new(code, message))
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    pub fn unauthenticated() -> Self {
        Self::new(ErrorCode::Unauthenticated, "sign in required")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Conflict, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    /// The wire representation. Internal causes are replaced by a generic message.
    pub fn to_api_error(&self) -> ApiError {
        match self {
            Self::Api(error) => error.clone(),
            Self::Database(
                sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_),
            ) => ApiError::new(
                ErrorCode::Unavailable,
                "the server is temporarily unavailable",
            ),
            Self::Database(_) | Self::Internal(_) => {
                ApiError::new(ErrorCode::Internal, "internal server error")
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if !matches!(self, Self::Api(_)) {
            tracing::error!(error = %self, "request failed");
        }
        let body = self.to_api_error();
        let status = StatusCode::from_u16(body.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(body)).into_response()
    }
}
