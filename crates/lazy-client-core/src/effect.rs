//! Work the core asks the runtime to perform. The runtime reports results back as
//! [`crate::Input`]s.

use lazy_domain::{ConversationId, EventSeq, MessageDocument, OperationId, WorkspaceId};
use lazy_protocol::{LoginRequest, RegisterRequest};

use crate::cache::ContentCache;
use crate::model::JournalEntry;

/// An instruction to the runtime. Effects returned from one `handle` call may run concurrently,
/// except that the core never emits an effect that depends on another's completion (a send is
/// emitted only after its journal write is reported durable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Answered by `Authenticated` or `AuthFailed`.
    Register(RegisterRequest),
    Login(LoginRequest),
    /// Revoke the session on the server (best effort) and delete all local data and secrets.
    SignOut,
    /// Drop the stored session token but keep local data.
    ForgetSession,
    /// Answered by `BootstrapLoaded` or `BootstrapFailed`.
    LoadBootstrap,
    /// Maintain the realtime subscription for a workspace, resuming after `after`. Replaces any
    /// existing subscription for that workspace. The runtime reconnects on its own and
    /// resubscribes from the last event position it forwarded.
    Subscribe {
        workspace: WorkspaceId,
        after: EventSeq,
    },
    /// Answered by `HistoryLoaded`, echoing `generation`.
    FetchHistory {
        conversation: ConversationId,
        before: Option<EventSeq>,
        limit: Option<u32>,
        generation: u64,
    },
    /// Durably record the submission. Answered by `SubmissionPersisted` only after the data is
    /// safely on disk, or `SubmissionPersistFailed`.
    PersistSubmission(JournalEntry),
    /// Send the message. Answered by `SendResult`. The runtime does not retry mutations.
    SendMessage {
        operation: OperationId,
        conversation: ConversationId,
        document: MessageDocument,
    },
    /// Ask whether an operation committed. Idempotent; the runtime may retry it. Answered by
    /// `OperationStatusResult`.
    QueryOperationStatus {
        operation: OperationId,
    },
    /// The operation reached a final state; remove its journal entry.
    SettleSubmission {
        operation: OperationId,
    },
    /// Save all drafts. Each effect supersedes the previous one; the runtime may debounce.
    PersistDrafts(Vec<(ConversationId, String)>),
    /// Save the content cache, replacing the previous one in a single atomic write. Each effect
    /// supersedes the previous one; the runtime may debounce. The cache carries its own cursor,
    /// which is why the cursor is never saved separately.
    PersistCache(Box<ContentCache>),
    /// Delete the saved content cache (and drop any pending write of it). Emitted when the
    /// cache can no longer be trusted, for example after the server asks for a fresh snapshot.
    DiscardCache,
}
