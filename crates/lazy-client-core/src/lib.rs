//! Lazy's client state machine.
//!
//! [`ClientCore`] holds everything a client knows (session, directory, timelines, drafts, and
//! in-flight sends) and changes only in [`ClientCore::handle`]. It performs no I/O and depends on
//! neither a UI toolkit nor an async runtime: person actions and runtime reports go in as
//! [`Input`]s; [`Effect`]s for the runtime and [`Change`] notifications for the UI come out. A
//! host reads the current state through the accessors on [`ClientCore`].
//!
//! The same code drives the desktop client today and phone clients later.

mod cache;
mod change;
mod core;
mod effect;
mod input;
mod model;
mod timeline;

pub use cache::{
    CachedTimeline, CachedUserState, CachedWorkspace, ContentCache, MAX_CACHE_BYTES,
    MAX_CACHED_CONVERSATIONS, MAX_CACHED_MESSAGES,
};
pub use change::{Change, Output};
pub use core::ClientCore;
pub use effect::Effect;
pub use input::{Input, SocketEvent};
pub use model::{
    ActionKind, ActionOutcome, ActionState, ConnectionState, Delivery, HistoryStatus, ItemKey,
    JournalEntry, RequestError, RetryKind, SessionPhase, Sidebar, StartupState, TimelineItem,
    UNKNOWN_MEMBER_NAME, WorkspaceInfo,
};
