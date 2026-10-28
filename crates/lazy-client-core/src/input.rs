//! Everything that can happen to the core: person actions and results reported by the runtime.

use lazy_domain::{ChannelVisibility, ConversationId, EventSeq, OperationId, UserId, WorkspaceId};
use lazy_protocol::{
    BootstrapResponse, ConversationResponse, ConversationUserState, HistoryPage, OperationStatus,
    SendMessageResponse, ServerFrame, User,
};

use crate::model::{ActionKind, ActionOutcome, RequestError, StartupState};

/// Input to [`crate::ClientCore::handle`].
#[derive(Debug, Clone)]
pub enum Input {
    // ----- Person actions (from the UI) -----
    /// Create an account and sign in. Fields are raw text; the core validates them.
    Register {
        username: String,
        display_name: String,
        password: String,
    },
    SignIn {
        username: String,
        password: String,
    },
    /// Sign out, revoke the session, and delete local data.
    SignOut,
    /// Try loading again after `SessionPhase::Unreachable`.
    RetryLoad,
    SelectConversation(ConversationId),
    /// The composer text changed. Persisted with a debounce.
    SetDraft {
        conversation: ConversationId,
        text: String,
    },
    /// Send the composer text. Empty text is ignored; the draft is cleared once accepted.
    SendMessage {
        conversation: ConversationId,
        text: String,
    },
    /// Retry a failed message, as described by its [`crate::RetryKind`].
    RetryOperation {
        conversation: ConversationId,
        operation: OperationId,
    },
    /// Fetch the page of messages older than the oldest loaded one.
    LoadOlder {
        conversation: ConversationId,
    },
    DismissNotice,

    // ----- Conversations and read state (person actions) -----
    /// Create a channel. Fields are raw text; the core validates them. An empty `topic` or
    /// `description` means none. Success selects the new channel (`Change::ConversationOpened`).
    CreateChannel {
        name: String,
        visibility: ChannelVisibility,
        topic: String,
        description: String,
    },
    /// Open (find or create) the direct or group conversation with exactly these other people.
    /// Opening is idempotent per participant set; success selects it. Adding someone to a group
    /// is opening a new conversation with the larger set.
    OpenConversation {
        participants: Vec<UserId>,
    },
    /// Load the directory of public channels into `ClientCore::browse_results`.
    BrowseChannels,
    JoinChannel(ConversationId),
    LeaveChannel(ConversationId),
    AddMembers {
        id: ConversationId,
        users: Vec<UserId>,
    },
    RemoveMember {
        id: ConversationId,
        user: UserId,
    },
    /// Change a channel's name, topic, or description. `None` leaves a field unchanged; `Some("")`
    /// clears a topic or description.
    UpdateConversation {
        id: ConversationId,
        name: Option<String>,
        topic: Option<String>,
        description: Option<String>,
    },
    ArchiveConversation(ConversationId),
    UnarchiveConversation(ConversationId),
    SetFavorite {
        id: ConversationId,
        favorite: bool,
    },
    /// Forget a failed action's error (for example when its dialog is closed).
    DismissAction(ActionKind),
    /// Whether the app window is focused and visible. Unread messages are marked read only while
    /// it is. Assumed active until told otherwise.
    WindowActive(bool),
    /// Whether the newest message of the conversation is on screen. Must be reported by the
    /// timeline view; a conversation is assumed *not* at the bottom until it says so, and the
    /// report is forgotten when the person leaves the conversation.
    TimelineAtBottom {
        conversation: ConversationId,
        at_bottom: bool,
    },

    // ----- Runtime reports -----
    /// First input after the runtime starts.
    Started(StartupState),
    /// Register or sign-in succeeded. The runtime now holds the session token.
    Authenticated {
        user: User,
    },
    AuthFailed(RequestError),
    /// The server no longer accepts the stored session.
    SessionExpired,
    BootstrapLoaded(Box<BootstrapResponse>),
    BootstrapFailed(RequestError),
    HistoryLoaded {
        conversation: ConversationId,
        generation: u64,
        result: Result<HistoryPage, RequestError>,
    },
    /// The journal entry for `operation` is durable on disk.
    SubmissionPersisted {
        operation: OperationId,
    },
    SubmissionPersistFailed {
        operation: OperationId,
        message: String,
    },
    SendResult {
        operation: OperationId,
        result: Result<SendMessageResponse, RequestError>,
    },
    OperationStatusResult {
        operation: OperationId,
        result: Result<OperationStatus, RequestError>,
    },
    Socket(SocketEvent),
    /// A frame received on the realtime connection.
    Frame(ServerFrame),
    /// A non-fatal local storage failure to surface to the person.
    StorageProblem(String),
    /// Result of creating a channel or opening a conversation, the two actions that carry an
    /// operation id. An unknown outcome is reconciled by asking the server, never by repeating
    /// the request with a new id.
    ConversationCommandResult {
        operation: OperationId,
        result: Result<ConversationResponse, RequestError>,
    },
    /// Result of every other management request, including browsing.
    ActionResult {
        kind: ActionKind,
        result: Result<ActionOutcome, RequestError>,
    },
    /// Result of marking a conversation read up to `up_to`.
    MarkReadResult {
        conversation: ConversationId,
        up_to: EventSeq,
        result: Result<ConversationUserState, RequestError>,
    },
}

/// Lifecycle of one workspace's realtime connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketEvent {
    /// Connecting. `attempt` is 0 for the first connection and counts retries after that.
    Connecting {
        workspace: WorkspaceId,
        attempt: u32,
    },
    Opened {
        workspace: WorkspaceId,
    },
    /// The connection closed or failed; `attempt` is the number of the retry that will follow.
    Closed {
        workspace: WorkspaceId,
        attempt: u32,
    },
}
