//! Notifications and attention (slice 3D, D20): the types the screen and the platform read.
//!
//! The rules themselves are `lazy_domain::notification::decide`; this module holds what the core
//! builds around them: a desktop notification, the thread inbox view.

use lazy_domain::notification::NotificationReason;
use lazy_domain::{ConversationId, MessageId};
use lazy_protocol::InboxEntry;

/// Longest notification body, in characters.
pub const MAX_NOTIFICATION_BODY: usize = 140;
/// How many threads the inbox asks for at a time.
pub const INBOX_PAGE: u32 = 20;
/// How many threads the server marks read in one request; the core asks again until none are left.
pub const INBOX_READ_CAP: u32 = 500;

/// A message the person should be told about with a desktop notification.
///
/// Notifications are grouped per conversation: a newer one with the same `tag` replaces the earlier
/// one and says how many new messages there are (`count`, which is also in the `body`).
#[derive(Clone, PartialEq, Eq)]
pub struct DesktopNotification {
    pub conversation: ConversationId,
    /// The newest message, which opening the notification goes to.
    pub message: MessageId,
    /// For a reply: the root of its thread.
    pub thread_root: Option<MessageId>,
    /// The conversation (`#design`), or the sender for a direct message.
    pub title: String,
    /// `Sender: text`, shortened; `New message` when the person asked not to show text.
    pub body: String,
    pub reason: NotificationReason,
    /// What replaces what: the conversation's id.
    pub tag: String,
    /// How many notified messages of this conversation have not been seen (1 for a lone message).
    pub count: u32,
}

impl std::fmt::Debug for DesktopNotification {
    /// The title and the body are messages and names: only their lengths are shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::redact::Text;
        f.debug_struct("DesktopNotification")
            .field("conversation", &self.conversation)
            .field("message", &self.message)
            .field("thread_root", &self.thread_root)
            .field("title", &Text(&self.title))
            .field("body", &Text(&self.body))
            .field("reason", &self.reason)
            .field("tag", &Text(&self.tag))
            .field("count", &self.count)
            .finish()
    }
}

/// Where a list that is loaded a page at a time stands: the thread inbox, the saved messages, the
/// people directory, the channel directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadStatus {
    /// Nothing has been asked since the view opened.
    Idle,
    /// The first page (the list is empty) or the next one (it holds the ones so far) is on its way.
    Loading,
    Ready,
    /// The last request failed; `can_retry` says whether asking again can help. A failed next page
    /// keeps what was loaded.
    Failed {
        reason: String,
        can_retry: bool,
    },
}

/// The thread inbox's name for [`LoadStatus`].
pub type InboxStatus = LoadStatus;

/// The thread inbox, as the screen reads it with `ClientCore::inbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxView {
    /// The threads the person follows, most recently active first.
    pub entries: Vec<InboxEntry>,
    /// Only threads with unread replies were asked for.
    pub unread_only: bool,
    pub status: InboxStatus,
    pub has_more: bool,
    /// "Mark all read" is being sent.
    pub marking_read: bool,
    /// The request this view waits for or last heard about; 0 until it has asked.
    pub generation: u64,
}
