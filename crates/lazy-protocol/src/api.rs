//! Request and response bodies for the HTTP API, and the route table.

use lazy_domain::{
    ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName, EventSeq,
    MessageDocument, MessageId, OperationId, Timestamp, UserId, Username, WorkspaceId,
    WorkspaceRole,
};
use serde::{Deserialize, Serialize};

use crate::{Password, SessionToken};

/// Route paths. `*_ROUTE` constants are Axum route patterns; the functions build concrete paths.
pub mod paths {
    use lazy_domain::{ConversationId, OperationId, UserId, WorkspaceId};

    pub const HEALTH: &str = "/healthz";
    pub const REGISTER: &str = "/v1/auth/register";
    pub const LOGIN: &str = "/v1/auth/login";
    pub const LOGOUT: &str = "/v1/auth/logout";
    pub const BOOTSTRAP: &str = "/v1/bootstrap";
    /// WebSocket upgrade. Authenticate with the `Authorization: Bearer` header.
    pub const REALTIME: &str = "/v1/realtime";
    pub const MESSAGES_ROUTE: &str = "/v1/conversations/{conversation_id}/messages";
    pub const OPERATION_ROUTE: &str = "/v1/operations/{operation_id}";

    /// `POST` creates a channel.
    pub const CHANNELS_ROUTE: &str = "/v1/workspaces/{workspace_id}/channels";
    /// `GET` lists the workspace's public channels, joined or not.
    pub const BROWSE_CHANNELS_ROUTE: &str = "/v1/workspaces/{workspace_id}/channels/browse";
    /// `POST` opens (finds or creates) a direct or group conversation.
    pub const OPEN_CONVERSATION_ROUTE: &str = "/v1/workspaces/{workspace_id}/conversations";
    /// `PATCH` changes a channel's name, topic, or description.
    pub const CONVERSATION_ROUTE: &str = "/v1/conversations/{conversation_id}";
    /// `POST` joins a public channel.
    pub const JOIN_ROUTE: &str = "/v1/conversations/{conversation_id}/join";
    /// `POST` leaves a channel.
    pub const LEAVE_ROUTE: &str = "/v1/conversations/{conversation_id}/leave";
    pub const ARCHIVE_ROUTE: &str = "/v1/conversations/{conversation_id}/archive";
    pub const UNARCHIVE_ROUTE: &str = "/v1/conversations/{conversation_id}/unarchive";
    /// `POST` adds people to a channel.
    pub const MEMBERS_ROUTE: &str = "/v1/conversations/{conversation_id}/members";
    /// `DELETE` removes someone from a channel.
    pub const MEMBER_ROUTE: &str = "/v1/conversations/{conversation_id}/members/{user_id}";
    /// `PUT` sets or clears the caller's favorite flag.
    pub const FAVORITE_ROUTE: &str = "/v1/conversations/{conversation_id}/favorite";
    /// `POST` moves the caller's read position forward.
    pub const READ_ROUTE: &str = "/v1/conversations/{conversation_id}/read";

    /// `GET` pages history, `POST` sends a message.
    pub fn messages(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/messages")
    }

    pub fn operation(operation_id: OperationId) -> String {
        format!("/v1/operations/{operation_id}")
    }

    pub fn channels(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/channels")
    }

    pub fn browse_channels(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/channels/browse")
    }

    pub fn open_conversation(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/conversations")
    }

    pub fn conversation(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}")
    }

    /// `action` is one of `join`, `leave`, `archive`, `unarchive`, `members`, `favorite`, `read`.
    pub fn conversation_action(conversation_id: ConversationId, action: &str) -> String {
        format!("/v1/conversations/{conversation_id}/{action}")
    }

    pub fn member(conversation_id: ConversationId, user_id: UserId) -> String {
        format!("/v1/conversations/{conversation_id}/members/{user_id}")
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

/// A channel, a direct conversation, or a group conversation.
///
/// The same value is sent to everyone who sees the conversation, so it holds nothing that depends
/// on who is looking. Whether the caller belongs to it is conveyed by where it appears: the
/// bootstrap lists only conversations the caller is a member of, and [`BrowsedChannel::joined`]
/// says so for public channels the caller may not have joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub kind: ConversationKind,
    /// Present for channels. Direct and group conversations have no name; show their
    /// participants instead.
    pub name: Option<ChannelName>,
    pub topic: Option<String>,
    pub description: Option<String>,
    pub visibility: ChannelVisibility,
    pub archived: bool,
    /// The workspace's default channel: everyone belongs, and it cannot be left or archived.
    pub is_default: bool,
    pub created_by: Option<UserId>,
    /// The people in a direct or group conversation, in a stable order. Empty for channels, whose
    /// membership can be large and is not listed here.
    pub participants: Vec<UserId>,
    pub member_count: u32,
}

/// One person's private state for one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationUserState {
    pub conversation_id: ConversationId,
    pub favorite: bool,
    /// Position of the newest message the person has read; `EventSeq::ZERO` means none.
    pub last_read: EventSeq,
    /// Messages from other people after `last_read`.
    pub unread_count: u32,
    /// Unread messages that mention the person. Always 0 until mentions exist.
    pub mention_count: u32,
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
    /// Conversations the caller is a member of: the sidebar. Public channels the caller has not
    /// joined are found through browsing instead.
    pub conversations: Vec<Conversation>,
    /// The caller's favorite flag and read position for each of `conversations`.
    pub user_state: Vec<ConversationUserState>,
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
    /// A send committed; this is its result.
    Committed { message: Message },
    /// A channel or conversation creation committed; this is its result.
    ConversationCommitted { conversation: Conversation },
    /// The server has no record of this operation for the caller. Resend the same request with
    /// the same id; never generate a new id for the same user action.
    NotFound,
}

/// `POST /v1/workspaces/{id}/channels`. The creator becomes a member and the channel's creator.
///
/// Repeating the same request with the same operation id returns the same channel; the same id
/// with different content is a `conflict`. A name already in use is `name_taken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub operation_id: OperationId,
    pub name: ChannelName,
    pub visibility: ChannelVisibility,
    pub topic: Option<String>,
    pub description: Option<String>,
}

/// `POST /v1/workspaces/{id}/conversations`: finds or creates the direct or group conversation
/// for exactly these people plus the caller. The same set always yields the same conversation.
/// Adding or removing a person is a different set, so it opens a new conversation with no
/// history, and the old one stays as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenConversationRequest {
    pub operation_id: OperationId,
    /// The other people. The caller is implied; naming themselves is ignored.
    pub participants: Vec<UserId>,
}

/// Response of the commands that return a conversation: create, open, join, update, archive,
/// unarchive, add members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationResponse {
    pub conversation: Conversation,
}

/// A public channel in the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowsedChannel {
    pub conversation: Conversation,
    /// Whether the caller is already a member.
    pub joined: bool,
}

/// Response of `GET /v1/workspaces/{id}/channels/browse`, ordered by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseChannelsResponse {
    pub channels: Vec<BrowsedChannel>,
}

/// `PATCH /v1/conversations/{id}`. Absent fields are unchanged; an empty string clears a topic or
/// description. Renaming needs the creator or an admin; the other fields need membership.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateConversationRequest {
    pub name: Option<ChannelName>,
    pub topic: Option<String>,
    pub description: Option<String>,
}

/// `POST /v1/conversations/{id}/members`. People already in the channel are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddMembersRequest {
    pub user_ids: Vec<UserId>,
}

/// `PUT /v1/conversations/{id}/favorite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetFavoriteRequest {
    pub favorite: bool,
}

/// `POST /v1/conversations/{id}/read`. Marks messages up to and including position `up_to` as
/// read. The read position only moves forward; an older value changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkReadRequest {
    pub up_to: EventSeq,
}

/// Response of favorite and read: the resulting state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStateResponse {
    pub state: ConversationUserState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_committed_status_shape() {
        let json = r#"{"status":"conversation_committed","conversation":{"id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","workspace_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d","kind":"channel","name":"design","topic":null,"description":null,"visibility":"public","archived":false,"is_default":false,"created_by":null,"participants":[],"member_count":1}}"#;
        let status: OperationStatus = serde_json::from_str(json).unwrap();
        assert!(matches!(
            status,
            OperationStatus::ConversationCommitted { .. }
        ));
    }

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
    fn conversation_routes_match_their_builders() {
        use lazy_domain::{ConversationId, UserId, WorkspaceId};
        let (ws, conv, user) = (
            WorkspaceId::generate(),
            ConversationId::generate(),
            UserId::generate(),
        );
        let fill = |route: &str| {
            route
                .replace("{workspace_id}", &ws.to_string())
                .replace("{conversation_id}", &conv.to_string())
                .replace("{user_id}", &user.to_string())
        };
        assert_eq!(paths::channels(ws), fill(paths::CHANNELS_ROUTE));
        assert_eq!(
            paths::browse_channels(ws),
            fill(paths::BROWSE_CHANNELS_ROUTE)
        );
        assert_eq!(
            paths::open_conversation(ws),
            fill(paths::OPEN_CONVERSATION_ROUTE)
        );
        assert_eq!(paths::conversation(conv), fill(paths::CONVERSATION_ROUTE));
        assert_eq!(paths::member(conv, user), fill(paths::MEMBER_ROUTE));
        for (action, route) in [
            ("join", paths::JOIN_ROUTE),
            ("leave", paths::LEAVE_ROUTE),
            ("archive", paths::ARCHIVE_ROUTE),
            ("unarchive", paths::UNARCHIVE_ROUTE),
            ("members", paths::MEMBERS_ROUTE),
            ("favorite", paths::FAVORITE_ROUTE),
            ("read", paths::READ_ROUTE),
        ] {
            assert_eq!(
                paths::conversation_action(conv, action),
                fill(route),
                "{action}"
            );
        }
    }

    #[test]
    fn create_channel_requests_validate_the_name() {
        let ok = r#"{"operation_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","name":"design","visibility":"private","topic":null,"description":null}"#;
        assert!(serde_json::from_str::<CreateChannelRequest>(ok).is_ok());
        assert!(
            serde_json::from_str::<CreateChannelRequest>(&ok.replace("design", "Not A Slug"))
                .is_err()
        );
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
