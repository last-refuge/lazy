//! Lazy's client state machine.
//!
//! [`ClientCore`] holds everything a client knows (session, directory, timelines, drafts, and
//! in-flight sends) and changes only in [`ClientCore::handle`]. It performs no I/O and depends on
//! neither a UI toolkit nor an async runtime: person actions and runtime reports go in as
//! [`Input`]s; [`Effect`]s for the runtime and [`Change`] notifications for the UI come out. A
//! host reads the current state through the accessors on [`ClientCore`].
//!
//! The same code drives the desktop client today and phone clients later.

mod admin;
mod attachments;
mod cache;
mod change;
mod core;
mod diagnostics;
mod discovery;
mod effect;
mod effect_debug;
mod input;
mod input_debug;
mod model;
mod notify;
mod onboarding;
mod redact;
mod search;
mod server;
mod timeline;
mod update;
mod workspaces;

pub use admin::{CreatedInvite, InvitationsView};
pub use attachments::{
    AttachmentState, ComposerAttachment, ComposerTarget, DownloadState, FileBytes, FileSource,
    ImageData, ImageState, JournalAttachment, LocalFile, MAX_CONCURRENT_IMAGE_FETCHES,
    MAX_CONCURRENT_UPLOADS, MAX_HELD_EMOJI_IMAGE_BYTES, MAX_HELD_EMOJI_IMAGES,
    MAX_HELD_FULL_IMAGES, MAX_HELD_THUMBNAIL_BYTES, MAX_HELD_THUMBNAILS, MAX_QUEUED_EMOJI_FETCHES,
    MAX_QUEUED_IMAGE_FETCHES, MAX_UPLOAD_ATTEMPTS, PendingFile, PendingFileState, StagedFile,
    TransferError, UploadProgress, guess_media_type,
};
pub use cache::{
    CachedThreadState, CachedTimeline, CachedUserState, CachedWorkspace, ContentCache,
    MAX_CACHE_BYTES, MAX_CACHED_CONVERSATIONS, MAX_CACHED_MESSAGES, MAX_CACHED_THREAD_STATES,
};
pub use change::{Change, Output};
pub use core::ClientCore;
pub use diagnostics::{DiagnosticEvent, Diagnostics, MAX_DIAGNOSTIC_EVENTS, WorkspaceDiagnostics};
pub use discovery::{
    BrowseView, DIRECTORY_PAGE, MAX_RECENT_CONVERSATIONS, PeopleView, ProfileView,
    QUICK_SWITCHER_LIMIT, QuickSwitcherItem, QuickSwitcherTarget, SAVED_PAGE, SavedView,
};
pub use effect::Effect;
pub use input::{Input, SocketEvent};
pub use model::{
    ActionKind, ActionOutcome, ActionState, CommandReply, ConnectionState, CreatedWebhook,
    Delivery, DraftSelection, EditState, EmojiPrefs, HistoryStatus, ItemKey, JournalEntry,
    JournalKind, JumpTarget, MAX_RECENT_EMOJI, MAX_SKIN_TONE, MentionCandidate, MentionKind,
    MessageExcerpt, OpenThread, PERMALINK_MAX_PAGES, RequestError, RetryKind, SecretUrl,
    SessionPhase, Sidebar, StartupState, TextSelection, ThreadDraft, ThreadView, TimelineItem,
    UNKNOWN_MEMBER_NAME, ViewState, WorkspaceInfo,
};
pub use notify::{
    DesktopNotification, INBOX_PAGE, INBOX_READ_CAP, InboxStatus, InboxView, LoadStatus,
    MAX_NOTIFICATION_BODY,
};
pub use onboarding::{InvitationCheck, PreviewFailure};
pub use search::{MAX_RECENT_SEARCHES, SEARCH_PAGE, SearchStatus, SearchView};
pub use server::{
    DEFAULT_SERVER_URL, MAX_SERVER_URL_CHARS, ServerCheck, ServerFacts, ServerInfo, ServerSource,
    ServerUrlError, normalize_server_url,
};
pub use update::{
    AvailableUpdate, CHECK_INTERVAL, Checksum, MAX_CHECKSUMS_BYTES, Package, RELEASES_URL,
    UpdateFailure, UpdateStatus, Version, parse_checksums,
};
pub use workspaces::{
    Appearance, MAX_HELD_AVATAR_BYTES, MAX_HELD_AVATARS, MAX_QUEUED_AVATAR_FETCHES, MotionChoice,
    RailItem, ThemeChoice,
};
