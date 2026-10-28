//! Everything that can happen to the core: person actions and results reported by the runtime.

use lazy_domain::{ConversationId, OperationId, WorkspaceId};
use lazy_protocol::{
    BootstrapResponse, HistoryPage, OperationStatus, SendMessageResponse, ServerFrame, User,
};

use crate::model::{RequestError, StartupState};

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
