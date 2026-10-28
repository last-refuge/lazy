//! Read-model and wire-adjacent types exposed by [`crate::ClientCore`].

use std::collections::HashMap;

use lazy_domain::{
    ConversationId, EventSeq, MessageDocument, MessageId, OperationId, Timestamp, UserId,
    WorkspaceRole,
};
use lazy_protocol::{ApiError, Workspace};
use serde::{Deserialize, Serialize};

use crate::cache::ContentCache;

/// Name shown for an author whose profile the client has not received.
pub const UNKNOWN_MEMBER_NAME: &str = "Unknown member";

/// Where the signed-in lifecycle stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPhase {
    /// Starting up or loading the workspace after sign-in. Show a splash or spinner.
    Loading,
    /// No usable session. `last_error` explains why the last attempt failed, if it did.
    SignedOut { last_error: Option<String> },
    /// A sign-in or registration request is in flight.
    SigningIn,
    /// A stored session exists but the server could not be reached. Offer `Input::RetryLoad`.
    Unreachable { message: String },
    /// Workspace data is loaded.
    Ready,
}

/// State of the realtime connection, aggregated over workspaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Connecting for the first time.
    Connecting,
    /// Connected; replaying events the client missed.
    CatchingUp,
    /// Connected and current.
    Live,
    /// The connection dropped; `attempt` counts retries since the last good connection.
    Reconnecting { attempt: u32 },
    /// Not applicable: signed out or no workspace yet.
    Offline,
}

/// Stable identity of a timeline row.
///
/// Messages this client sent keep their `Operation` key from the moment they are composed,
/// through delivery, until the session ends, so a row does not change identity when the server
/// confirms it. Every other message is keyed by its `Message` id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemKey {
    Operation(OperationId),
    Message(MessageId),
}

/// What retrying a failed message does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryKind {
    /// The server definitively rejected it. Retrying composes a new operation from the same text.
    NewOperation,
    /// The outcome is still unknown. Retrying asks the server first, and resends the same
    /// operation only if the server has no record of it.
    SameOperation,
}

/// Delivery state of a timeline row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// Committed by the server.
    Confirmed {
        message_id: MessageId,
        /// Event-log position of creation. Confirmed rows are ordered by this value.
        seq: EventSeq,
        created_at: Timestamp,
        revision: u32,
    },
    /// Being saved locally or sent.
    Pending,
    /// A send did not report a result; the client is asking the server whether it committed.
    UnknownOutcome,
    /// Not delivered. `reason` is suitable for display.
    Failed { reason: String, retry: RetryKind },
}

/// One row in a conversation's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub key: ItemKey,
    pub author_id: UserId,
    pub document: MessageDocument,
    pub delivery: Delivery,
}

impl TimelineItem {
    pub fn is_confirmed(&self) -> bool {
        matches!(self.delivery, Delivery::Confirmed { .. })
    }

    /// Event-log position, for confirmed rows.
    pub fn seq(&self) -> Option<EventSeq> {
        match self.delivery {
            Delivery::Confirmed { seq, .. } => Some(seq),
            _ => None,
        }
    }

    pub fn message_id(&self) -> Option<MessageId> {
        match self.delivery {
            Delivery::Confirmed { message_id, .. } => Some(message_id),
            _ => None,
        }
    }

    pub fn created_at(&self) -> Option<Timestamp> {
        match self.delivery {
            Delivery::Confirmed { created_at, .. } => Some(created_at),
            _ => None,
        }
    }

    /// The operation that created this row, for messages sent by this client.
    pub fn operation_id(&self) -> Option<OperationId> {
        match self.key {
            ItemKey::Operation(operation) => Some(operation),
            ItemKey::Message(_) => None,
        }
    }
}

/// Loading state of a conversation's older history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryStatus {
    /// Nothing requested yet.
    NotLoaded,
    /// A page is being fetched.
    Loading,
    /// The latest fetch succeeded. `has_more` says whether older messages exist.
    Ready { has_more: bool },
    /// The latest fetch failed; another `LoadOlder` or `SelectConversation` retries.
    Failed { message: String },
}

/// A workspace the signed-in person belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub workspace: Workspace,
    pub role: WorkspaceRole,
}

/// A message submission recorded durably before it is sent, so an interrupted send can be
/// reconciled after a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub operation_id: OperationId,
    pub user_id: UserId,
    pub conversation_id: ConversationId,
    pub document: MessageDocument,
    pub submitted_at: Timestamp,
}

/// Local state read by the runtime at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartupState {
    /// A stored session exists for the configured server.
    pub session_restored: bool,
    /// Submissions that were journaled but never settled.
    pub journal: Vec<JournalEntry>,
    pub drafts: HashMap<ConversationId, String>,
    /// The content cache saved by a previous run, if it was readable. With a restored session it
    /// is shown immediately and revalidated; otherwise it is ignored.
    pub cache: Option<ContentCache>,
    /// Human-readable storage problems found while loading, such as a corrupt file that was set
    /// aside.
    pub problems: Vec<String>,
}

/// Failure of a server request, classified by what the caller may safely do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestError {
    /// The server received the request and definitively refused it. Nothing was committed.
    Rejected(ApiError),
    /// The request may or may not have been processed: a timeout, a dropped connection, or a
    /// server failure. The outcome of a mutation must be reconciled before anything is resent.
    Unknown(String),
}

impl RequestError {
    /// Text suitable for showing to a person.
    pub fn message(&self) -> String {
        match self {
            Self::Rejected(error) => error.message.clone(),
            Self::Unknown(detail) => format!("Couldn't reach the server ({detail})."),
        }
    }

    pub(crate) fn is_unauthenticated(&self) -> bool {
        matches!(self, Self::Rejected(error) if error.code == lazy_protocol::ErrorCode::Unauthenticated)
    }
}
