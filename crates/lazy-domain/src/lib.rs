//! Typed identities, message documents, and the pure rules shared by Lazy's server and clients.
//!
//! Nothing here performs I/O or depends on a UI toolkit, so the same code runs on the server,
//! on every desktop client, and on future phone clients.

pub mod authz;
pub mod document;
pub mod ids;
pub mod names;
pub mod time_stamp;

pub use authz::{ChannelAccess, ChannelVisibility, WorkspaceRole};
pub use document::{Block, DocumentError, Inline, MessageDocument};
pub use ids::{ConversationId, EventSeq, MessageId, OperationId, UserId, WorkspaceId};
pub use names::{ChannelName, DisplayName, NameError, Username};
pub use time_stamp::Timestamp;
