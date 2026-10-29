//! Notifications describing what changed in the read model.

use lazy_domain::{AttachmentId, ConversationId, EmojiId, MessageId, UserId};

use crate::{ActionKind, ComposerTarget, Effect};

/// What changed. Apply the changes of one [`crate::Output`] in order, then read the new state
/// from the core. A timeline change describes the row list as it was at that point in the
/// sequence; row contents are always read from the core's final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    PhaseChanged,
    ConnectionChanged,
    /// Workspaces, conversations, members, or display names changed.
    DirectoryChanged,
    SelectionChanged,
    DraftChanged {
        conversation: ConversationId,
    },
    /// All rows of the timeline were replaced; rebuild any list state.
    TimelineReset {
        conversation: ConversationId,
    },
    /// `removed` rows starting at `at` were replaced by `inserted` new rows. Prepending older
    /// history is a splice at 0.
    TimelineSplice {
        conversation: ConversationId,
        at: usize,
        removed: usize,
        inserted: usize,
    },
    /// The row at `index` changed in place (delivery state or content); remeasure it.
    TimelineItemUpdated {
        conversation: ConversationId,
        index: usize,
    },
    HistoryStatusChanged {
        conversation: ConversationId,
    },
    NoticeChanged,
    /// A conversation's title, member count, archived flag, favorite flag, unread counts, or
    /// "New messages" marker changed. Sidebar sections may need re-reading.
    ConversationStateChanged {
        conversation: ConversationId,
    },
    /// `ClientCore::browse` (the public channel directory) changed.
    BrowseChanged,
    /// Progress of a management action changed. Read it with `ClientCore::action`.
    ActionChanged(ActionKind),
    /// A created channel or an opened direct/group conversation is ready and has been selected.
    /// Navigate to it.
    ConversationOpened(ConversationId),
    /// `ClientCore::jump_target` was set (scroll to it and highlight it) or cleared. Set again
    /// when the same message is opened a second time.
    JumpTargetChanged,
    /// `ClientCore::emoji_prefs` changed (skin tone or recent emoji).
    EmojiPrefsChanged,
    /// `ClientCore::groups` changed.
    GroupsChanged,
    /// `ClientCore::conversation_members` of this conversation changed or arrived.
    ConversationMembersChanged {
        conversation: ConversationId,
    },
    /// The thread pane was opened, closed, or replaced by another thread
    /// (`ClientCore::open_thread`).
    OpenThreadChanged,
    /// All replies of the open thread were replaced; rebuild any list state.
    ThreadReset {
        root: MessageId,
    },
    /// `removed` replies starting at `at` (an index into the replies) were replaced by `inserted`
    /// new ones.
    ThreadSplice {
        root: MessageId,
        at: usize,
        removed: usize,
        inserted: usize,
    },
    /// The reply at `index` changed in place.
    ThreadItemUpdated {
        root: MessageId,
        index: usize,
    },
    /// The root of the open thread arrived or changed in place (its content, reactions, or
    /// summary). The root is not among the replies.
    ThreadRootChanged {
        root: MessageId,
    },
    /// The loading state of the replies changed.
    ThreadHistoryChanged {
        root: MessageId,
    },
    /// The person's following flag, read position, or counts for this thread changed.
    ThreadStateChanged {
        root: MessageId,
    },
    /// The reply composer text of this thread changed.
    ThreadDraftChanged {
        root: MessageId,
    },
    /// `ClientCore::created_webhook` was set or cleared.
    CreatedWebhookChanged,
    /// `ClientCore::webhooks` of this conversation changed.
    WebhooksChanged {
        conversation: ConversationId,
    },
    /// `ClientCore::composer_attachments` of this composer changed (a chip was added, removed,
    /// or moved on). Sent messages still uploading change through `TimelineItemUpdated` and
    /// `ThreadItemUpdated`.
    ComposerAttachmentsChanged {
        target: ComposerTarget,
    },
    /// `ClientCore::download` of this attachment changed.
    DownloadChanged {
        attachment: AttachmentId,
    },
    /// `ClientCore::thumbnail` or `ClientCore::full_image` of this attachment changed.
    ImageChanged {
        attachment: AttachmentId,
    },
    /// `ClientCore::search` changed: the view opened or closed, the text or sort changed, a request
    /// started, or hits arrived, failed, or went.
    SearchChanged,
    /// `ClientCore::recent_searches` changed.
    RecentSearchesChanged,
    /// The notification settings, a conversation's preference, or whether notifications are
    /// paused changed.
    NotificationsChanged,
    /// Someone's status changed, or one ended.
    StatusChanged,
    /// `ClientCore::inbox` changed.
    InboxChanged,
    /// `ClientCore::attention_count` changed.
    AttentionChanged,
    /// A message was saved or unsaved (`is_saved`), or `ClientCore::saved` changed.
    SavedChanged,
    /// `ClientCore::people` changed.
    PeopleChanged,
    /// The workspace's name or settings changed, or the person's own role in it (so what the
    /// interface offers may have changed), or the access to it was lost or came back.
    WorkspaceChanged,
    /// `ClientCore::invitations` changed.
    InvitationsChanged,
    /// `ClientCore::profile` changed.
    ProfileChanged,
    /// `ClientCore::custom_emoji` changed (added, renamed, deleted, or replaced by a bootstrap).
    CustomEmojiChanged,
    /// `ClientCore::emoji_image` of this custom emoji changed.
    EmojiImageChanged {
        emoji: EmojiId,
    },
    /// `ClientCore::workspace_rail` changed (a workspace arrived or left, a name, an attention
    /// count, an unread mark, a connection state).
    RailChanged,
    /// `ClientCore::active_workspace` changed: everything workspace-scoped is now another
    /// workspace's (conversations, the selection, members, groups, emoji, notification settings,
    /// the views).
    ActiveWorkspaceChanged,
    /// `ClientCore::appearance` changed.
    AppearanceChanged,
    /// `ClientCore::avatar` of this person changed (the picture arrived or a new version is
    /// known). Names and versions also come with `DirectoryChanged`.
    AvatarChanged {
        user: UserId,
    },
    /// `ClientCore::server()` changed: the address in use, the one being tried, or a check.
    ServerChanged,
}

/// Result of [`crate::ClientCore::handle`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    /// Work for the runtime. Pass to `Runtime::execute`.
    pub effects: Vec<Effect>,
    /// Read-model changes for the UI, in order.
    pub changes: Vec<Change>,
}

impl Output {
    pub fn is_empty(&self) -> bool {
        self.effects.is_empty() && self.changes.is_empty()
    }
}
