//! Request and response bodies for the HTTP API, and the route table.

use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, DisplayName, EventSeq, MessageDocument,
    MessageId, OperationId, Timestamp, UserId, Username, WorkspaceId, WorkspaceRole,
};
use serde::{Deserialize, Serialize};

use crate::{Password, SessionToken};

/// Route paths. `*_ROUTE` constants are Axum route patterns; the functions build concrete paths.
pub mod paths {
    use lazy_domain::{ConversationId, OperationId};

    pub const HEALTH: &str = "/healthz";
    pub const REGISTER: &str = "/v1/auth/register";
    pub const LOGIN: &str = "/v1/auth/login";
    pub const LOGOUT: &str = "/v1/auth/logout";
    pub const BOOTSTRAP: &str = "/v1/bootstrap";
    /// WebSocket upgrade. Authenticate with the `Authorization: Bearer` header.
    pub const REALTIME: &str = "/v1/realtime";
    pub const MESSAGES_ROUTE: &str = "/v1/conversations/{conversation_id}/messages";
    pub const OPERATION_ROUTE: &str = "/v1/operations/{operation_id}";

    /// `GET` pages history, `POST` sends a message.
    pub fn messages(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/messages")
    }

    pub fn operation(operation_id: OperationId) -> String {
        format!("/v1/operations/{operation_id}")
    }
}

/// Response of `GET /healthz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub protocol_version: u32,
}

/// A person, as other people see them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: Username,
    pub display_name: DisplayName,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
}

/// A channel. Direct and group conversations will share this type with a different kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub name: ChannelName,
    pub topic: Option<String>,
    pub visibility: ChannelVisibility,
    pub archived: bool,
}

/// A committed message revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub author_id: UserId,
    pub document: MessageDocument,
    pub created_at: Timestamp,
    /// Starts at 1 and increases with each edit.
    pub revision: u32,
    /// Event-log position at which the message was created. Messages in a conversation are
    /// ordered by this value, never by timestamp.
    pub seq: EventSeq,
}

/// `POST /v1/auth/register`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: Username,
    pub display_name: DisplayName,
    pub password: Password,
}

/// `POST /v1/auth/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: Username,
    pub password: Password,
}

/// Response of register and login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResponse {
    pub token: SessionToken,
    pub user: User,
}

/// One workspace's consistent snapshot. `head` is the event-log position the snapshot reflects;
/// subscribe with `after = head` to receive everything newer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBootstrap {
    pub workspace: Workspace,
    pub role: WorkspaceRole,
    /// Conversations the caller may read.
    pub conversations: Vec<Conversation>,
    pub members: Vec<User>,
    pub head: EventSeq,
}

/// Response of `GET /v1/bootstrap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapResponse {
    pub protocol_version: u32,
    pub user: User,
    pub workspaces: Vec<WorkspaceBootstrap>,
}

/// `POST /v1/conversations/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub operation_id: OperationId,
    pub document: MessageDocument,
}

/// Acknowledgment of a committed send. Returned only after the transaction commits, and
/// identifies exactly what was committed. Repeating the same request returns the same response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub message: Message,
}

/// Query string of `GET /v1/conversations/{id}/messages`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    /// Return messages strictly older than this position; omit for the newest page.
    pub before: Option<EventSeq>,
    /// Page size; the server applies a default and a maximum.
    pub limit: Option<u32>,
}

/// One page of history, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    /// Whether older messages exist beyond this page.
    pub has_more: bool,
}

/// Response of `GET /v1/operations/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OperationStatus {
    /// The operation committed; this is its result.
    Committed { message: Message },
    /// The server has no record of this operation for the caller. Resend the same request with
    /// the same id; never generate a new id for the same user action.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_status_shape() {
        assert_eq!(
            serde_json::to_string(&OperationStatus::NotFound).unwrap(),
            r#"{"status":"not_found"}"#
        );
    }

    #[test]
    fn requests_with_invalid_names_fail_to_parse() {
        let json = r#"{"username":"x","display_name":"X","password":"pw"}"#;
        assert!(serde_json::from_str::<RegisterRequest>(json).is_err());
    }

    #[test]
    fn route_builders_match_patterns() {
        let id = ConversationId::generate();
        assert_eq!(
            paths::messages(id),
            paths::MESSAGES_ROUTE.replace("{conversation_id}", &id.to_string())
        );
    }
}
