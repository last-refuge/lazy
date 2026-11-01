//! Read-model and wire-adjacent types exposed by [`crate::ClientCore`].

use std::collections::HashMap;

use lazy_domain::connector::ConnectorProvider;
use lazy_domain::{
    ConversationId, EmojiId, EventSeq, GroupId, InvitationId, MentionTarget, MessageDocument,
    MessageId, OperationId, ReactionEmoji, Timestamp, UserId, WebhookId, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::{
    ApiError, Attachment, Conversation, ConversationUserState, CreatedInvitation, CustomEmoji,
    Invitation, Mentions, ReactionSummary, ThreadSummary, ThreadUserState, UserGroup, Webhook,
    Workspace,
};
use serde::{Deserialize, Serialize};

use crate::attachments::{JournalAttachment, UploadProgress};
use crate::cache::ContentCache;

/// Most pages of older history that opening one permalink loads while looking for its message.
/// A page is 50 messages, so a message further back than this is left for the person to scroll
/// to.
pub const PERMALINK_MAX_PAGES: u32 = 10;

/// Most recently used emoji remembered.
pub const MAX_RECENT_EMOJI: usize = 24;

/// Highest skin tone preference (0 is the default tone, 1 to 5 run from light to dark).
pub const MAX_SKIN_TONE: u8 = 5;

/// Emoji preferences of this device: a person's chosen skin tone and the emoji they used most
/// recently. Kept per profile by the runtime, never synced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmojiPrefs {
    /// `lazy_domain::emoji::skin_tone_index`: 0 is the default tone, 1 to 5 light to dark.
    pub skin_tone: u8,
    /// Most recent first, without repeats, at most [`MAX_RECENT_EMOJI`].
    pub recent: Vec<ReactionEmoji>,
}

impl EmojiPrefs {
    /// The preferences with out-of-range values repaired: a tone beyond the last is the default,
    /// repeats are dropped (keeping the first), and the list is cut to its limit.
    pub fn sanitized(mut self) -> Self {
        if self.skin_tone > MAX_SKIN_TONE {
            self.skin_tone = 0;
        }
        let mut seen = std::collections::HashSet::new();
        self.recent.retain(|emoji| seen.insert(*emoji));
        self.recent.truncate(MAX_RECENT_EMOJI);
        self
    }
}

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

/// Where an edit of one of the person's own messages stands. Read per row; changes arrive as
/// `Change::TimelineItemUpdated`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EditState {
    /// No edit is outstanding.
    #[default]
    None,
    /// An edit is being saved, sent, or reconciled with the server. The row's `document` already
    /// shows the new content.
    Saving,
    /// The edit was not saved. The row shows the last saved content again; `text` is what the
    /// person wrote, so the editor can offer it back. `retry` says what `Input::RetryEdit` does,
    /// as for a failed send.
    Failed {
        reason: String,
        text: String,
        retry: RetryKind,
    },
    /// Someone changed the message after the person began editing it, so the edit was not
    /// applied. The row shows the newer version; `text` is what the person wrote.
    /// `Input::RetryEdit` saves it on top of the newer version, replacing the other change.
    Conflict { text: String },
}

/// One row in a conversation's timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineItem {
    pub key: ItemKey,
    pub author_id: UserId,
    pub document: MessageDocument,
    pub delivery: Delivery,
    /// When the message was last edited, for the "(edited)" mark.
    pub edited_at: Option<Timestamp>,
    pub edit: EditState,
    /// The reactions to show, one entry per emoji in the order each was first used, with who
    /// reacted. Includes the person's own reaction changes that are still being saved, so a
    /// click shows at once; `ClientCore::reacted_by_me` reads it.
    pub reactions: Vec<ReactionSummary>,
    /// The event-log position of the server's reactions this list is built from (0 before any
    /// reaction). A list from a position at or before this is never shown over it.
    pub reactions_as_of: EventSeq,
    /// Who the message notifies by name, as the server decided. Empty for a message that
    /// mentions nobody, and for one this client is still sending.
    pub mentions: Mentions,
    /// For a root that has replies, the thread's summary (the highest version the client has
    /// seen, however it arrived). A deleted root keeps it while replies remain.
    pub thread: Option<ThreadSummary>,
    /// For a reply, the root of its thread.
    pub thread_root: Option<MessageId>,
    /// For a reply: it was also sent to the channel, so the channel's timeline lists it too (it is
    /// the same message, not a copy).
    pub in_channel: bool,
    /// A deleted root that still has replies, shown as a placeholder. Every other deleted message
    /// is removed from view.
    pub deleted: bool,
    /// The files on the message, in the order they were attached. A message with files may have
    /// an empty document (`MessageDocument::has_no_content`). For a message this client is still
    /// sending, these are placeholders: the name, the size, and a guessed media type, with no
    /// SHA-256 and no thumbnail.
    pub attachments: Vec<Attachment>,
    /// For a message this client is sending: how far its files have got. `None` once the message
    /// is confirmed, and for a message without files.
    pub upload: Option<UploadProgress>,
}

impl TimelineItem {
    pub fn is_confirmed(&self) -> bool {
        matches!(self.delivery, Delivery::Confirmed { .. })
    }

    /// Whether the message notifies `user` by name: they are named, in a group that was named,
    /// or `@channel` was used. For highlighting messages that mention the signed-in person.
    pub fn mentions_user(&self, user: UserId) -> bool {
        self.mentions.includes(user)
    }

    /// Whether `user` has reacted with `emoji`, as shown.
    pub fn reacted_by(&self, user: UserId, emoji: ReactionEmoji) -> bool {
        self.reactions
            .iter()
            .any(|reaction| reaction.emoji == emoji && reaction.users.contains(&user))
    }

    /// Whether the message has been edited since it was sent.
    pub fn is_edited(&self) -> bool {
        self.edited_at.is_some()
    }

    /// The server's revision of a confirmed row; 1 is the original.
    pub fn revision(&self) -> Option<u32> {
        match self.delivery {
            Delivery::Confirmed { revision, .. } => Some(revision),
            _ => None,
        }
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

/// What a journaled submission does.
///
/// Journal files written before edits existed have no `kind` and are sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum JournalKind {
    /// Send a new message to the conversation.
    #[default]
    Send,
    /// Replace the content of `message_id`, which the editor saw at `base_revision`.
    Edit {
        message_id: MessageId,
        base_revision: u32,
    },
}

impl JournalKind {
    fn is_send(&self) -> bool {
        matches!(self, Self::Send)
    }
}

/// A submission (a message send or an edit) recorded durably before it is sent, so an
/// interrupted one can be reconciled after a restart. `document` is the message to send, or the
/// new content of the edit.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub operation_id: OperationId,
    pub user_id: UserId,
    pub conversation_id: ConversationId,
    pub document: MessageDocument,
    pub submitted_at: Timestamp,
    /// Absent in entries written by sends, which keeps their stored form unchanged.
    #[serde(default, skip_serializing_if = "JournalKind::is_send")]
    pub kind: JournalKind,
    /// For a send: the root of the thread it replies to. Absent in entries written before
    /// threads, and for sends to the channel itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root: Option<MessageId>,
    /// For a reply: also list it in the channel.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub also_in_channel: bool,
    /// For a send with files: each file, whose copy in the profile's outbox is named by its id.
    /// Absent in entries written before attachments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<JournalAttachment>,
}

/// Local state read by the runtime at startup.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct StartupState {
    /// A stored session exists for the configured server.
    pub session_restored: bool,
    /// Submissions that were journaled but never settled.
    pub journal: Vec<JournalEntry>,
    pub drafts: HashMap<ConversationId, String>,
    /// Reply composer texts saved for threads.
    pub thread_drafts: Vec<ThreadDraft>,
    /// The conversation last open in each workspace, and the caret of each draft.
    pub view: ViewState,
    /// The content cache saved by a previous run, if it was readable. With a restored session it
    /// is shown immediately and revalidated; otherwise it is ignored.
    pub cache: Option<ContentCache>,
    /// The emoji preferences saved for this profile (defaults when none could be read).
    pub emoji_prefs: EmojiPrefs,
    /// The recent searches saved for this profile, most recent first (empty when none could be
    /// read).
    pub recent_searches: Vec<String>,
    /// The conversations last visited, most recent first (empty when none could be read).
    pub recent_conversations: Vec<ConversationId>,
    /// How this device looks and moves (the defaults when nothing could be read).
    pub appearance: crate::workspaces::Appearance,
    /// The workspace that was active when the app last ran (`None` when none was saved).
    pub active_workspace: Option<WorkspaceId>,
    /// Human-readable storage problems found while loading, such as a corrupt file that was set
    /// aside.
    pub problems: Vec<String>,
    /// The server in use, as the runtime resolved it (the environment, else the saved choice,
    /// else the default). Never checked yet.
    pub server: crate::server::ServerInfo,
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

    /// A word for what went wrong, safe to log: the server's error code (`invalid_request`), or
    /// `unknown_outcome`. Never the message or the detail, which can name things.
    pub fn code(&self) -> String {
        match self {
            Self::Rejected(error) => crate::diagnostics::snake_case(&format!("{:?}", error.code)),
            Self::Unknown(_) => "unknown_outcome".to_owned(),
        }
    }

    pub(crate) fn is_unauthenticated(&self) -> bool {
        matches!(self, Self::Rejected(error) if error.code == lazy_protocol::ErrorCode::Unauthenticated)
    }
}

/// Which management action an [`ActionState`] belongs to. Each kind has one state at a time, so a
/// dialog can show progress and errors for exactly the thing it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionKind {
    CreateChannel,
    /// Opening (finding or creating) a direct or group conversation.
    OpenConversation,
    Join(ConversationId),
    Leave(ConversationId),
    AddMembers(ConversationId),
    RemoveMember(ConversationId, UserId),
    /// Renaming, or changing the topic or description.
    UpdateConversation(ConversationId),
    Archive(ConversationId),
    Unarchive(ConversationId),
    SetFavorite(ConversationId),
    /// Creating a user group.
    CreateGroup,
    /// Changing a user group's handle, name, or members.
    UpdateGroup(GroupId),
    DeleteGroup(GroupId),
    /// Deleting a message. Failure leaves the message in place; deleting again retries.
    DeleteMessage(MessageId),
    /// Creating an incoming webhook for a conversation.
    CreateWebhook(ConversationId),
    DeleteWebhook(WebhookId),
    /// Loading a conversation's webhooks.
    LoadWebhooks(ConversationId),
    /// Loading every webhook of a workspace, with how each is doing.
    LoadWorkspaceWebhooks(WorkspaceId),
    /// Loading a webhook's delivery log.
    LoadWebhookDeliveries(WebhookId),
    /// Giving a webhook a new address (the old one stops working).
    NewWebhookAddress(WebhookId),
    /// Loading a workspace's GitHub connection, subscriptions and deliveries (D32).
    LoadGithub(WorkspaceId),
    /// Starting to connect GitHub.
    StartGithubSetup(WorkspaceId),
    /// Subscribing a channel to a repository.
    GithubSubscribe(ConversationId),
    GithubUnsubscribe(lazy_domain::GithubSubscriptionId),
    /// Running a failed GitHub delivery again.
    RetryGithubDelivery(i64),
    DisconnectGithub(WorkspaceId),
    /// Loading a webhook connector's connection, subscriptions and deliveries (D34).
    LoadConnector(WorkspaceId, ConnectorProvider),
    /// Connecting a webhook connector (making its address and bot).
    ConnectConnector(WorkspaceId, ConnectorProvider),
    /// Setting or making a connector's signing secret.
    SetConnectorSecret(WorkspaceId, ConnectorProvider),
    /// Subscribing a channel to a team's or project's activity.
    ConnectorSubscribe(ConversationId, ConnectorProvider),
    ConnectorUnsubscribe(lazy_domain::ConnectorSubscriptionId),
    /// Running a failed connector delivery again.
    RetryConnectorDelivery(i64),
    DisconnectConnector(WorkspaceId, ConnectorProvider),
    /// Loading a workspace's apps (D35).
    LoadApps(WorkspaceId),
    /// Making an app (its token arrives once).
    CreateApp(WorkspaceId),
    /// Giving an app a new token.
    NewAppToken(lazy_domain::AppId),
    /// Changing an app's scopes.
    UpdateAppScopes(lazy_domain::AppId),
    /// Adding an app's bot to a channel, or taking it out.
    AppChannel(lazy_domain::AppId, ConversationId),
    DeleteApp(lazy_domain::AppId),
    /// Loading an app's signing secret (D36).
    LoadAppSigningSecret(lazy_domain::AppId),
    /// Adding a slash command to an app, or removing one.
    AppCommand(lazy_domain::AppId),
    RemoveAppCommand(lazy_domain::AppCommandId),
    /// Loading the slash commands members can use in a workspace.
    LoadCommands(WorkspaceId),
    /// Running a slash command in a conversation.
    RunCommand(ConversationId),
    /// Setting an app's events (D38).
    AppEvents(lazy_domain::AppId),
    /// Adding a custom emoji (one at a time).
    AddCustomEmoji,
    RenameCustomEmoji(EmojiId),
    DeleteCustomEmoji(EmojiId),
    /// Changing the workspace's name or settings.
    UpdateWorkspace,
    /// Setting a person's role.
    SetMemberRole(UserId),
    /// Deactivating or reactivating a person.
    SetMemberDeactivated(UserId),
    /// Creating an invitation (one at a time; never repeated by the core).
    CreateInvitation,
    RevokeInvitation(InvitationId),
    /// Creating a workspace (the core chose its id; repeated until the outcome is known).
    CreateWorkspace,
    /// Joining a workspace with an invitation code.
    AcceptInvitation,
    /// Changing the person's display name.
    UpdateProfile,
    /// Setting or removing the person's avatar.
    SetAvatar,
}

/// A message to scroll to and highlight, set by opening a permalink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JumpTarget {
    pub conversation: ConversationId,
    /// The row to show: find it in `ClientCore::timeline(conversation)` by key, or, when `thread`
    /// is set, among the replies of that open thread.
    pub key: ItemKey,
    /// For a reply that is not in the channel's timeline: the root of the thread whose pane is
    /// open and holds the row.
    pub thread: Option<MessageId>,
}

/// The thread whose pane is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenThread {
    pub conversation: ConversationId,
    pub root: MessageId,
}

/// What the thread pane shows. Borrowed from the core; replies are oldest first and the root is
/// not among them.
#[derive(Debug, Clone, Copy)]
pub struct ThreadView<'a> {
    /// The root message, once it is held (it is, at once, when the channel has it loaded).
    pub root: Option<&'a TimelineItem>,
    pub replies: &'a [TimelineItem],
    /// Loading state of the replies.
    pub history: &'a HistoryStatus,
    /// The person's following flag, read position, and counts for the thread, if the client
    /// holds them.
    pub state: Option<&'a ThreadUserState>,
    /// The reply composer text saved for this thread.
    pub draft: &'a str,
}

/// A short look at a message for a header that points at it ("replied to a thread").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageExcerpt {
    pub author: UserId,
    /// The start of the message as plain text.
    pub text: String,
}

/// A reply composer's saved text, with where it belongs so it goes when the conversation does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadDraft {
    pub root: MessageId,
    pub conversation: ConversationId,
    pub text: String,
    /// Where the caret and selection were, if the composer reported it. Absent in files written
    /// before selections were kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<TextSelection>,
}

/// A caret or selection in a composer's text: `anchor` is where it began and `head` where it
/// ends (the caret moves with `head`); equal means a caret with nothing selected. Offsets are
/// UTF-8 bytes into the text, always on grapheme boundaries when read from the core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextSelection {
    pub anchor: usize,
    pub head: usize,
}

/// The saved caret and selection of one conversation's draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftSelection {
    pub conversation: ConversationId,
    pub selection: TextSelection,
}

/// What the person was looking at, saved so the app reopens as they left it. Not secret and not
/// synced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewState {
    /// The conversation last open in each workspace.
    pub last_selected: Vec<(WorkspaceId, ConversationId)>,
    /// The caret and selection of each conversation's draft.
    pub draft_selections: Vec<DraftSelection>,
}

impl ViewState {
    /// Whether nothing is remembered (so a saved file need not mention it).
    pub fn is_empty(&self) -> bool {
        self.last_selected.is_empty() && self.draft_selections.is_empty()
    }
}

/// A private reply to a slash command: only the person who ran it sees it, until they dismiss it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReply {
    /// The command as typed, e.g. `/deploy web`.
    pub command: String,
    pub app_name: String,
    pub document: lazy_domain::MessageDocument,
}

/// A secret kept out of every debug and display string. Its text is available only by asking.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretUrl(String);

impl SecretUrl {
    pub fn new(url: String) -> Self {
        Self(url)
    }

    /// The text itself. Show it to the person; do not log, save, or copy it anywhere else.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretUrl(<redacted>)")
    }
}

/// A webhook that was just created, with the address to post to. The address is shown once: the
/// server keeps only a hash of its secret. It is held in memory until dismissed, never cached,
/// never saved, and never part of any debug output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedWebhook {
    pub webhook: Webhook,
    pub url: SecretUrl,
}

/// Progress of one management action. Read with `ClientCore::action`; changes arrive as
/// `Change::ActionChanged`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ActionState {
    /// Nothing started, finished successfully, or dismissed.
    #[default]
    Idle,
    /// A request is in flight, or its outcome is being reconciled with the server.
    InFlight,
    /// Failed, with a message suitable for showing in the dialog. Starting the action again, or
    /// `Input::DismissAction`, clears it.
    Failed(String),
}

/// What a management request returned, reported by the runtime in `Input::ActionResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The conversation after join, update, archive, unarchive, or add members.
    Conversation(Conversation),
    /// The person's state after favorite.
    UserState(ConversationUserState),
    /// A user group after it was changed.
    Group(UserGroup),
    /// A conversation's webhooks.
    Webhooks(Vec<Webhook>),
    /// A webhook that was just created, or given a new address, with its one-time address.
    WebhookCreated(CreatedWebhook),
    /// A workspace's webhooks, with how each is doing.
    WorkspaceWebhooks(Vec<lazy_protocol::WorkspaceWebhook>),
    /// A webhook's delivery log, newest first.
    WebhookDeliveries(Vec<lazy_protocol::WebhookDelivery>),
    /// A workspace's GitHub connection, subscriptions and deliveries.
    Github(Box<lazy_protocol::GithubStatusResponse>),
    /// The one-time address that continues connecting GitHub in a browser.
    GithubSetup(SecretUrl),
    /// A webhook connector's connection, subscriptions and deliveries.
    Connector(Box<lazy_protocol::ConnectorStatusResponse>),
    /// A signing secret Lazy made, to paste into the provider's form once.
    ConnectorSecret(SecretUrl),
    /// A workspace's apps, oldest first.
    Apps(Vec<lazy_protocol::App>),
    /// An app just made or given a new token, with the token to show once.
    AppToken(Box<lazy_protocol::App>, SecretUrl),
    /// An app after a change.
    App(Box<lazy_protocol::App>),
    /// An app's signing secret, to show its admin.
    AppSigningSecret(SecretUrl),
    /// The slash commands of a workspace.
    Commands(Vec<lazy_protocol::WorkspaceCommand>),
    /// What came of running a command.
    CommandRan(Box<lazy_protocol::RunCommandResponse>),
    /// A custom emoji after it was renamed.
    CustomEmoji(CustomEmoji),
    /// The workspace after its name or settings were changed.
    Workspace(Workspace),
    /// A person's role after it was set.
    Role { user: UserId, role: WorkspaceRole },
    /// Whether a person is deactivated, after it was set.
    Deactivated { user: UserId, deactivated: bool },
    /// An invitation after it was revoked.
    Invitation(Invitation),
    /// An invitation that was just created, with its one-time code.
    InvitationCreated(CreatedInvitation),
    /// A workspace the person created or joined, as the bootstrap lists it.
    Joined(Box<lazy_protocol::WorkspaceBootstrap>),
    /// The person as others now see them (after a profile or avatar change).
    Profile(lazy_protocol::User),
    /// The request succeeded and returned nothing to apply (leave, remove member, delete group).
    Done,
}

/// What an `@` can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MentionKind {
    User,
    Group,
    /// The whole conversation (`@channel`).
    Channel,
}

/// One suggestion for the text typed after an `@`. See `ClientCore::mention_candidates`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionCandidate {
    pub target: MentionTarget,
    /// What is written after the `@` (lowercase).
    pub handle: String,
    /// The person's or group's display name; a short description for `@channel`.
    pub display_name: String,
    pub kind: MentionKind,
    /// `false` for a workspace member who is not in this public channel: naming them is allowed
    /// but notifies no one, so the list should say so. `true` for everyone and everything else.
    pub in_conversation: bool,
}

/// The sidebar sections, each already ordered for display.
///
/// Every conversation the person belongs to appears in exactly one section: favorites first
/// (whatever their kind), then the rest split into channels and people.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sidebar {
    /// Favorite conversations of any kind, ordered by title.
    pub favorites: Vec<ConversationId>,
    /// Other channels, ordered by name.
    pub channels: Vec<ConversationId>,
    /// Other direct and group conversations, most recently active first.
    pub direct_and_group: Vec<ConversationId>,
}
