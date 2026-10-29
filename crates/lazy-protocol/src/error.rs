use serde::{Deserialize, Serialize};

/// Machine-readable failure category, carried in [`ApiError`] and in realtime error frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request was malformed or failed validation. Retrying unchanged will fail again.
    InvalidRequest,
    /// No valid session. The client should sign in again.
    Unauthenticated,
    /// Username or password did not match.
    InvalidCredentials,
    /// The caller may not do this.
    Forbidden,
    /// The target does not exist, or the caller may not know it exists.
    NotFound,
    /// The request conflicts with existing state, such as an operation id reused for a different
    /// message.
    Conflict,
    /// An edit was based on a revision that is no longer current: someone, perhaps the same
    /// person on another device, changed the message first. Fetch the message and reconcile.
    StaleRevision,
    /// Registration failed because the username is taken.
    UsernameTaken,
    /// A channel with that name already exists in the workspace.
    NameTaken,
    /// The invitation code is wrong, expired, revoked, or used up. The server does not say which,
    /// so codes cannot be probed.
    InvitationInvalid,
    PayloadTooLarge,
    /// The content is not something the server accepts: an image it cannot decode, or one far
    /// larger than it will decode. Retrying unchanged will fail again.
    UnsupportedMedia,
    /// A limit on how much of something there may be was reached, such as the number of
    /// different emoji on one message. Retrying unchanged will fail again.
    LimitReached,
    RateLimited,
    /// The server failed. The outcome of a mutation may be unknown.
    Internal,
    /// The server is temporarily unable to serve. The outcome of a mutation may be unknown.
    Unavailable,
}

impl ErrorCode {
    /// The HTTP status used for this code.
    pub const fn http_status(self) -> u16 {
        match self {
            Self::InvalidRequest | Self::InvitationInvalid => 400,
            Self::Unauthenticated | Self::InvalidCredentials => 401,
            Self::Forbidden => 403,
            Self::NotFound => 404,
            Self::Conflict | Self::StaleRevision | Self::UsernameTaken | Self::NameTaken => 409,
            Self::PayloadTooLarge => 413,
            Self::UnsupportedMedia => 415,
            Self::LimitReached => 422,
            Self::RateLimited => 429,
            Self::Internal => 500,
            Self::Unavailable => 503,
        }
    }

    /// Whether the server definitively rejected the request without committing it. For
    /// `Internal` and `Unavailable` a mutation's outcome is unknown and must be reconciled by
    /// querying the operation's status before anything else is attempted.
    pub const fn is_definitive_rejection(self) -> bool {
        !matches!(self, Self::Internal | Self::Unavailable)
    }
}

/// Body of every non-success HTTP response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    pub code: ErrorCode,
    /// A human-readable explanation. Never contains credentials or other users' data.
    pub message: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_is_snake_case() {
        let error = ApiError::new(ErrorCode::UsernameTaken, "taken");
        assert_eq!(
            serde_json::to_string(&error).unwrap(),
            r#"{"code":"username_taken","message":"taken"}"#
        );
    }

    #[test]
    fn a_stale_edit_is_a_definitive_conflict_with_its_own_code() {
        assert_eq!(ErrorCode::StaleRevision.http_status(), 409);
        assert!(ErrorCode::StaleRevision.is_definitive_rejection());
        assert_eq!(
            serde_json::to_string(&ErrorCode::StaleRevision).unwrap(),
            r#""stale_revision""#
        );
    }

    #[test]
    fn reaching_a_limit_is_a_definitive_unprocessable_request() {
        assert_eq!(ErrorCode::LimitReached.http_status(), 422);
        assert!(ErrorCode::LimitReached.is_definitive_rejection());
        assert_eq!(
            serde_json::to_string(&ErrorCode::LimitReached).unwrap(),
            r#""limit_reached""#
        );
    }

    #[test]
    fn only_server_side_failures_leave_the_outcome_unknown() {
        assert!(ErrorCode::Forbidden.is_definitive_rejection());
        assert!(!ErrorCode::Internal.is_definitive_rejection());
        assert!(!ErrorCode::Unavailable.is_definitive_rejection());
    }

    #[test]
    fn a_bad_invitation_is_a_definitive_bad_request_with_its_own_code() {
        let code = ErrorCode::InvitationInvalid;
        assert_eq!(code.http_status(), 400);
        assert!(code.is_definitive_rejection());
        assert_eq!(
            serde_json::to_string(&code).unwrap(),
            r#""invitation_invalid""#
        );
    }
}
