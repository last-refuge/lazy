//! Work the core asks the runtime to perform. The runtime reports results back as
//! [`crate::Input`]s.

use std::path::PathBuf;

use lazy_domain::notification::NotificationSettings;
use lazy_domain::{
    AttachmentId, ConversationId, EmojiId, EmojiName, EventSeq, GroupId, InvitationId,
    MessageDocument, MessageId, OperationId, UserId, WebhookId, WorkspaceId,
};
use lazy_protocol::{
    AddMembersRequest, Attachment, BrowseChannelsQuery, CreateChannelRequest, CreateGroupRequest,
    CreateInvitationRequest, CreateWebhookRequest, EditMessageRequest, InboxQuery, LoginRequest,
    OpenConversationRequest, PeopleQuery, PreviewInvitationRequest, RegisterRequest,
    RenameEmojiRequest, SavedQuery, SearchParams, SetConversationNotificationPrefsRequest,
    SetDeactivatedRequest, SetReactionRequest, SetRoleRequest, SetSavedRequest, SetStatusRequest,
    UpdateConversationRequest, UpdateGroupRequest, UpdateWorkspaceRequest,
};

use crate::attachments::FileSource;
use crate::cache::ContentCache;
use crate::model::{EmojiPrefs, JournalEntry, ThreadDraft, ViewState};
use crate::notify::DesktopNotification;

/// An instruction to the runtime. Effects returned from one `handle` call may run concurrently,
/// except that the core never emits an effect that depends on another's completion (a send is
/// emitted only after its journal write is reported durable).
#[derive(Clone, PartialEq, Eq)]
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
    /// Durably record the submission (a send or an edit). Answered by `SubmissionPersisted` only
    /// after the data is safely on disk, or `SubmissionPersistFailed`.
    PersistSubmission(JournalEntry),
    /// Send the message. Answered by `SendResult`. The runtime does not retry mutations. A reply
    /// names the root of its thread.
    SendMessage {
        operation: OperationId,
        conversation: ConversationId,
        document: MessageDocument,
        thread_root: Option<MessageId>,
        also_in_channel: bool,
        /// The files to attach, all uploaded, in order. Empty for most messages.
        attachments: Vec<AttachmentId>,
    },
    /// Copy a chosen file into the profile's encrypted outbox under `attachment`'s name, after
    /// checking its size against the limit. Answered by `FileStaged`, echoing `generation` (a
    /// copy that was cancelled and started again must not be mistaken for the new one). A path
    /// is read by the runtime; pasted bytes are written as they are.
    StageFile {
        attachment: AttachmentId,
        generation: u64,
        source: FileSource,
    },
    /// Upload the staged file with a single `PUT` (the core repeats it after an unknown
    /// outcome; the same id is the same upload). Reports `UploadProgress` while bytes go, and
    /// answers with `UploadResult`, both echoing `generation`.
    UploadFile {
        attachment: AttachmentId,
        conversation: ConversationId,
        generation: u64,
        name: String,
        media_type: String,
        size: u64,
    },
    /// Stop an upload in flight (its result, if it still arrives, is ignored).
    CancelUpload {
        attachment: AttachmentId,
    },
    /// Delete these files' copies in the outbox, stopping any copy or upload of them. Safe to
    /// repeat and safe for ids the runtime never saw.
    ReleaseFiles {
        attachments: Vec<AttachmentId>,
    },
    /// Download a file to `destination` through a temporary file beside it, check its SHA-256,
    /// and rename it into place. Reports `DownloadProgress` and answers with `DownloadResult`.
    DownloadAttachment {
        attachment: Attachment,
        destination: PathBuf,
    },
    /// Stop a download and delete its temporary file.
    CancelDownload {
        attachment: AttachmentId,
    },
    /// Fetch a thumbnail (`full: false`) or a whole image (`full: true`), from the runtime's
    /// cache if it holds it. Answered by `ImageLoaded`. Idempotent.
    FetchImage {
        attachment: AttachmentId,
        full: bool,
    },
    /// Search the workspace. Answered by `SearchResult`, echoing `generation`. A read: the runtime
    /// repeats it after a lost answer, and a newer `Search` (or `CancelSearch`) makes it
    /// pointless.
    Search {
        workspace: WorkspaceId,
        generation: u64,
        params: SearchParams,
    },
    /// Nothing is waiting for the search in flight any more.
    CancelSearch,
    /// Save the recent searches, replacing the previous save. The runtime may debounce.
    PersistRecentSearches(Vec<String>),
    /// Upload a custom emoji's image with a single `PUT` (the core repeats it after an unknown
    /// outcome; the same id is the same upload). The runtime reads the file, refusing one over
    /// 512 KiB. Answered by `CustomEmojiUploaded`.
    UploadEmoji {
        workspace: WorkspaceId,
        emoji: EmojiId,
        name: EmojiName,
        source: FileSource,
    },
    /// Rename a custom emoji. Answered by `ActionResult { kind: RenameCustomEmoji(emoji) }` with
    /// `ActionOutcome::CustomEmoji`. A single attempt.
    RenameEmoji {
        emoji: EmojiId,
        request: RenameEmojiRequest,
    },
    /// Delete a custom emoji. Answered by `ActionResult { kind: DeleteCustomEmoji(emoji) }`.
    /// Idempotent; the runtime may repeat it.
    DeleteEmoji {
        emoji: EmojiId,
    },
    /// Fetch the image of a custom emoji, from the runtime's cache if it holds it. Answered by
    /// `EmojiImageLoaded`. Idempotent.
    FetchEmojiImage {
        emoji: EmojiId,
    },
    /// These custom emoji were deleted: drop their cached images.
    ForgetEmojiImages {
        emojis: Vec<EmojiId>,
    },
    /// These attachments can no longer be read (their message was deleted): drop any cached
    /// thumbnails and images.
    ForgetAttachments {
        attachments: Vec<AttachmentId>,
    },
    /// Fetch a page of a thread's replies, oldest first like history. Answered by
    /// `RepliesLoaded`, echoing `generation`. Idempotent; the runtime may retry it.
    FetchReplies {
        root: MessageId,
        before: Option<EventSeq>,
        limit: Option<u32>,
        generation: u64,
    },
    /// Set whether the person follows a thread. Answered by `ThreadFollowResult`. It sets a
    /// state, so repeating it is harmless; the core repeats it after an unknown outcome and the
    /// runtime makes a single attempt.
    SetThreadFollow {
        root: MessageId,
        following: bool,
    },
    /// Move the person's read position in a thread forward to `up_to`. Answered by
    /// `ThreadReadResult`. At most one is in flight per thread.
    MarkThreadRead {
        root: MessageId,
        up_to: EventSeq,
    },
    /// Create an incoming webhook. Answered by `ActionResult { kind: CreateWebhook(conversation) }`
    /// with `ActionOutcome::WebhookCreated`. Never repeated by the runtime: it carries no
    /// operation id, so an unknown outcome is resolved by listing the webhooks.
    CreateWebhook {
        conversation: ConversationId,
        request: CreateWebhookRequest,
    },
    /// Delete a webhook. Answered by `ActionResult { kind: DeleteWebhook(webhook) }`. Idempotent;
    /// the runtime may retry it.
    DeleteWebhook {
        webhook: WebhookId,
    },
    /// List a conversation's webhooks. Answered by `ActionResult { kind: LoadWebhooks(conversation) }`
    /// with `ActionOutcome::Webhooks`. Idempotent; the runtime may retry it.
    LoadWebhooks {
        conversation: ConversationId,
    },
    /// Save what the person was looking at, replacing the previous save. The runtime may
    /// debounce.
    PersistView(ViewState),
    /// Save the reply composer texts, replacing the previous save (the conversation drafts are
    /// saved by `PersistDrafts` and kept alongside). The runtime may debounce.
    PersistThreadDrafts(Vec<ThreadDraft>),
    /// Edit a message. Answered by `EditResult`. The runtime does not retry mutations; the core
    /// reconciles an unknown outcome with `QueryOperationStatus` and resends the same request.
    EditMessage {
        message: MessageId,
        request: EditMessageRequest,
    },
    /// Create a user group. Answered by `GroupCommandResult`. Never repeated by the runtime; the
    /// core reconciles an unknown outcome with `QueryOperationStatus`.
    CreateGroup {
        workspace: WorkspaceId,
        request: CreateGroupRequest,
    },
    /// Change a user group. Answered by `ActionResult { kind: UpdateGroup(group) }`.
    UpdateGroup {
        group: GroupId,
        request: UpdateGroupRequest,
    },
    /// Delete a user group. Answered by `ActionResult { kind: DeleteGroup(group) }`.
    DeleteGroup {
        group: GroupId,
    },
    /// List the people in a conversation. Answered by `MembersFetched`, echoing `generation`.
    /// Idempotent; the runtime may retry it.
    FetchMembers {
        conversation: ConversationId,
        generation: u64,
    },
    /// Set whether the person has reacted to a message with one emoji. Answered by
    /// `ReactionResult`. It sets a state, so repeating it is harmless; the core repeats it after
    /// an unknown outcome and the runtime makes a single attempt.
    SetReaction {
        message: MessageId,
        request: SetReactionRequest,
    },
    /// Save the emoji preferences of this profile, replacing the previous save. Each effect
    /// supersedes the previous one; the runtime may debounce. Not secret and not synced.
    PersistEmojiPrefs(EmojiPrefs),
    /// Fetch one message. Answered by `MessageFetched`. Idempotent; the runtime may retry it.
    FetchMessage {
        message: MessageId,
    },
    /// Delete a message. Answered by `DeleteResult`. Deleting is idempotent, so the runtime may
    /// retry it after a lost response.
    DeleteMessage {
        message: MessageId,
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
    /// Answered by `ConversationCommandResult`. Never repeated by the runtime; the core reconciles
    /// an unknown outcome with `QueryOperationStatus`.
    CreateChannel {
        workspace: WorkspaceId,
        request: CreateChannelRequest,
    },
    /// Answered by `ConversationCommandResult`. Never repeated by the runtime.
    OpenConversation {
        workspace: WorkspaceId,
        request: OpenConversationRequest,
    },
    /// Answered by `ActionResult { kind: Join(conversation) }`.
    JoinChannel {
        conversation: ConversationId,
    },
    /// Answered by `ActionResult { kind: Leave(conversation) }`.
    LeaveChannel {
        conversation: ConversationId,
    },
    /// Answered by `ActionResult { kind: AddMembers(conversation) }`.
    AddMembers {
        conversation: ConversationId,
        request: AddMembersRequest,
    },
    /// Answered by `ActionResult { kind: RemoveMember(conversation, user) }`.
    RemoveMember {
        conversation: ConversationId,
        user: UserId,
    },
    /// Answered by `ActionResult { kind: UpdateConversation(conversation) }`.
    UpdateConversation {
        conversation: ConversationId,
        request: UpdateConversationRequest,
    },
    /// Answered by `ActionResult { kind: Archive(conversation) }`.
    ArchiveConversation {
        conversation: ConversationId,
    },
    /// Answered by `ActionResult { kind: Unarchive(conversation) }`.
    UnarchiveConversation {
        conversation: ConversationId,
    },
    /// Answered by `ActionResult { kind: SetFavorite(conversation) }`.
    SetFavorite {
        conversation: ConversationId,
        favorite: bool,
    },
    /// Move the read position forward to `up_to`. Answered by `MarkReadResult`. At most one is in
    /// flight per conversation.
    MarkRead {
        conversation: ConversationId,
        up_to: EventSeq,
    },
    /// Delete the saved content cache (and drop any pending write of it). Emitted when the
    /// cache can no longer be trusted, for example after the server asks for a fresh snapshot.
    DiscardCache,

    // ----- Notifications, status, and the thread inbox (slice 3D) -----
    /// `PUT` the notification settings (replaced whole, idempotent). Answered by
    /// `NotificationSettingsResult`.
    SetNotificationSettings {
        workspace: WorkspaceId,
        settings: NotificationSettings,
    },
    /// `PUT` a conversation's preference. Answered by `ConversationPrefsResult`.
    SetConversationNotificationPrefs {
        conversation: ConversationId,
        request: SetConversationNotificationPrefsRequest,
    },
    /// `PUT` the person's status. Answered by `StatusResult`.
    SetStatus {
        workspace: WorkspaceId,
        request: SetStatusRequest,
    },
    /// `GET` a page of the thread inbox. Answered by `InboxLoaded` with the same `generation`.
    LoadInbox {
        workspace: WorkspaceId,
        generation: u64,
        query: InboxQuery,
    },
    /// Stop waiting for the inbox page in flight; it reports nothing.
    CancelInbox,
    /// `POST` mark every followed thread read. Answered by `InboxReadResult`.
    MarkInboxRead {
        workspace: WorkspaceId,
    },
    /// `PUT` whether a message is saved (idempotent). Answered by `MessageSavedResult`.
    SetMessageSaved {
        message: MessageId,
        request: SetSavedRequest,
    },
    /// `GET` a page of the saved messages. Answered by `SavedLoaded` with the same `generation`.
    LoadSaved {
        workspace: WorkspaceId,
        generation: u64,
        query: SavedQuery,
    },
    /// Stop waiting for the saved page in flight; it reports nothing.
    CancelSaved,
    /// `GET` a page of the people directory. Answered by `PeopleLoaded`.
    LoadPeople {
        workspace: WorkspaceId,
        generation: u64,
        query: PeopleQuery,
    },
    CancelPeople,
    /// `GET` one person. Answered by `ProfileLoaded`.
    LoadProfile {
        workspace: WorkspaceId,
        generation: u64,
        user: UserId,
    },
    CancelProfile,
    /// `GET` a page of the channel directory. Answered by `BrowseLoaded`.
    LoadBrowse {
        workspace: WorkspaceId,
        generation: u64,
        query: BrowseChannelsQuery,
    },
    CancelBrowse,
    /// `PUT` a workspace with the id the core chose. Idempotent; the core repeats it up to three
    /// times after an unknown outcome. Answered by `ActionResult { kind: CreateWorkspace }`
    /// carrying `ActionOutcome::Joined`.
    CreateWorkspace {
        workspace: WorkspaceId,
        request: lazy_protocol::CreateWorkspaceRequest,
    },
    /// `POST` an invitation code. One attempt (it counts a use). Answered by
    /// `ActionResult { kind: AcceptInvitation }` carrying `ActionOutcome::Joined`.
    AcceptInvitation {
        request: lazy_protocol::AcceptInvitationRequest,
    },
    /// `PATCH /v1/me`. Idempotent. Answered by `ActionResult { kind: UpdateProfile }` carrying
    /// `ActionOutcome::Profile`.
    UpdateProfile {
        request: lazy_protocol::UpdateProfileRequest,
    },
    /// Read the picture (checking its size against `MAX_AVATAR_UPLOAD_BYTES` before reading it
    /// all) and `PUT /v1/me/avatar`. Answered by `ActionResult { kind: SetAvatar }` carrying
    /// `ActionOutcome::Profile`.
    SetAvatar {
        source: FileSource,
    },
    /// `DELETE /v1/me/avatar`. Idempotent. Answered like `SetAvatar`.
    RemoveAvatar,
    /// Fetch a person's avatar at a version, from the runtime's cache if it holds it. Answered by
    /// `AvatarLoaded`. Idempotent.
    FetchAvatar {
        user: UserId,
        version: u32,
    },
    /// These people have a newer avatar, or none: drop any cached older versions.
    ForgetAvatars {
        users: Vec<UserId>,
    },
    /// Save which workspace is active (a local preference). Nothing answers.
    PersistActiveWorkspace(Option<WorkspaceId>),
    /// Save the device's appearance. Nothing answers.
    PersistAppearance(crate::workspaces::Appearance),
    /// Save the recently visited conversations (most recent first). Nothing answers.
    PersistRecentConversations(Vec<ConversationId>),
    /// `PATCH` the workspace. Answered by `ActionResult { kind: UpdateWorkspace }`.
    UpdateWorkspace {
        workspace: WorkspaceId,
        request: UpdateWorkspaceRequest,
    },
    /// `PUT` a person's role. Answered by `ActionResult { kind: SetMemberRole(user) }`.
    SetMemberRole {
        workspace: WorkspaceId,
        user: UserId,
        request: SetRoleRequest,
    },
    /// `PUT` whether a person is deactivated. Answered by
    /// `ActionResult { kind: SetMemberDeactivated(user) }`.
    SetMemberDeactivated {
        workspace: WorkspaceId,
        user: UserId,
        request: SetDeactivatedRequest,
    },
    /// `GET` the invitations. Answered by `InvitationsLoaded` with the same `generation`.
    LoadInvitations {
        workspace: WorkspaceId,
        generation: u64,
    },
    CancelInvitations,
    /// `POST` a new invitation, once: the outcome of a lost answer is found by listing. Answered by
    /// `ActionResult { kind: CreateInvitation }`.
    CreateInvitation {
        workspace: WorkspaceId,
        request: CreateInvitationRequest,
    },
    /// `DELETE` (revoke) an invitation. Answered by `ActionResult { kind: RevokeInvitation(id) }`.
    RevokeInvitation {
        invitation: InvitationId,
    },
    /// Stop the realtime subscription to a workspace the person no longer has access to.
    Unsubscribe {
        workspace: WorkspaceId,
    },
    /// Show a desktop notification. The platform part (`lazy-desktop`) handles it; the runtime
    /// ignores it.
    Notify(DesktopNotification),
    /// Withdraw the notifications of a conversation (it was read). Platform only.
    ClearNotifications {
        conversation: ConversationId,
    },
    /// Withdraw the notification of a message that was deleted. Platform only.
    RetractNotification {
        message: MessageId,
    },
    /// The attention count changed (the launcher badge, the window title). Platform only.
    SetBadge(u32),
    /// Ask a server (`GET /healthz`, a short timeout, no redirect to another host) which protocol
    /// it speaks. Answered by `ServerChecked`, echoing `generation`.
    CheckServer {
        url: String,
        generation: u64,
    },
    /// Move to this server, which passed the check: save the address, delete the local state and
    /// the secrets of the server left, point requests and realtime connections at the new one,
    /// and load what that server's profile holds. Answered by `ServerSwitched`.
    UseServer {
        url: String,
        generation: u64,
    },
    /// Ask the server in use what a code is for, without using it (`POST /v1/invitations/preview`,
    /// no session needed). Answered by `InvitationPreviewed`, echoing `generation`. A read: the
    /// runtime may repeat it, and a newer preview makes the answer pointless.
    PreviewInvitation {
        request: PreviewInvitationRequest,
        generation: u64,
    },
}
