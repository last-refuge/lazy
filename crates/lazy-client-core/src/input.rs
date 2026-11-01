//! Everything that can happen to the core: person actions and results reported by the runtime.

use std::path::PathBuf;

use lazy_domain::notification::{NotificationSettings, NotifyLevel, UserStatus};
use lazy_domain::workspace::WorkspaceSettings;
use lazy_domain::{
    AttachmentId, ChannelVisibility, ConversationId, EmojiId, EventSeq, GroupId, InvitationId,
    MessageId, OperationId, Permalink, ReactionEmoji, Timestamp, UserId, WebhookId, WorkspaceId,
    WorkspaceRole,
};
use lazy_protocol::InvitationPreview;
use lazy_protocol::{
    Attachment, AttachmentResponse, BootstrapResponse, BrowseChannelsResponse, ChannelSort,
    ConversationMembersResponse, ConversationNotificationPrefsResponse, ConversationResponse,
    ConversationUserState, CustomEmojiResponse, GroupResponse, HistoryPage, InboxPage,
    InboxReadResponse, InvitationList, MessageResponse, NotificationSettingsResponse,
    OperationStatus, PeoplePage, PersonResponse, SavedPage, SavedResponse, SearchResponse,
    SearchSort, SendMessageResponse, ServerFrame, StatusResponse, ThreadStateResponse, User,
};

use crate::attachments::{
    ComposerTarget, FileSource, ImageData, LocalFile, StagedFile, TransferError,
};
use crate::discovery::QuickSwitcherTarget;
use crate::model::{ActionKind, ActionOutcome, RequestError, StartupState};
use crate::onboarding::PreviewFailure;
use crate::server::ServerFacts;

/// Input to [`crate::ClientCore::handle`].
#[derive(Clone)]
pub enum Input {
    // ----- Person actions (from the UI) -----
    /// Create an account and sign in. Fields are raw text; the core validates them.
    Register {
        username: String,
        display_name: String,
        password: String,
        /// An invitation code to join a workspace with. Blank means none. A code that cannot be
        /// right is refused here, without a request.
        invite_code: String,
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
    /// Send the composer text and the files attached to the conversation's composer. Empty text
    /// with no files is ignored; empty text with files (reading, uploading, or uploaded) sends a
    /// message that is only files. A failed file blocks the send with a notice (retry or remove
    /// it first). The draft and the files leave the composer once accepted.
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

    // ----- Editing, deleting, and permalinks (person actions) -----
    /// Replace the content of one of the person's own messages. `text` is composer source (see
    /// `MessageDocument::from_markdown`); `ClientCore::edit_text` gives the text to start from.
    /// Empty text, text that changes nothing, and text over the document limits are refused
    /// with a notice or ignored. While an edit of the message is being saved, a further edit
    /// waits and is sent on top of the saved one; only the latest text is kept.
    EditMessage {
        conversation: ConversationId,
        message: MessageId,
        text: String,
    },
    /// Drop the waiting edit of a message, or forget a failed or conflicted edit and show the
    /// message as the server has it. An edit already being sent cannot be taken back.
    CancelEdit {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Try a failed edit again, as described by its [`crate::RetryKind`], or save a conflicted
    /// edit on top of the newer version of the message.
    RetryEdit {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Delete a message: the author's own, or anyone's for an admin or owner. Progress and
    /// failure are `ClientCore::action(ActionKind::DeleteMessage(message))`.
    DeleteMessage {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Go to a message named by a permalink: select its conversation, load older history until
    /// it is found, and set `ClientCore::jump_target`. Problems are reported as a notice.
    OpenPermalink(Permalink),
    /// The view has scrolled to and highlighted the jump target; forget it.
    ClearJumpTarget,

    // ----- Threads (person actions) -----
    /// Open the pane of the thread whose root is `root` in `conversation`, selecting the
    /// conversation first if another is open. Fetches the root if the channel has not loaded it,
    /// and the newest replies. Opening a thread never moves the channel's reading position.
    OpenThread {
        conversation: ConversationId,
        root: MessageId,
    },
    /// Close the thread pane.
    CloseThread,
    /// Fetch the replies older than the oldest loaded one.
    LoadOlderReplies {
        root: MessageId,
    },
    /// Reply in the thread of `root`. `text` is composer source, parsed exactly like
    /// `SendMessage` (shortcodes, then mentions through `mention_directory`). With
    /// `also_in_channel` the reply is also listed in the conversation's history. Empty text is
    /// ignored; the thread's draft is cleared once accepted.
    SendReply {
        conversation: ConversationId,
        root: MessageId,
        text: String,
        also_in_channel: bool,
    },
    /// The reply composer text of a thread changed. Persisted with a debounce.
    SetThreadDraft {
        root: MessageId,
        text: String,
    },
    /// Follow or stop following a thread. The display changes at once and the request follows.
    SetThreadFollow {
        root: MessageId,
        following: bool,
    },
    /// Whether the newest reply of the open thread is on screen. A thread is assumed *not* at the
    /// bottom until the view says so, and the report is forgotten when the thread closes.
    ThreadAtBottom {
        root: MessageId,
        at_bottom: bool,
    },

    // ----- Attachments (person actions) -----
    /// Add files to a composer (the file dialog, a drop, or pasted bytes). Each becomes a chip
    /// at once (`ClientCore::composer_attachments`): the runtime copies it into its outbox, then
    /// it uploads, up to three at a time. More than ten files in a composer, or a file over the
    /// size limit, is refused with a notice or a failed chip.
    AttachFiles {
        target: ComposerTarget,
        files: Vec<LocalFile>,
    },
    /// Take a file out of a composer in any state: its upload stops and its copy is deleted.
    RemoveAttachment {
        target: ComposerTarget,
        attachment: AttachmentId,
    },
    /// Stop reading or uploading a file. The chip stays, failed with "Cancelled", so it can be
    /// retried or removed.
    CancelAttachment {
        target: ComposerTarget,
        attachment: AttachmentId,
    },
    /// Upload a failed file again (only when its `Failed` state says `can_retry`).
    RetryAttachment {
        target: ComposerTarget,
        attachment: AttachmentId,
    },
    /// Remove a message that failed to send, with the copies of its files. Only a failed row
    /// can be discarded.
    DiscardOperation {
        conversation: ConversationId,
        operation: OperationId,
    },
    /// Save a file to `destination`, an exact path the person chose. It is written to a
    /// temporary file beside it, checked against `attachment.sha256`, and renamed into place.
    /// Progress is `ClientCore::download(attachment.id)`.
    DownloadAttachment {
        attachment: Attachment,
        destination: PathBuf,
    },
    /// Stop a download; its temporary file is deleted.
    CancelDownload(AttachmentId),
    /// Forget a finished or failed download's state (when its toast is dismissed).
    DismissDownload(AttachmentId),
    /// Ask for a thumbnail (`Attachment::has_thumbnail`). Idempotent and cheap to repeat, so a
    /// row can ask whenever it is drawn; the answer is `ClientCore::thumbnail`.
    LoadThumbnail(AttachmentId),
    /// Ask for the whole image, for a viewer. The answer is `ClientCore::full_image`.
    LoadFullImage(AttachmentId),
    /// The viewer closed; the full image need not be held.
    ReleaseFullImage(AttachmentId),

    // ----- Search (person actions) -----
    /// Open the search view (`ClientCore::search`). Opening it again changes nothing.
    OpenSearch,
    /// The text in the search field changed. The core records it and parses it (see
    /// `SearchView::parsed`) and asks for nothing: the screen waits for a pause in typing and
    /// then sends `RunSearch`.
    SetSearchQuery {
        text: String,
    },
    /// Search for the current text and sort, from the first result. Ignored when the text does
    /// not parse (nothing to search for, a bad date, too long). A newer run makes the answer to
    /// an older one irrelevant. The results shown so far are dropped.
    RunSearch,
    /// Ask for the next page of the results shown, when there is one (`has_more`, not at the
    /// limit, nothing loading). Also retries a next page that failed.
    LoadMoreSearch,
    /// Order the results by relevance or by date; runs the search again.
    SetSearchSort(SearchSort),
    /// Close the search view. Recent searches are kept.
    CloseSearch,
    /// Open the hit at `index` of `SearchView::hits` as a permalink does: select its conversation,
    /// scroll to it and highlight it (`jump_target`), open its thread for a reply, or, for a
    /// public channel the person has not joined, offer to join it. The search that found it
    /// becomes a recent search. The view stays open; close it when the conversation is shown.
    OpenSearchHit(usize),

    // ----- Custom emoji (person actions) -----
    /// Add a custom emoji to the workspace (admins and owners). `name` is raw text the core
    /// validates (`EmojiName`); `file` is the image (a path or pasted bytes, at most 512 KiB).
    /// Progress is `ClientCore::action(ActionKind::AddCustomEmoji)`.
    AddCustomEmoji {
        name: String,
        file: LocalFile,
    },
    /// Rename a custom emoji. Messages and reactions that used it show the new name.
    RenameCustomEmoji {
        id: EmojiId,
        name: String,
    },
    /// Delete a custom emoji. Messages and reactions that used it keep showing `:name:`.
    DeleteCustomEmoji(EmojiId),
    /// Ask for the image of a custom emoji (idempotent and cheap to repeat; the answer is
    /// `ClientCore::emoji_image`).
    LoadEmojiImage(EmojiId),

    // ----- Webhooks and composer positions (person actions) -----
    /// Create an incoming webhook for a conversation (admins and owners). `name` is raw text the
    /// core validates (it becomes the bot's display name). On success the one-time address is
    /// `ClientCore::created_webhook`. Progress is `action(ActionKind::CreateWebhook(conversation))`.
    CreateWebhook {
        conversation: ConversationId,
        name: String,
    },
    /// Delete a webhook. Its bot leaves the conversation; past messages stay.
    DeleteWebhook(WebhookId),
    /// Load a conversation's webhooks into `ClientCore::webhooks`.
    LoadWebhooks(ConversationId),
    /// Load every webhook of a workspace that the person can see, with how each is doing, into
    /// `ClientCore::workspace_webhooks` (admins and owners).
    LoadWorkspaceWebhooks(WorkspaceId),
    /// Load a webhook's delivery log into `ClientCore::webhook_deliveries`.
    LoadWebhookDeliveries(WebhookId),
    /// Load the workspace's GitHub connection, subscriptions and deliveries into
    /// `ClientCore::github` (admins and owners, D32).
    LoadGithub(WorkspaceId),
    /// Start connecting GitHub for an organization. The address to open in a browser arrives in
    /// `ClientCore::github_setup_url`.
    StartGithubSetup {
        workspace: WorkspaceId,
        organization: String,
    },
    /// Forget the setup address (it was opened, or the view closed).
    DismissGithubSetup,
    /// Make a channel follow a repository (or change what it follows).
    GithubSubscribe {
        conversation: ConversationId,
        repository: String,
        events: Vec<lazy_domain::github::GithubEvent>,
    },
    GithubUnsubscribe(lazy_domain::GithubSubscriptionId),
    /// Run a failed or unknown GitHub delivery again.
    RetryGithubDelivery {
        workspace: WorkspaceId,
        delivery: i64,
    },
    DisconnectGithub(WorkspaceId),
    /// Give a webhook a new address. The old one stops working at once; the new one arrives in
    /// `ClientCore::created_webhook`, to be shown once. Progress is
    /// `action(ActionKind::NewWebhookAddress(webhook))`.
    NewWebhookAddress(WebhookId),
    /// Forget the address shown after creating a webhook (when its dialog is closed).
    DismissCreatedWebhook,
    /// The caret or selection in a conversation's composer moved. Saved with the draft.
    SetDraftSelection {
        conversation: ConversationId,
        anchor: usize,
        head: usize,
    },
    /// The caret or selection in a thread's reply composer moved.
    SetThreadDraftSelection {
        root: MessageId,
        anchor: usize,
        head: usize,
    },

    // ----- User groups (person actions) -----
    /// Create a user group (admins and owners). `handle` and `name` are raw text the core
    /// validates (a leading `@` and capitals in the handle are forgiven). `members` are people of
    /// the workspace. Progress is `ClientCore::action(ActionKind::CreateGroup)`.
    CreateGroup {
        handle: String,
        name: String,
        members: Vec<UserId>,
    },
    /// Change a group. `None` leaves a field unchanged; `members`, when given, is the complete
    /// new membership. Progress is `action(ActionKind::UpdateGroup(id))`.
    UpdateGroup {
        id: GroupId,
        handle: Option<String>,
        name: Option<String>,
        members: Option<Vec<UserId>>,
    },
    /// Delete a group. Messages that mentioned it keep the handle they were written with.
    DeleteGroup(GroupId),

    // ----- Reactions and emoji preferences (person actions) -----
    /// Flip the person's reaction with `emoji` on a message: add it if they have not reacted
    /// (as shown), remove it if they have. The display changes at once and the request follows;
    /// rapid toggles of the same emoji collapse to the final state. See `ClientCore::reacted_by_me`.
    ToggleReaction {
        conversation: ConversationId,
        message: MessageId,
        emoji: ReactionEmoji,
    },
    /// The person picked `emoji` (from the picker or an autocomplete). It becomes the first of
    /// `ClientCore::emoji_prefs().recent`.
    RecordEmojiUse(ReactionEmoji),
    /// Choose the skin tone for emoji that have one: 0 is the default tone, 1 to 5 run from
    /// light to dark. Anything else is ignored.
    SetSkinTone(u8),

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
    /// Open the channel directory with its current settings (`ClientCore::browse`) and load the
    /// first page.
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
    /// Result of an edit request.
    EditResult {
        operation: OperationId,
        result: Result<MessageResponse, RequestError>,
    },
    /// Result of creating a group, the one group action that carries an operation id. An unknown
    /// outcome is reconciled by asking the server, never by repeating the request with a new id.
    GroupCommandResult {
        operation: OperationId,
        result: Result<GroupResponse, RequestError>,
    },
    /// Result of fetching who is in a conversation.
    MembersFetched {
        conversation: ConversationId,
        generation: u64,
        result: Result<ConversationMembersResponse, RequestError>,
    },
    /// Result of fetching a page of a thread's replies, echoing `generation`.
    RepliesLoaded {
        root: MessageId,
        generation: u64,
        result: Result<HistoryPage, RequestError>,
    },
    /// Result of a follow request.
    ThreadFollowResult {
        root: MessageId,
        result: Result<ThreadStateResponse, RequestError>,
    },
    /// Result of marking a thread read up to `up_to`.
    ThreadReadResult {
        root: MessageId,
        up_to: EventSeq,
        result: Result<ThreadStateResponse, RequestError>,
    },
    /// Result of a reaction request.
    ReactionResult {
        message: MessageId,
        emoji: ReactionEmoji,
        result: Result<MessageResponse, RequestError>,
    },
    /// Result of fetching one message.
    MessageFetched {
        message: MessageId,
        result: Result<MessageResponse, RequestError>,
    },
    /// The runtime copied a chosen file into its outbox (or could not).
    FileStaged {
        attachment: AttachmentId,
        generation: u64,
        result: Result<StagedFile, TransferError>,
    },
    /// Bytes of an upload have gone (throttled by the runtime). `generation` echoes the effect.
    UploadProgress {
        attachment: AttachmentId,
        generation: u64,
        sent: u64,
        total: u64,
    },
    /// An upload finished, echoing `generation`.
    UploadResult {
        attachment: AttachmentId,
        generation: u64,
        result: Result<AttachmentResponse, TransferError>,
    },
    /// Bytes of a download have arrived (throttled by the runtime).
    DownloadProgress {
        attachment: AttachmentId,
        received: u64,
        total: u64,
    },
    /// A download finished: the checked file is at the path, or it failed.
    DownloadResult {
        attachment: AttachmentId,
        result: Result<PathBuf, TransferError>,
    },
    /// The answer to the search request made as `generation`.
    SearchResult {
        generation: u64,
        result: Result<SearchResponse, RequestError>,
    },
    /// The upload of a custom emoji's image finished (echoing the emoji's id), or the file could
    /// not be read.
    CustomEmojiUploaded {
        emoji: EmojiId,
        result: Result<CustomEmojiResponse, TransferError>,
    },
    /// The image of a custom emoji arrived.
    EmojiImageLoaded {
        emoji: EmojiId,
        result: Result<ImageData, TransferError>,
    },
    /// A thumbnail (`full: false`) or a whole image (`full: true`) arrived.
    ImageLoaded {
        attachment: AttachmentId,
        full: bool,
        result: Result<ImageData, TransferError>,
    },
    /// Result of a delete request.
    DeleteResult {
        message: MessageId,
        result: Result<MessageResponse, RequestError>,
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

    // ----- Notifications, status, and the thread inbox (slice 3D) -----
    /// The time now. The runtime sends it with the first realtime frames of a batch and every 30
    /// seconds; the core has no clock of its own. It hides statuses that have ended and ends a
    /// snooze for display.
    Clock(Timestamp),
    /// Replace the person's notification settings (optimistic; saved with one idempotent `PUT`).
    SetNotificationSettings(NotificationSettings),
    /// Set the person's preference for a conversation: its own level (`None` follows the
    /// workspace) and whether it is muted.
    SetConversationNotificationPrefs {
        conversation: ConversationId,
        level: Option<NotifyLevel>,
        muted: bool,
    },
    /// Pause notifications until a time, or resume them with `None` (edits `dnd_until` in the
    /// settings; quiet hours are untouched).
    PauseNotifications {
        until: Option<Timestamp>,
    },
    /// Result of saving the settings.
    NotificationSettingsResult {
        /// The workspace the settings are for (it may not be the active one by now).
        workspace: WorkspaceId,
        result: Result<NotificationSettingsResponse, RequestError>,
    },
    /// Result of saving a conversation's preference.
    ConversationPrefsResult {
        conversation: ConversationId,
        result: Result<ConversationNotificationPrefsResponse, RequestError>,
    },
    /// Set the person's status, or clear it with `None` (optimistic; one idempotent `PUT`).
    SetStatus(Option<UserStatus>),
    /// Result of saving the status.
    StatusResult {
        /// The workspace the status was set in.
        workspace: WorkspaceId,
        result: Result<StatusResponse, RequestError>,
    },
    /// The person clicked a desktop notification: open its message as a permalink does (a reply
    /// opens its thread). The window is raised by the host.
    NotificationActivated {
        conversation: ConversationId,
        message: MessageId,
    },
    /// Open the thread inbox and load its first page.
    OpenInbox,
    /// Load the first page again, with or without only the threads that have unread replies.
    LoadInbox {
        unread_only: bool,
    },
    /// The next page.
    LoadMoreInbox,
    /// Mark every followed thread read.
    MarkInboxRead,
    CloseInbox,
    /// A page of the inbox arrived.
    InboxLoaded {
        generation: u64,
        result: Result<InboxPage, RequestError>,
    },
    /// Result of marking followed threads read.
    InboxReadResult {
        result: Result<InboxReadResponse, RequestError>,
    },

    // ----- Discovery (slice 3E) -----
    /// Save a message for later, or remove it from the saved messages (optimistic).
    SetMessageSaved {
        message_id: MessageId,
        saved: bool,
    },
    /// Result of saving or unsaving a message.
    MessageSavedResult {
        message_id: MessageId,
        result: Result<SavedResponse, RequestError>,
    },
    /// Open the saved messages and load the first page.
    OpenSaved,
    CloseSaved,
    LoadMoreSaved,
    /// A page of the saved messages arrived.
    SavedLoaded {
        generation: u64,
        result: Result<SavedPage, RequestError>,
    },
    /// Open the people directory and load the first page.
    OpenPeople,
    ClosePeople,
    /// Search the people directory; the newest text wins.
    SetPeopleQuery(String),
    LoadMorePeople,
    /// A page of the people directory arrived.
    PeopleLoaded {
        generation: u64,
        result: Result<PeoplePage, RequestError>,
    },
    /// Open one person's profile.
    OpenProfile {
        user_id: UserId,
    },
    CloseProfile,
    /// A profile arrived.
    ProfileLoaded {
        generation: u64,
        result: Result<PersonResponse, RequestError>,
    },
    /// Search the channel directory; the newest settings win.
    SetBrowseQuery(String),
    SetBrowseJoined(Option<bool>),
    SetBrowseSort(ChannelSort),
    LoadMoreBrowse,
    /// Forget the channel directory's list (it is not kept between openings).
    CloseBrowse,
    /// A page of the channel directory arrived.
    BrowseLoaded {
        generation: u64,
        result: Result<BrowseChannelsResponse, RequestError>,
    },
    /// Go where a quick switcher result says.
    ChooseQuickSwitcher(QuickSwitcherTarget),

    // ----- Workspace administration (slice 3F) -----
    /// Change the workspace's name and/or settings (admins and owners). Progress and failure are
    /// `action(ActionKind::UpdateWorkspace)`.
    UpdateWorkspace {
        name: Option<String>,
        settings: Option<WorkspaceSettings>,
    },
    /// Set a person's role. Progress and failure are `action(ActionKind::SetMemberRole(user))`.
    SetMemberRole {
        user_id: UserId,
        role: WorkspaceRole,
    },
    /// Deactivate or reactivate a person. Progress and failure are
    /// `action(ActionKind::SetMemberDeactivated(user))`.
    SetMemberDeactivated {
        user_id: UserId,
        deactivated: bool,
    },
    /// Whether the people directory also lists deactivated people (admins and owners); asks for
    /// the first page again.
    SetPeopleIncludeDeactivated(bool),
    /// Open the invitations and load them.
    OpenInvitations,
    /// Close them, forgetting the list and any code that was shown.
    CloseInvitations,
    /// Create an invitation. `None` takes the server's default (7 days, unlimited uses).
    CreateInvitation {
        expires_in_secs: Option<u64>,
        max_uses: Option<u32>,
    },
    RevokeInvitation(InvitationId),
    /// Forget the code that was shown after creating an invitation.
    DismissCreatedInvitation,
    // ----- Several workspaces, profile, appearance (slice 3G) -----
    /// Make a workspace the active one: the screen, the selection, the members, the views and the
    /// settings are now its. An id the person does not belong to is ignored. The open inbox,
    /// search, saved list, people, and invitations of the workspace left are closed.
    SwitchWorkspace(WorkspaceId),
    /// Create a workspace and make it active. `name` is raw text (a workspace name). Progress and
    /// failure are `action(ActionKind::CreateWorkspace)`.
    CreateWorkspace {
        name: String,
    },
    /// Join the workspace an invitation code belongs to and make it active. The code is raw text,
    /// checked for its shape here and never saved or logged. Progress and failure are
    /// `action(ActionKind::AcceptInvitation)`.
    AcceptInvitation {
        code: String,
    },
    /// Change the person's display name (raw text; `None` leaves it). Progress and failure are
    /// `action(ActionKind::UpdateProfile)`.
    UpdateProfile {
        display_name: Option<String>,
    },
    /// Set the person's avatar from a picture (a path or pasted bytes; the runtime reads it, off
    /// the core's thread). Progress and failure are `action(ActionKind::SetAvatar)`.
    SetAvatar {
        source: FileSource,
    },
    /// Remove the person's avatar (also `action(ActionKind::SetAvatar)`).
    RemoveAvatar,
    /// Ask for a person's avatar (idempotent and cheap to repeat; the answer is
    /// `ClientCore::avatar`). Ask only for people on screen.
    LoadAvatar(UserId),
    /// Change how this device looks and moves; applied at once and saved.
    SetAppearance(crate::workspaces::Appearance),
    /// The picture of a person at a version arrived.
    AvatarLoaded {
        user: UserId,
        version: u32,
        result: Result<ImageData, TransferError>,
    },
    /// The invitations arrived.
    InvitationsLoaded {
        generation: u64,
        result: Result<InvitationList, RequestError>,
    },

    // ----- Choosing the server -----
    /// Use the server at this address (what the person typed). Only while signed out (or when a
    /// stored session could not be loaded): otherwise ignored with a notice, as it is when the
    /// environment locked the address (`ClientCore::server().locked()`). The address is
    /// normalised (`normalize_server_url`); one that cannot be an address leaves the server in
    /// use alone and says why in `server().check`. Otherwise it is checked; the first answer
    /// that is a compatible Lazy server saves it and moves to it (`server().url`), with the
    /// state of that server as of a first run; any other leaves it as `server().trying`.
    SetServerUrl(String),
    /// Ask the address on screen (`server().trying`, else `server().url`) again. Harmless to
    /// repeat while that is already being asked. If the server in use answers while a stored
    /// session waits (`SessionPhase::Unreachable`), the load is tried again.
    CheckServer,
    /// The runtime asked an address (`Effect::CheckServer`): the protocol version it speaks, or
    /// a short phrase for why nothing usable answered.
    ServerChecked {
        generation: u64,
        result: Result<u32, String>,
    },
    /// The runtime saved the address and moved to it (`Effect::UseServer`), with what it loaded
    /// for that server as `Started` carries it; or could not, for the phrase given.
    ServerSwitched {
        generation: u64,
        result: Result<Box<StartupState>, String>,
    },
    /// What the server at `url` says about taking accounts (D29), or `None` when it says nothing
    /// (an older server). Arrives just before the `ServerChecked` of the same check.
    ServerFacts {
        url: String,
        facts: Option<ServerFacts>,
    },
    /// Ask the server in use what this code is for, without using it (D29). The code is read in
    /// any of the ways a person writes it (case, groups, dashes); blank forgets the last answer.
    /// Only while signed out and with the server settled; answered by `InvitationPreviewed`.
    PreviewInvitation {
        code: String,
    },
    /// The runtime asked (`Effect::PreviewInvitation`), echoing `generation`.
    InvitationPreviewed {
        generation: u64,
        result: Result<InvitationPreview, PreviewFailure>,
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
