//! Request and response bodies for the HTTP API, and the route table.

use lazy_domain::notification::{
    ConversationNotificationPrefs, NotificationSettings, NotifyLevel, UserStatus,
};
use lazy_domain::workspace::WorkspaceSettings;
use lazy_domain::{
    AttachmentId, ChannelName, ChannelVisibility, ConversationId, ConversationKind, DisplayName,
    EmojiId, EmojiName, EventSeq, GroupHandle, GroupId, InvitationId, MessageDocument, MessageId,
    OperationId, ReactionEmoji, Timestamp, UserId, Username, WebhookId, WorkspaceId, WorkspaceName,
    WorkspaceRole,
};
use serde::{Deserialize, Serialize};

use crate::{InviteCode, Password, SessionToken};

/// Route paths. `*_ROUTE` constants are Axum route patterns; the functions build concrete paths.
pub mod paths {
    use lazy_domain::{
        AttachmentId, ConversationId, EmojiId, GroupId, InvitationId, MessageId, OperationId,
        UserId, WebhookId, WorkspaceId,
    };

    pub const HEALTH: &str = "/healthz";
    /// `GET` what a client may know about a server before anyone has signed in (D29).
    pub const SERVER_INFO: &str = "/v1/server";
    /// `POST` asks what an invitation code is for, without using it (D29).
    pub const PREVIEW_INVITATION: &str = "/v1/invitations/preview";
    pub const REGISTER: &str = "/v1/auth/register";
    pub const LOGIN: &str = "/v1/auth/login";
    pub const LOGOUT: &str = "/v1/auth/logout";
    pub const BOOTSTRAP: &str = "/v1/bootstrap";
    /// WebSocket upgrade. Authenticate with the `Authorization: Bearer` header.
    pub const REALTIME: &str = "/v1/realtime";
    pub const MESSAGES_ROUTE: &str = "/v1/conversations/{conversation_id}/messages";
    pub const OPERATION_ROUTE: &str = "/v1/operations/{operation_id}";
    /// `GET` returns one message, `PATCH` edits it, `DELETE` deletes it.
    pub const MESSAGE_ROUTE: &str = "/v1/messages/{message_id}";
    /// `PUT` sets or clears the caller's reaction with one emoji.
    pub const REACTIONS_ROUTE: &str = "/v1/messages/{message_id}/reactions";
    /// `PUT` replaces the caller's notification settings for a workspace.
    pub const NOTIFICATION_SETTINGS_ROUTE: &str =
        "/v1/workspaces/{workspace_id}/notification-settings";
    /// `PUT` sets the caller's notification preference for one conversation.
    pub const CONVERSATION_NOTIFICATION_ROUTE: &str =
        "/v1/conversations/{conversation_id}/notification-prefs";
    /// `PUT` sets or clears the caller's status.
    pub const STATUS_ROUTE: &str = "/v1/workspaces/{workspace_id}/status";
    /// `GET` lists the caller's followed threads, most recently active first.
    pub const INBOX_ROUTE: &str = "/v1/workspaces/{workspace_id}/inbox";
    /// `POST` marks every followed thread read.
    pub const INBOX_READ_ROUTE: &str = "/v1/workspaces/{workspace_id}/inbox/read";
    /// `GET` searches the workspace's messages and files.
    /// `PATCH` changes the caller's profile (display name).
    pub const ME_ROUTE: &str = "/v1/me";
    /// `PUT` sets the caller's avatar from the image in the request body, `DELETE` removes it.
    pub const ME_AVATAR_ROUTE: &str = "/v1/me/avatar";
    /// `GET` a person's avatar as a PNG, for anyone who shares a workspace with them.
    pub const USER_AVATAR_ROUTE: &str = "/v1/users/{user_id}/avatar";
    /// `POST` joins the workspace an invitation code belongs to.
    pub const ACCEPT_INVITATION_ROUTE: &str = "/v1/invitations/accept";
    /// `PUT` creates a workspace with the id in the path (idempotent), `PATCH` changes its name or
    /// settings (admins and owners).
    pub const WORKSPACE_ROUTE: &str = "/v1/workspaces/{workspace_id}";
    /// `PUT` sets a person's role.
    pub const PERSON_ROLE_ROUTE: &str = "/v1/workspaces/{workspace_id}/people/{user_id}/role";
    /// `PUT` deactivates a person's membership of the workspace, or reactivates it.
    pub const PERSON_DEACTIVATED_ROUTE: &str =
        "/v1/workspaces/{workspace_id}/people/{user_id}/deactivated";
    /// `GET` lists the workspace's invitations, `POST` creates one.
    pub const INVITATIONS_ROUTE: &str = "/v1/workspaces/{workspace_id}/invitations";
    /// `DELETE` revokes an invitation.
    pub const INVITATION_ROUTE: &str = "/v1/invitations/{invitation_id}";
    /// `GET` pages the workspace's people: the people directory.
    pub const PEOPLE_ROUTE: &str = "/v1/workspaces/{workspace_id}/people";
    /// `GET` one person's profile.
    pub const PERSON_ROUTE: &str = "/v1/workspaces/{workspace_id}/people/{user_id}";
    /// `GET` pages the caller's saved messages, most recently saved first.
    pub const SAVED_ROUTE: &str = "/v1/workspaces/{workspace_id}/saved";
    /// `PUT` saves a message for the caller or removes it from their saved messages.
    pub const MESSAGE_SAVED_ROUTE: &str = "/v1/messages/{message_id}/saved";
    pub const SEARCH_ROUTE: &str = "/v1/workspaces/{workspace_id}/search";
    /// `PUT` uploads an image for a new custom emoji. The body is the image's bytes.
    pub const EMOJI_UPLOAD_ROUTE: &str = "/v1/workspaces/{workspace_id}/emoji/{emoji_id}";
    /// `GET` downloads a custom emoji's image, `PATCH` renames it, `DELETE` deletes it.
    pub const EMOJI_ROUTE: &str = "/v1/emoji/{emoji_id}";
    /// The header carrying a new custom emoji's name.
    pub const EMOJI_NAME_HEADER: &str = "x-emoji-name";
    /// `PUT` uploads a file for a conversation. The body is the file's bytes.
    pub const ATTACHMENT_UPLOAD_ROUTE: &str =
        "/v1/conversations/{conversation_id}/attachments/{attachment_id}";
    /// `GET` downloads a file.
    pub const ATTACHMENT_ROUTE: &str = "/v1/attachments/{attachment_id}";
    /// `GET` downloads a file's thumbnail (images only).
    pub const ATTACHMENT_THUMBNAIL_ROUTE: &str = "/v1/attachments/{attachment_id}/thumbnail";
    /// The header carrying an upload's file name, percent-encoded UTF-8.
    pub const ATTACHMENT_NAME_HEADER: &str = "x-attachment-name";
    /// `POST` creates an incoming webhook for a conversation, `GET` lists them.
    pub const WEBHOOKS_ROUTE: &str = "/v1/conversations/{conversation_id}/webhooks";
    /// `DELETE` removes an incoming webhook.
    pub const WEBHOOK_ROUTE: &str = "/v1/webhooks/{webhook_id}";
    /// `GET` lists the incoming webhooks of every channel in the workspace the caller can read,
    /// each with its latest delivery. Admins and owners only.
    pub const WORKSPACE_WEBHOOKS_ROUTE: &str = "/v1/workspaces/{workspace_id}/webhooks";
    /// `GET` lists a webhook's most recent deliveries, newest first.
    pub const WEBHOOK_DELIVERIES_ROUTE: &str = "/v1/webhooks/{webhook_id}/deliveries";
    /// `POST` gives a webhook a new secret: the old address stops working at once and the new one
    /// is shown once.
    pub const WEBHOOK_ADDRESS_ROUTE: &str = "/v1/webhooks/{webhook_id}/address";
    /// `GET` the workspace's GitHub connection, installations, subscriptions and recent
    /// deliveries; `DELETE` disconnects it. Admins and owners only (D32).
    pub const GITHUB_ROUTE: &str = "/v1/workspaces/{workspace_id}/github";
    /// `POST` starts connecting GitHub: answers with an address to open in a browser.
    pub const GITHUB_SETUP_ROUTE: &str = "/v1/workspaces/{workspace_id}/github/setup";
    /// `POST` subscribes a channel to a repository (or changes what it follows).
    pub const GITHUB_SUBSCRIPTIONS_ROUTE: &str =
        "/v1/conversations/{conversation_id}/github/subscriptions";
    /// `DELETE` removes a subscription.
    pub const GITHUB_SUBSCRIPTION_ROUTE: &str = "/v1/github/subscriptions/{subscription_id}";
    /// `POST` runs a failed or unknown GitHub delivery again.
    pub const GITHUB_DELIVERY_RETRY_ROUTE: &str = "/v1/github/deliveries/{delivery_id}/retry";
    /// Public pages and endpoints GitHub and the admin's browser reach. Not part of the API.
    pub const GITHUB_SETUP_PAGE_ROUTE: &str = "/github/setup/{state}";
    pub const GITHUB_CALLBACK_ROUTE: &str = "/github/callback";
    pub const GITHUB_INSTALLED_ROUTE: &str = "/github/installed";
    pub const GITHUB_WEBHOOK_ROUTE: &str = "/github/webhook";
    /// `POST` delivers a message through an incoming webhook. Public: the secret in the path is
    /// the only credential, so it carries no `Authorization` header.
    pub const HOOK_ROUTE: &str = "/hooks/{webhook_id}/{secret}";
    /// `GET` pages a thread's replies, oldest first.
    pub const REPLIES_ROUTE: &str = "/v1/messages/{message_id}/replies";
    /// `PUT` follows or unfollows a thread.
    pub const THREAD_FOLLOW_ROUTE: &str = "/v1/messages/{message_id}/thread/follow";
    /// `POST` moves the caller's read position in a thread forward.
    pub const THREAD_READ_ROUTE: &str = "/v1/messages/{message_id}/thread/read";
    /// `POST` creates a user group, `GET` lists the workspace's groups.
    pub const GROUPS_ROUTE: &str = "/v1/workspaces/{workspace_id}/groups";
    /// `PATCH` changes a user group, `DELETE` deletes it.
    pub const GROUP_ROUTE: &str = "/v1/groups/{group_id}";

    /// `POST` creates a channel.
    pub const CHANNELS_ROUTE: &str = "/v1/workspaces/{workspace_id}/channels";
    /// `GET` lists the workspace's public channels, joined or not.
    pub const BROWSE_CHANNELS_ROUTE: &str = "/v1/workspaces/{workspace_id}/channels/browse";
    /// `POST` opens (finds or creates) a direct or group conversation.
    pub const OPEN_CONVERSATION_ROUTE: &str = "/v1/workspaces/{workspace_id}/conversations";
    /// `PATCH` changes a channel's name, topic, or description.
    pub const CONVERSATION_ROUTE: &str = "/v1/conversations/{conversation_id}";
    /// `POST` joins a public channel.
    pub const JOIN_ROUTE: &str = "/v1/conversations/{conversation_id}/join";
    /// `POST` leaves a channel.
    pub const LEAVE_ROUTE: &str = "/v1/conversations/{conversation_id}/leave";
    pub const ARCHIVE_ROUTE: &str = "/v1/conversations/{conversation_id}/archive";
    pub const UNARCHIVE_ROUTE: &str = "/v1/conversations/{conversation_id}/unarchive";
    /// `POST` adds people to a channel; `GET` lists the people in a conversation.
    pub const MEMBERS_ROUTE: &str = "/v1/conversations/{conversation_id}/members";
    /// `DELETE` removes someone from a channel.
    pub const MEMBER_ROUTE: &str = "/v1/conversations/{conversation_id}/members/{user_id}";
    /// `PUT` sets or clears the caller's favorite flag.
    pub const FAVORITE_ROUTE: &str = "/v1/conversations/{conversation_id}/favorite";
    /// `POST` moves the caller's read position forward.
    pub const READ_ROUTE: &str = "/v1/conversations/{conversation_id}/read";

    /// `GET` pages history, `POST` sends a message.
    pub fn messages(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/messages")
    }

    pub fn message(message_id: MessageId) -> String {
        format!("/v1/messages/{message_id}")
    }

    pub fn notification_settings(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/notification-settings")
    }

    pub fn conversation_notification_prefs(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/notification-prefs")
    }

    pub fn status(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/status")
    }

    pub fn inbox(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/inbox")
    }

    pub fn inbox_read(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/inbox/read")
    }

    pub fn user_avatar(user_id: UserId, version: u32) -> String {
        format!("/v1/users/{user_id}/avatar?v={version}")
    }

    pub fn workspace(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}")
    }

    pub fn person_role(workspace_id: WorkspaceId, user_id: UserId) -> String {
        format!("/v1/workspaces/{workspace_id}/people/{user_id}/role")
    }

    pub fn person_deactivated(workspace_id: WorkspaceId, user_id: UserId) -> String {
        format!("/v1/workspaces/{workspace_id}/people/{user_id}/deactivated")
    }

    pub fn invitations(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/invitations")
    }

    pub fn invitation(invitation_id: InvitationId) -> String {
        format!("/v1/invitations/{invitation_id}")
    }

    pub fn people(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/people")
    }

    pub fn person(workspace_id: WorkspaceId, user_id: UserId) -> String {
        format!("/v1/workspaces/{workspace_id}/people/{user_id}")
    }

    pub fn saved(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/saved")
    }

    pub fn message_saved(message_id: MessageId) -> String {
        format!("/v1/messages/{message_id}/saved")
    }

    pub fn search(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/search")
    }

    pub fn emoji_upload(workspace_id: WorkspaceId, emoji_id: EmojiId) -> String {
        format!("/v1/workspaces/{workspace_id}/emoji/{emoji_id}")
    }

    pub fn emoji(emoji_id: EmojiId) -> String {
        format!("/v1/emoji/{emoji_id}")
    }

    pub fn attachment_upload(
        conversation_id: ConversationId,
        attachment_id: AttachmentId,
    ) -> String {
        format!("/v1/conversations/{conversation_id}/attachments/{attachment_id}")
    }

    pub fn attachment(attachment_id: AttachmentId) -> String {
        format!("/v1/attachments/{attachment_id}")
    }

    pub fn attachment_thumbnail(attachment_id: AttachmentId) -> String {
        format!("/v1/attachments/{attachment_id}/thumbnail")
    }

    pub fn webhooks(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/webhooks")
    }

    pub fn webhook(webhook_id: WebhookId) -> String {
        format!("/v1/webhooks/{webhook_id}")
    }

    pub fn workspace_webhooks(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/webhooks")
    }

    pub fn webhook_deliveries(webhook_id: WebhookId) -> String {
        format!("/v1/webhooks/{webhook_id}/deliveries")
    }

    pub fn webhook_address(webhook_id: WebhookId) -> String {
        format!("/v1/webhooks/{webhook_id}/address")
    }

    pub fn github(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/github")
    }

    pub fn github_setup(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/github/setup")
    }

    pub fn github_subscriptions(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}/github/subscriptions")
    }

    pub fn github_subscription(subscription_id: lazy_domain::GithubSubscriptionId) -> String {
        format!("/v1/github/subscriptions/{subscription_id}")
    }

    pub fn github_delivery_retry(delivery_id: i64) -> String {
        format!("/v1/github/deliveries/{delivery_id}/retry")
    }

    pub fn hook(webhook_id: WebhookId, secret: &str) -> String {
        format!("/hooks/{webhook_id}/{secret}")
    }

    pub fn replies(message_id: MessageId) -> String {
        format!("/v1/messages/{message_id}/replies")
    }

    /// `action` is `follow` or `read`.
    pub fn thread_action(message_id: MessageId, action: &str) -> String {
        format!("/v1/messages/{message_id}/thread/{action}")
    }

    pub fn groups(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/groups")
    }

    pub fn group(group_id: GroupId) -> String {
        format!("/v1/groups/{group_id}")
    }

    pub fn reactions(message_id: MessageId) -> String {
        format!("/v1/messages/{message_id}/reactions")
    }

    pub fn operation(operation_id: OperationId) -> String {
        format!("/v1/operations/{operation_id}")
    }

    pub fn channels(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/channels")
    }

    pub fn browse_channels(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/channels/browse")
    }

    pub fn open_conversation(workspace_id: WorkspaceId) -> String {
        format!("/v1/workspaces/{workspace_id}/conversations")
    }

    pub fn conversation(conversation_id: ConversationId) -> String {
        format!("/v1/conversations/{conversation_id}")
    }

    /// `action` is one of `join`, `leave`, `archive`, `unarchive`, `members`, `favorite`, `read`.
    pub fn conversation_action(conversation_id: ConversationId, action: &str) -> String {
        format!("/v1/conversations/{conversation_id}/{action}")
    }

    pub fn member(conversation_id: ConversationId, user_id: UserId) -> String {
        format!("/v1/conversations/{conversation_id}/members/{user_id}")
    }
}

/// Response of `GET /healthz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub protocol_version: u32,
}

/// Who may create an account on a server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerRegistration {
    /// Anyone may register, with no code.
    Open,
    /// An invitation code is needed (or, for the first account, the setup code).
    InviteOnly,
    /// The server accepts no new accounts, invitations included.
    Closed,
}

/// Response of `GET /v1/server` (D29), which needs no sign-in: what a new client needs to show the
/// right first screen. It holds no name and no count, so a server nobody has invited a stranger to
/// tells that stranger nothing but how to talk to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfoResponse {
    /// As in [`HealthResponse`]: a client that does not speak it cannot use the server.
    pub protocol_version: u32,
    pub registration: ServerRegistration,
    /// The server is waiting for its first account, which needs the setup code (given where an
    /// invitation code goes). False once the workspace has a member.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup_code_required: bool,
}

/// What a code is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationKind {
    /// An invitation to join the workspace it belongs to.
    Invitation,
    /// The server's setup code: the person will be the workspace's first member and its owner.
    Setup,
}

/// `POST /v1/invitations/preview` (D29), which needs no sign-in: asks what a code is for before
/// anything is created or used. A code that is wrong, expired, revoked, used up, or of the wrong
/// shape is `invitation_invalid` with one message, as at registration, and a server that accepts no
/// registrations is `forbidden`. Names are given only for a code that is valid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewInvitationRequest {
    pub code: InviteCode,
}

/// Response of `POST /v1/invitations/preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitationPreview {
    pub kind: InvitationKind,
    pub workspace_name: String,
    /// Who made the invitation; absent for the setup code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inviter: Option<DisplayName>,
    /// How many people are in the workspace now (0 for the setup code).
    pub member_count: u32,
}

/// A person, as other people see them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub id: UserId,
    pub username: Username,
    pub display_name: DisplayName,
    /// An integration's identity, not a person: it cannot sign in and is shown with an "APP"
    /// badge. Bots are never offered as people to message or mention.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_bot: bool,
    /// What the person says they are doing, if they have set something that has not expired
    /// (clients also hide one whose `expires_at` has passed, as the server does not announce
    /// expiry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<UserStatus>,
    /// The person's membership of the workspace is deactivated: they cannot sign in to it, are
    /// not offered for mentions, direct messages, or adding to channels, and their old messages
    /// stay. Only the workspace's own listings set this (it depends on the workspace).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deactivated: bool,
    /// Which picture the person has set as their avatar: 0 for none, otherwise a number that
    /// changes whenever the picture does, so `GET /v1/users/{id}/avatar?v=<this>` can be cached
    /// for good.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub avatar_version: u32,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    #[serde(default)]
    pub settings: WorkspaceSettings,
}

/// A channel, a direct conversation, or a group conversation.
///
/// The same value is sent to everyone who sees the conversation, so it holds nothing that depends
/// on who is looking. Whether the caller belongs to it is conveyed by where it appears: the
/// bootstrap lists only conversations the caller is a member of, and [`BrowsedChannel::joined`]
/// says so for public channels the caller may not have joined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: ConversationId,
    pub workspace_id: WorkspaceId,
    pub kind: ConversationKind,
    /// Present for channels. Direct and group conversations have no name; show their
    /// participants instead.
    pub name: Option<ChannelName>,
    pub topic: Option<String>,
    pub description: Option<String>,
    pub visibility: ChannelVisibility,
    pub archived: bool,
    /// The workspace's default channel: everyone belongs, and it cannot be left or archived.
    pub is_default: bool,
    pub created_by: Option<UserId>,
    /// The people in a direct or group conversation, in a stable order. Empty for channels, whose
    /// membership can be large and is not listed here.
    pub participants: Vec<UserId>,
    pub member_count: u32,
}

/// One person's private state for one conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationUserState {
    pub conversation_id: ConversationId,
    pub favorite: bool,
    /// Position of the newest message the person has read; `EventSeq::ZERO` means none.
    pub last_read: EventSeq,
    /// Messages from other people after `last_read`.
    pub unread_count: u32,
    /// Unread messages that mention the person ([`Mentions::includes`]): from someone else,
    /// after `last_read`, not deleted.
    pub mention_count: u32,
}

/// Who a message notifies by name.
///
/// The server works this out when the message is committed: the people mentioned, the members of
/// each group mentioned, both limited to people in the conversation and never the author, plus
/// whether `@channel` was used (which means every member, so they are not listed). It is the same
/// for everyone who sees the message, so a client decides "am I mentioned" with
/// [`Mentions::includes`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mentions {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<UserId>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub channel: bool,
}

impl Mentions {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && !self.channel
    }

    /// Whether `user`, who is in the conversation, is notified: named, in a named group, or the
    /// message used `@channel`.
    pub fn includes(&self, user: UserId) -> bool {
        self.channel || self.users.contains(&user)
    }
}

/// A named set of people that a message can mention together (`@design-team`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserGroup {
    pub id: GroupId,
    pub workspace_id: WorkspaceId,
    /// What it is mentioned by. Unique among the workspace's groups and people.
    pub handle: GroupHandle,
    /// What it is called when listed.
    pub name: DisplayName,
    /// The people in it, in a stable order.
    pub members: Vec<UserId>,
}

/// Who reacted to a message with one emoji.
///
/// Reactions are the same for everyone who sees the message, so this lists the people and a
/// client works out "did I react" itself. `users` is in the order people reacted, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionSummary {
    pub emoji: ReactionEmoji,
    /// At least one person: an emoji nobody uses any more is dropped from the list.
    pub users: Vec<UserId>,
}

/// Response of `PUT .../notification-settings`: the settings now in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettingsResponse {
    pub settings: NotificationSettings,
}

/// `PUT /v1/conversations/{id}/notification-prefs`: the caller's own notification preference for
/// one conversation (a member of it, or anyone who can read a public channel). `level: None`
/// follows the workspace's setting; `muted` silences the conversation entirely. Setting a
/// conversation back to the defaults deletes the preference. Idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetConversationNotificationPrefsRequest {
    pub level: Option<NotifyLevel>,
    pub muted: bool,
}

/// Response of `PUT .../notification-prefs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationNotificationPrefsResponse {
    pub prefs: ConversationNotificationPrefs,
}

/// `PUT /v1/workspaces/{id}/status`: sets the caller's status, or clears it with `None`. A status
/// whose `expires_at` is already past is `invalid_request`. Idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetStatusRequest {
    pub status: Option<UserStatus>,
}

/// Response of `PUT .../status`: the caller as other people now see them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    pub user: User,
}

/// Query string of `GET /v1/workspaces/{id}/inbox`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxQuery {
    /// Only threads with unread replies.
    pub unread: Option<bool>,
    /// Return threads whose latest reply is older than this position; omit for the newest.
    pub before: Option<EventSeq>,
    pub limit: Option<u32>,
}

/// One followed thread in the inbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEntry {
    /// The root message, with its thread summary.
    pub root: Message,
    /// The newest reply that has not been deleted.
    pub last_reply: Option<Message>,
    pub conversation: Conversation,
    pub state: ThreadUserState,
}

/// One page of the inbox, most recently active thread first. Threads in conversations the caller
/// can no longer read are left out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxPage {
    pub entries: Vec<InboxEntry>,
    pub has_more: bool,
}

/// Response of `POST .../inbox/read`: how many threads had unread replies and are now read (at
/// most 500 at once; ask again for more). Each announces a `ThreadStateChanged` to the caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxReadResponse {
    pub updated: u32,
}

/// How search results are ordered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchSort {
    /// Best matches first, among the newest candidates (see [`SearchResponse::truncated`]). A
    /// query with only filters is always newest first.
    #[default]
    Relevance,
    /// Newest first.
    Recent,
}

/// Query string of `GET /v1/workspaces/{id}/search`.
///
/// `q` is what the person typed, in the language of [`lazy_domain::search::SearchQuery`]: words
/// (each matching as a prefix), "quoted phrases", `-excluded`, and the filters `from:`, `in:`,
/// `after:`, `before:`, and `has:`. A query that cannot be used is `invalid_request` with the
/// reason. Results are limited to what the caller can read: conversations they are in and public
/// channels (a hit in a channel they have not joined carries the channel, so the client can offer
/// to join). Deleted messages never match. A query that takes too long is `unavailable`, and the
/// person narrows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchParams {
    pub q: String,
    /// Results per page; the server applies a default and a maximum.
    pub limit: Option<u32>,
    /// How many results to skip; the server caps how far a query can page.
    pub offset: Option<u32>,
    pub sort: Option<SearchSort>,
}

/// A stretch of a snippet to highlight, in characters: `start` is inclusive, `end` exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnippetRange {
    pub start: u32,
    pub end: u32,
}

/// A short piece of the matching text with the matched words marked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub text: String,
    pub highlights: Vec<SnippetRange>,
}

/// What matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchHitKind {
    /// The message's text.
    Message,
    /// The name of one of the message's files (`snippet` is the file name).
    Attachment { attachment_id: AttachmentId },
}

/// One search result: the message, where it is, and why it matched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The message as it stands. A reply carries `thread_root`, so a client can open its thread.
    pub message: Message,
    /// The conversation it is in, even if the caller has not joined it.
    pub conversation: Conversation,
    pub kind: SearchHitKind,
    pub snippet: Snippet,
}

/// Response of search: one page, best first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    /// Whether more results can be fetched with a larger `offset`.
    pub has_more: bool,
    /// Whether the query matched more than the server looks at when ranking (it ranks among the
    /// newest few thousand matches), so older matches may be missing. The person can narrow the
    /// query with a filter.
    pub truncated: bool,
}

/// A custom emoji of a workspace.
///
/// The image is served by `GET /v1/emoji/{id}` to members of the workspace (a PNG whose long side
/// is at most [`lazy_domain::emoji::EMOJI_IMAGE_EDGE`] pixels, immutable, so it can be cached
/// forever). A deleted emoji stays in the list with `deleted` set and no image, so that messages
/// and reactions that used it can still show `:name:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub id: EmojiId,
    pub workspace_id: WorkspaceId,
    /// Without the colons. Unique among the workspace's live emoji, and never a standard
    /// shortcode when it was made.
    pub name: String,
    pub created_by: UserId,
    pub created_at: Timestamp,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}

/// Response of creating or renaming a custom emoji.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomEmojiResponse {
    pub emoji: CustomEmoji,
}

/// `PATCH /v1/emoji/{id}`: renames a custom emoji. Admins and owners only. Messages and reactions
/// that used it keep naming it by identity and show the new name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameEmojiRequest {
    pub name: EmojiName,
}

/// A file attached to a message.
///
/// Attachments are immutable once the message is sent. The bytes are served by
/// `GET /v1/attachments/{id}` to anyone who can read the message's conversation; everything else
/// here is what a client needs to show the file before it downloads anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    /// The file name, already made safe ([`lazy_domain::attachment::sanitize_file_name`]).
    pub name: String,
    /// What the server decided the file is ([`lazy_domain::attachment::resolve_media_type`]),
    /// which can differ from what the uploader said.
    pub media_type: String,
    pub size: u64,
    /// Lowercase hex SHA-256 of the bytes, so a download can be checked.
    pub sha256: String,
    /// Pixel size, for images the server decoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Whether `GET /v1/attachments/{id}/thumbnail` has something to return.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_thumbnail: bool,
}

/// Response of an upload. The attachment is *pending*: only its uploader can see it, and it is
/// removed after [`lazy_domain::attachment::PENDING_ATTACHMENT_TTL_HOURS`] unless a message is
/// sent with its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachmentResponse {
    pub attachment: Attachment,
}

/// The replies under a root message.
///
/// A thread changes independently of the root's content, so its summary has its own version,
/// `as_of`: the event-log position of the latest change to the thread (a reply added or deleted).
/// A client keeps, for each root, the summary with the highest `as_of` however it arrives (a
/// history page, a response, an event), exactly as for reactions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSummary {
    /// Replies that have not been deleted; at least 1 for a summary that is present.
    pub reply_count: u32,
    /// The position and time of the newest reply.
    pub last_reply_seq: EventSeq,
    pub last_reply_at: Timestamp,
    /// Up to [`lazy_domain::MAX_THREAD_REPLIERS_LISTED`] different people who replied, most
    /// recent first.
    pub recent_repliers: Vec<UserId>,
    pub as_of: EventSeq,
}

/// One person's private state for one thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadUserState {
    pub root_id: MessageId,
    pub conversation_id: ConversationId,
    /// Whether replies to the thread notify the person. They follow a thread when they reply to
    /// it, when they started it and someone else replies, or when they choose to.
    pub following: bool,
    /// Position of the newest reply the person has read; `EventSeq::ZERO` means none.
    pub last_read: EventSeq,
    /// Replies from other people after `last_read`, not deleted.
    pub unread_count: u32,
    /// Of those, the replies that mention the person.
    pub mention_count: u32,
}

/// A committed message revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub conversation_id: ConversationId,
    pub author_id: UserId,
    pub document: MessageDocument,
    pub created_at: Timestamp,
    /// Starts at 1 and increases by one with each edit, and with deletion. A client keeps the
    /// highest revision it has seen, so a late or repeated event never undoes a newer one.
    pub revision: u32,
    /// Event-log position at which the message was created. Messages in a conversation are
    /// ordered by this value, never by timestamp. An edit does not move a message.
    pub seq: EventSeq,
    /// When the latest edit committed; `None` for a message that was never edited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited_at: Option<Timestamp>,
    /// For a reply, the root of its thread (always a top-level message). `None` for every other
    /// message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root: Option<MessageId>,
    /// For a reply: it was also sent to the channel, so the conversation's history lists it (it is
    /// the same message, with the same identity, not a copy).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_channel: bool,
    /// For a root that has replies, the thread's summary. A deleted root that still has replies
    /// keeps it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<ThreadSummary>,
    /// The files attached to the message, in the order they were sent. A message with attachments
    /// may have an empty document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// Who the message notifies by name; empty for a message that mentions nobody.
    #[serde(default, skip_serializing_if = "Mentions::is_empty")]
    pub mentions: Mentions,
    /// The reactions on the message, one entry per emoji, in the order each emoji was first used.
    /// Empty for a message nobody reacted to, and for a deleted one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<ReactionSummary>,
    /// The event-log position of the change that produced `reactions` (0 before any reaction).
    /// Reactions change independently of the content, so they have their own version: a client
    /// keeps, for each message, the reactions with the highest `reactions_as_of`, whether they
    /// arrive in a history page, a response, or an event (an event's own position is its
    /// envelope `seq`). A late page can then never undo a newer reaction.
    #[serde(default, skip_serializing_if = "lazy_domain::EventSeq::is_zero")]
    pub reactions_as_of: EventSeq,
    /// The message was deleted. Its `document` is then the fixed placeholder
    /// ([`lazy_domain::message::deleted_placeholder`]) and its earlier content is gone from the
    /// server; clients show their own wording and ignore the placeholder text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
}

/// `POST /v1/auth/register`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: Username,
    pub display_name: DisplayName,
    pub password: Password,
    /// An invitation to join the workspace it belongs to. Without one, registration joins the
    /// default workspace if its registration policy is open (otherwise `forbidden`). A code that
    /// is not valid is `invitation_invalid` and creates nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<InviteCode>,
}

/// `POST /v1/auth/login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: Username,
    pub password: Password,
}

/// Response of register and login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResponse {
    pub token: SessionToken,
    pub user: User,
}

/// One workspace's consistent snapshot. `head` is the event-log position the snapshot reflects;
/// subscribe with `after = head` to receive everything newer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceBootstrap {
    pub workspace: Workspace,
    pub role: WorkspaceRole,
    /// Conversations the caller is a member of: the sidebar. Public channels the caller has not
    /// joined are found through browsing instead.
    pub conversations: Vec<Conversation>,
    /// The caller's favorite flag and read position for each of `conversations`.
    pub user_state: Vec<ConversationUserState>,
    pub members: Vec<User>,
    /// The workspace's user groups.
    #[serde(default)]
    pub groups: Vec<UserGroup>,
    /// The workspace's custom emoji, including deleted ones (see [`CustomEmoji`]).
    #[serde(default)]
    pub custom_emoji: Vec<CustomEmoji>,
    /// The caller's notification settings for this workspace (the defaults if they never set any).
    #[serde(default)]
    pub notification_settings: NotificationSettings,
    /// The caller's notification preferences for conversations where they have set one.
    #[serde(default)]
    pub conversation_notification_prefs: Vec<ConversationNotificationPrefs>,
    /// The caller's state for the threads they follow and for those with unread replies (the
    /// newest 500 by activity), including threads in conversations that are not selected.
    #[serde(default)]
    pub threads: Vec<ThreadUserState>,
    /// The messages the caller has saved that they can still read, newest saved first, at most
    /// [`SAVED_BOOTSTRAP_LIMIT`]. A message saved earlier than those is not marked saved in its
    /// conversation until the saved list has been paged to it.
    #[serde(default)]
    pub saved: Vec<MessageId>,
    pub head: EventSeq,
}

/// Response of `GET /v1/bootstrap`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapResponse {
    pub protocol_version: u32,
    pub user: User,
    pub workspaces: Vec<WorkspaceBootstrap>,
}

/// `POST /v1/conversations/{id}/messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub operation_id: OperationId,
    pub document: MessageDocument,
    /// Reply in the thread of this message. It must be a top-level message of this conversation
    /// that has not been deleted (a reply to a reply names the same root); otherwise `not_found`
    /// or `invalid_request`. The author starts following the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_root: Option<MessageId>,
    /// With `thread_root`: also list the reply in the conversation's history.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub also_in_channel: bool,
    /// Files to attach, each uploaded by the sender for this conversation and not yet attached
    /// to a message, at most [`lazy_domain::attachment::MAX_ATTACHMENTS_PER_MESSAGE`]. They are
    /// attached in the same transaction as the message; if any is missing, belongs to someone
    /// else, or was already attached, nothing is sent (`not_found` or `invalid_request`). With
    /// attachments the document may be empty ([`lazy_domain::MessageDocument::empty`]); without
    /// them it may not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<AttachmentId>,
}

/// Acknowledgment of a committed send. Returned only after the transaction commits, and
/// identifies exactly what was committed. Repeating the same request returns the same response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub message: Message,
}

/// `PATCH /v1/messages/{id}`: replaces the message's content.
///
/// `base_revision` is the revision the editor started from. The edit commits only if it is still
/// the current revision; otherwise the server rejects it with `stale_revision` and the client
/// fetches the message (`GET /v1/messages/{id}`) to let the person reconcile, rather than
/// overwriting a change they have not seen. Repeating the same request with the same operation id
/// returns the original result even though the revision has since moved on; the same id with
/// different content is a `conflict`. An edit that changes nothing commits nothing and returns the
/// current message. Only the author edits; a deleted message is `not_found`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditMessageRequest {
    pub operation_id: OperationId,
    pub base_revision: u32,
    pub document: MessageDocument,
}

/// Response of `GET`, `PATCH` and `DELETE` on `/v1/messages/{id}`: the message as it now stands.
/// Deleting a message that is already deleted returns it unchanged, so a lost response is
/// retried with the same request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageResponse {
    pub message: Message,
}

/// An incoming webhook: an address that posts into one conversation as a bot.
///
/// The server keeps only a hash of the secret in the address, so the address can be shown exactly
/// once, when the webhook is created ([`CreatedWebhook`]). Messages posted through it are
/// authored by the webhook's bot user (a member of the conversation, shown with an "APP" badge),
/// go through the ordinary send path, and behave like any other message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    pub id: WebhookId,
    pub conversation_id: ConversationId,
    /// The bot's display name.
    pub name: DisplayName,
    pub bot_user_id: UserId,
    pub created_by: UserId,
    pub created_at: Timestamp,
}

/// `POST /v1/conversations/{id}/webhooks`: creates an incoming webhook and its bot, which joins
/// the conversation. Admins and owners who can read the conversation only (`forbidden`
/// otherwise); an archived conversation is `conflict`; at most
/// [`lazy_domain::MAX_WEBHOOKS_PER_CONVERSATION`] per conversation (`limit_reached`). It carries
/// no operation id: if the answer is lost, list the webhooks and delete the one without an
/// address, because the secret cannot be shown again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWebhookRequest {
    pub name: DisplayName,
}

/// Response of creating a webhook, and of `POST /v1/webhooks/{id}/address` (a new secret, with
/// no body): the only time an address is shown. A new secret makes the old address `not_found` at
/// once; the bot, its channel, and its delivery log stay. Like creating, it carries no operation
/// id: if the answer is lost, ask for a new address again (the one lost is already dead).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedWebhook {
    pub webhook: Webhook,
    /// The full address to post to, including the secret. Never stored by the server in this
    /// form; clients must not log or persist it.
    pub url: String,
}

/// Response of `GET /v1/conversations/{id}/webhooks`, oldest first. Admins and owners only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhooksResponse {
    pub webhooks: Vec<Webhook>,
}

/// What became of one request to a webhook's address, as its delivery log records it.
///
/// Only requests whose address was right are recorded: a wrong address belongs to no webhook. The
/// log holds no content (no text, no `Idempotency-Key`), only what happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookDeliveryOutcome {
    /// A message was posted.
    Posted,
    /// The `Idempotency-Key` had been used before: the earlier message was answered again and
    /// nothing new was posted.
    Duplicate,
    /// The webhook was posting too fast (`rate_limited`). Recorded once per run of refusals, not
    /// once per refused request.
    RateLimited,
    /// The body was over [`lazy_domain::MAX_WEBHOOK_BODY_BYTES`] (`payload_too_large`).
    TooLarge,
    /// The request could not be read: not JSON like `{"text": "..."}`, empty text, text that
    /// cannot be sent, or a malformed `Idempotency-Key` (`invalid_request`).
    Invalid,
    /// The channel is archived (`conflict`).
    Archived,
    /// The server failed while posting (`internal`); the sender may retry.
    Failed,
    /// An outcome this client does not know (sent by a newer server).
    #[serde(other)]
    Unknown,
}

impl WebhookDeliveryOutcome {
    /// Whether a message is in the channel because of this request.
    pub fn posted(self) -> bool {
        matches!(self, Self::Posted | Self::Duplicate)
    }
}

/// One entry of a webhook's delivery log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDelivery {
    pub received_at: Timestamp,
    pub outcome: WebhookDeliveryOutcome,
    /// The message posted (or answered again, for a duplicate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<MessageId>,
    /// The size of the body in bytes, as far as it was read (0 when it was refused before reading).
    pub body_bytes: u32,
    /// Whether the request carried an `Idempotency-Key`.
    pub idempotency_key: bool,
}

/// Response of `GET /v1/webhooks/{id}/deliveries`: the newest first, at most
/// [`lazy_domain::MAX_WEBHOOK_DELIVERIES_KEPT`] (the server keeps no more). Admins and owners who
/// can read the webhook's channel; a webhook that was deleted keeps its log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDeliveriesResponse {
    pub deliveries: Vec<WebhookDelivery>,
}

/// A webhook in the workspace's list of integrations, with how it is doing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWebhook {
    pub webhook: Webhook,
    /// The name of the webhook's channel, which the caller can read but may not have joined.
    pub channel_name: ChannelName,
    /// The most recent entry of its delivery log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery: Option<WebhookDelivery>,
    /// How many of its logged requests in the last 24 hours did not post a message.
    pub recent_failures: u32,
}

/// Response of `GET /v1/workspaces/{id}/webhooks`: every webhook in use in a channel the caller
/// can read, oldest first. Admins and owners only (`forbidden`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceWebhooksResponse {
    pub webhooks: Vec<WorkspaceWebhook>,
}

/// The workspace's GitHub App (D32).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubConnection {
    /// The app's name on GitHub, as in `https://github.com/apps/{slug}`.
    pub app_slug: String,
    /// The app's page on GitHub (install it, change its repositories).
    pub html_url: String,
    /// The organization that owns it.
    pub owner: String,
    /// The bot that posts what GitHub sends.
    pub bot_user_id: UserId,
    pub created_by: UserId,
    pub created_at: Timestamp,
}

/// Where the app is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubInstallation {
    pub account: String,
    /// Every repository of the account, rather than chosen ones.
    pub all_repositories: bool,
    pub suspended: bool,
}

/// One repository's activity followed in one channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSubscription {
    pub id: lazy_domain::GithubSubscriptionId,
    pub conversation_id: ConversationId,
    /// The channel's name (an admin may not have joined it).
    pub channel_name: ChannelName,
    /// `owner/name`, lower case.
    pub repository: String,
    pub events: Vec<lazy_domain::github::GithubEvent>,
    pub created_by: UserId,
    pub created_at: Timestamp,
}

/// Where one delivery from GitHub stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubDeliveryState {
    /// Waiting to be posted (also between retries).
    Queued,
    Running,
    /// Posted to every channel that follows it (or to none, if none does).
    Posted,
    /// Gave up: posting failed and will not be tried again unless asked.
    Failed,
    /// It may or may not have been posted; not tried again unless asked.
    Unknown,
    #[serde(other)]
    Other,
}

/// One event GitHub sent, as Lazy handled it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubDelivery {
    pub id: i64,
    /// GitHub's event name (`pull_request`, `push`, ...).
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub state: GithubDeliveryState,
    pub attempts: u32,
    /// Why the last attempt did not post, in Lazy's words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub received_at: Timestamp,
}

/// Response of `GET /v1/workspaces/{id}/github`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubStatusResponse {
    /// Whether this server can hold GitHub's credentials (it has a credentials key). Without it,
    /// connecting is `unavailable`.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<GithubConnection>,
    pub installations: Vec<GithubInstallation>,
    /// Subscriptions in channels the caller can read, by channel then repository.
    pub subscriptions: Vec<GithubSubscription>,
    /// The newest deliveries first, at most 50.
    pub deliveries: Vec<GithubDelivery>,
}

/// `POST /v1/workspaces/{id}/github/setup`: starts connecting GitHub. The app is created in
/// `organization`, owned by it, and installable only there. Answers with a one-time address to
/// open in a browser, valid for an hour. `conflict` if GitHub is already connected; `unavailable`
/// without a credentials key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartGithubSetupRequest {
    pub organization: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSetupResponse {
    pub url: String,
}

/// `POST /v1/conversations/{id}/github/subscriptions`: the channel follows `repository`
/// (`owner/name`, or a GitHub address) for `events` (at least one). The same repository again
/// changes the events. The app's bot joins the channel. Admins and owners who can read the
/// channel; the repository must belong to an account the app is installed on (`invalid_request`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGithubSubscriptionRequest {
    pub repository: String,
    pub events: Vec<lazy_domain::github::GithubEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubSubscriptionResponse {
    pub subscription: GithubSubscription,
}

/// Body of `POST /hooks/{id}/{secret}`. `text` is composer source: formatting and `:shortcode:`
/// emoji work, mentions do not (a webhook cannot notify people by name).
///
/// Slack's incoming-webhook body is accepted as well (D33): `text` in Slack's `mrkdwn` (its
/// `<url|label>` links and `&amp;` entities included), `blocks`, legacy `attachments`, and
/// `mrkdwn: false`, as JSON or as a form field `payload`. With `blocks`, `text` is only their
/// fallback, as in Slack. Fields that cannot apply (`channel`, `username`, `icon_*`) are ignored.
///
/// An `Idempotency-Key` header (up to 64 printable ASCII characters) makes a retry post the
/// message once. A wrong or revoked address is `not_found`, indistinguishable from one that never
/// existed. Bodies over [`lazy_domain::MAX_WEBHOOK_BODY_BYTES`] are `payload_too_large`; a
/// webhook that posts too fast is `rate_limited`; an archived conversation is `conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingWebhookRequest {
    /// May be empty or absent when Slack's `blocks` or `attachments` carry the message.
    #[serde(default)]
    pub text: String,
}

/// Response of a webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncomingWebhookResponse {
    pub message_id: MessageId,
}

/// `POST /v1/workspaces/{id}/groups`: creates a user group. Admins and owners only.
///
/// Repeating the same request with the same operation id returns the same group; the same id with
/// different content is a `conflict`. A handle that is already a group's or a person's is
/// `name_taken`. At most [`lazy_domain::MAX_GROUPS_PER_WORKSPACE`] groups, each with at most
/// [`lazy_domain::MAX_GROUP_MEMBERS`] people from the workspace (`limit_reached`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateGroupRequest {
    pub operation_id: OperationId,
    pub handle: GroupHandle,
    pub name: DisplayName,
    pub members: Vec<UserId>,
}

/// `PATCH /v1/groups/{id}`. Absent fields are unchanged; `members`, when present, is the complete
/// new membership. Admins and owners only. Renaming the handle does not change messages that
/// mentioned the group: they keep naming it by identity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateGroupRequest {
    pub handle: Option<GroupHandle>,
    pub name: Option<DisplayName>,
    pub members: Option<Vec<UserId>>,
}

/// Response of creating or changing a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupResponse {
    pub group: UserGroup,
}

/// Response of `GET /v1/workspaces/{id}/groups`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupsResponse {
    pub groups: Vec<UserGroup>,
}

/// Response of `GET /v1/conversations/{id}/members`: the people in the conversation, for
/// choosing who to mention or add. Anyone who can read the conversation may ask: members of a
/// private channel or a direct or group conversation, and anyone in the workspace for a public
/// channel. The list is a snapshot; `MemberAdded` and `MemberRemoved` keep it current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMembersResponse {
    pub conversation_id: ConversationId,
    pub user_ids: Vec<UserId>,
}

/// `PUT /v1/messages/{id}/reactions`: sets whether the caller has reacted with `emoji`.
///
/// This sets a state, so it is idempotent and needs no operation id: asking for a reaction that
/// is already there, or to remove one that is not, commits nothing and returns the message as it
/// stands. Anyone who could post in the conversation may react to any message in it
/// (`MessageFacts::can_react`). A message holds at most [`lazy_domain::MAX_REACTION_KINDS`]
/// different emoji; adding a new one beyond that is `limit_reached`. A deleted message is
/// `not_found`. The response is the message with its updated `reactions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetReactionRequest {
    pub emoji: ReactionEmoji,
    pub present: bool,
}

/// `PUT /v1/messages/{root}/thread/follow`: follow or stop following a thread. Sets a state, so it
/// is idempotent and carries no operation id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetThreadFollowRequest {
    pub following: bool,
}

/// `POST /v1/messages/{root}/thread/read`: marks replies up to and including position `up_to` as
/// read. The position only moves forward and is clamped to the thread's newest reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkThreadReadRequest {
    pub up_to: EventSeq,
}

/// Response of the thread follow and read requests: the resulting state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadStateResponse {
    pub state: ThreadUserState,
}

/// Query string of `GET /v1/conversations/{id}/messages`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryQuery {
    /// Return messages strictly older than this position; omit for the newest page.
    pub before: Option<EventSeq>,
    /// Page size; the server applies a default and a maximum.
    pub limit: Option<u32>,
}

/// One page of history, oldest first. Deleted messages are left out (and the page size counts
/// only the messages returned), except a deleted root that still has replies, which is kept as a
/// tombstone. A conversation's history lists top-level messages and replies sent also to the
/// channel; the replies of a thread are paged with `GET /v1/messages/{root}/replies`, which takes
/// the same query and returns the same page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub messages: Vec<Message>,
    /// Whether older messages exist beyond this page.
    pub has_more: bool,
}

/// Response of `GET /v1/operations/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OperationStatus {
    /// A send committed; this is its result.
    Committed { message: Message },
    /// An edit committed (or changed nothing); this is the message as the edit left it.
    EditCommitted { message: Message },
    /// A user group creation committed; this is its result.
    GroupCommitted { group: UserGroup },
    /// A channel or conversation creation committed; this is its result.
    ConversationCommitted { conversation: Conversation },
    /// The server has no record of this operation for the caller. Resend the same request with
    /// the same id; never generate a new id for the same user action.
    NotFound,
}

/// `POST /v1/workspaces/{id}/channels`. The creator becomes a member and the channel's creator.
///
/// Repeating the same request with the same operation id returns the same channel; the same id
/// with different content is a `conflict`. A name already in use is `name_taken`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateChannelRequest {
    pub operation_id: OperationId,
    pub name: ChannelName,
    pub visibility: ChannelVisibility,
    pub topic: Option<String>,
    pub description: Option<String>,
}

/// `POST /v1/workspaces/{id}/conversations`: finds or creates the direct or group conversation
/// for exactly these people plus the caller. The same set always yields the same conversation.
/// Adding or removing a person is a different set, so it opens a new conversation with no
/// history, and the old one stays as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenConversationRequest {
    pub operation_id: OperationId,
    /// The other people. The caller is implied; naming themselves is ignored.
    pub participants: Vec<UserId>,
}

/// Response of the commands that return a conversation: create, open, join, update, archive,
/// unarchive, add members.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationResponse {
    pub conversation: Conversation,
}

/// A public channel in the directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowsedChannel {
    pub conversation: Conversation,
    /// Whether the caller is already a member.
    pub joined: bool,
}

/// How the channel directory is ordered. Ties are broken by name, then id, so paging is stable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelSort {
    /// A to Z.
    #[default]
    Name,
    /// Most members first.
    Members,
}

/// Query string of `GET /v1/workspaces/{id}/channels/browse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseChannelsQuery {
    /// Keep channels whose name, topic, or description contains this text (case-insensitive; at
    /// most [`MAX_DIRECTORY_QUERY_CHARS`] characters, otherwise `invalid_request`).
    pub query: Option<String>,
    /// `Some(true)` keeps only channels the caller has joined, `Some(false)` only those they have
    /// not.
    pub joined: Option<bool>,
    /// Include archived channels (default: no).
    pub include_archived: Option<bool>,
    pub sort: Option<ChannelSort>,
    /// How many matches to skip.
    pub offset: Option<u32>,
    /// At most [`MAX_DIRECTORY_PAGE`]; default [`DEFAULT_DIRECTORY_PAGE`].
    pub limit: Option<u32>,
}

/// Response of `GET /v1/workspaces/{id}/channels/browse`: one page of the public channels, in the
/// order asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowseChannelsResponse {
    pub channels: Vec<BrowsedChannel>,
    /// There are more matches after these.
    #[serde(default)]
    pub has_more: bool,
}

/// The most people or channels one directory page holds, and how many it holds when the request
/// does not say.
pub const MAX_DIRECTORY_PAGE: u32 = 100;
pub const DEFAULT_DIRECTORY_PAGE: u32 = 50;
/// The longest text a directory query may hold, in characters.
pub const MAX_DIRECTORY_QUERY_CHARS: usize = 100;
/// The most saved message ids the bootstrap carries.
pub const SAVED_BOOTSTRAP_LIMIT: usize = 2_000;

/// A person in the workspace's directory: what is public about them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    /// Name, username, and status (an expired one is left out).
    pub user: User,
    pub role: WorkspaceRole,
    pub joined_at: Timestamp,
}

/// Query string of `GET /v1/workspaces/{id}/people`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeopleQuery {
    /// Keep people whose display name or username contains this text (case-insensitive; at most
    /// [`MAX_DIRECTORY_QUERY_CHARS`] characters, otherwise `invalid_request`).
    pub query: Option<String>,
    /// Include people whose membership is deactivated (default: no). Admins and owners only;
    /// anyone else asking for them is `forbidden`.
    pub include_deactivated: Option<bool>,
    pub offset: Option<u32>,
    /// At most [`MAX_DIRECTORY_PAGE`]; default [`DEFAULT_DIRECTORY_PAGE`].
    pub limit: Option<u32>,
}

/// Response of `GET /v1/workspaces/{id}/people`: one page of people (not integration bots), by
/// display name then username then id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeoplePage {
    pub people: Vec<Person>,
    pub has_more: bool,
}

/// Response of `GET /v1/workspaces/{id}/people/{user_id}`. Integration bots have a profile too
/// (their `user.is_bot` is set); a person in another workspace is `not_found`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonResponse {
    pub person: Person,
}

/// `PUT /v1/messages/{id}/saved`: saves the message for the caller, or removes it with `saved:
/// false`. The caller must be able to read the message (otherwise `not_found`); a deleted message
/// cannot be saved but can be removed. Idempotent: saving a saved message keeps its place in the
/// list and announces nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSavedRequest {
    pub saved: bool,
}

/// Response of `PUT .../saved`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedResponse {
    pub message_id: MessageId,
    pub saved: bool,
}

/// Query string of `GET /v1/workspaces/{id}/saved`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedQuery {
    /// Return items saved before this one: the `cursor` of the last item of the previous page;
    /// omit for the most recent.
    pub before: Option<u64>,
    /// At most [`MAX_DIRECTORY_PAGE`]; default [`DEFAULT_DIRECTORY_PAGE`].
    pub limit: Option<u32>,
}

/// One saved message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedItem {
    pub message: Message,
    pub conversation: Conversation,
    pub saved_at: Timestamp,
    /// Where this item sits in the caller's saved list (larger is more recently saved). It orders
    /// and pages the list; it means nothing else.
    pub cursor: u64,
}

/// One page of saved messages, most recently saved first. Messages that were deleted, and
/// messages in conversations the caller can no longer read, are left out (they come back if the
/// caller can read them again).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedPage {
    pub items: Vec<SavedItem>,
    pub has_more: bool,
}

/// `PATCH /v1/workspaces/{id}`: absent fields are unchanged. Admins and owners (otherwise
/// `forbidden`). Idempotent: changing nothing commits and announces nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateWorkspaceRequest {
    pub name: Option<WorkspaceName>,
    pub settings: Option<WorkspaceSettings>,
}

/// Response of `PATCH /v1/workspaces/{id}`: the workspace as it now stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceResponse {
    pub workspace: Workspace,
}

/// `PUT /v1/workspaces/{id}/people/{user_id}/role`. Who may set whose role is
/// [`lazy_domain::workspace::check_role_change`]: `forbidden` when not allowed, `conflict` for
/// "a workspace needs at least one owner". Idempotent: setting the role someone has commits and
/// announces nothing. A deactivated person's role can be set too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetRoleRequest {
    pub role: WorkspaceRole,
}

/// `PUT /v1/workspaces/{id}/people/{user_id}/deactivated`. Who may act on whom is
/// [`lazy_domain::workspace::check_member_action`] (`forbidden`; nobody deactivates themself).
/// Deactivating ends the person's connections to the workspace and removes it from their
/// bootstrap; it keeps their messages, their role, and their conversation memberships (none of
/// which give access while deactivated). Reactivating restores access with all of those.
/// Idempotent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetDeactivatedRequest {
    pub deactivated: bool,
}

/// Where an invitation stands, as the server sees it when listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationState {
    Active,
    Expired,
    Revoked,
    UsedUp,
}

/// An invitation to join a workspace. It never holds the code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invitation {
    pub id: InvitationId,
    pub workspace_id: WorkspaceId,
    pub created_by: UserId,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
    /// How many people may use it; `None` is no limit (other than expiry).
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub state: InvitationState,
}

/// `POST /v1/workspaces/{id}/invitations`. Admins and owners. `expires_in_secs` is between
/// `MIN_INVITE_TTL_SECS` and `MAX_INVITE_TTL_SECS` (default `DEFAULT_INVITE_TTL_SECS`) and
/// `max_uses` between 1 and `MAX_INVITE_USES`, otherwise `invalid_request`; the workspace may
/// have at most `MAX_ACTIVE_INVITATIONS` active ones (`limit_reached`). Not idempotent: an
/// unknown outcome is reconciled by listing, and an invitation whose code was lost is revoked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateInvitationRequest {
    pub expires_in_secs: Option<u64>,
    pub max_uses: Option<u32>,
}

/// Response of `POST .../invitations`: the invitation and its code, which is shown only now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatedInvitation {
    pub invitation: Invitation,
    pub code: InviteCode,
}

/// Response of `GET .../invitations`: every invitation of the workspace that was created in the
/// last 30 days or is still active, newest first. Admins and owners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitationList {
    pub invitations: Vec<Invitation>,
}

/// `PUT /v1/workspaces/{id}`: creates a workspace with an id the client chose, so a repeated
/// request is the same request. The caller becomes its owner, and it starts with the default
/// channel and default settings. Repeating it as the same person with the same name returns the
/// workspace unchanged; the id of a workspace that is anyone else's, or the same person's with
/// another name, is `conflict`. A person may create `MAX_WORKSPACES_CREATED` (`limit_reached`).
/// The server may switch creation off (`forbidden`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateWorkspaceRequest {
    pub name: WorkspaceName,
}

/// Response of `PUT /v1/workspaces/{id}` and `POST /v1/invitations/accept`: the workspace as the
/// caller sees it, to subscribe to after `head`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinedWorkspace {
    pub workspace: WorkspaceBootstrap,
}

/// `POST /v1/invitations/accept`: an existing account joins the workspace the code belongs to, as a
/// member with its default channel, and the invitation's use is counted. Someone who already
/// belongs is returned the workspace without using the invitation again; someone whose membership
/// is deactivated is `forbidden`. A code that is wrong, expired, revoked, or used up is
/// `invitation_invalid`, the same answer in all four cases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptInvitationRequest {
    pub code: InviteCode,
}

/// `PATCH /v1/me`: absent fields are unchanged. Announced to every workspace the person belongs to
/// as `UserProfileChanged`. Idempotent: changing nothing announces nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateProfileRequest {
    pub display_name: Option<DisplayName>,
}

/// Response of `PATCH /v1/me`, `PUT /v1/me/avatar`, and `DELETE /v1/me/avatar`: the caller as
/// others now see them (without a workspace's status or deactivation).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileResponse {
    pub user: User,
}

/// Response of `DELETE /v1/invitations/{id}`: the invitation as it now stands (revoked).
/// Revoking a revoked invitation returns it unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvitationResponse {
    pub invitation: Invitation,
}

/// `PATCH /v1/conversations/{id}`. Absent fields are unchanged; an empty string clears a topic or
/// description. Renaming needs the creator or an admin; the other fields need membership.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateConversationRequest {
    pub name: Option<ChannelName>,
    pub topic: Option<String>,
    pub description: Option<String>,
}

/// `POST /v1/conversations/{id}/members`. People already in the channel are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddMembersRequest {
    pub user_ids: Vec<UserId>,
}

/// `PUT /v1/conversations/{id}/favorite`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetFavoriteRequest {
    pub favorite: bool,
}

/// `POST /v1/conversations/{id}/read`. Marks messages up to and including position `up_to` as
/// read. The read position only moves forward; an older value changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkReadRequest {
    pub up_to: EventSeq,
}

/// Response of favorite and read: the resulting state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserStateResponse {
    pub state: ConversationUserState,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_committed_status_shape() {
        let json = r#"{"status":"conversation_committed","conversation":{"id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","workspace_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d","kind":"channel","name":"design","topic":null,"description":null,"visibility":"public","archived":false,"is_default":false,"created_by":null,"participants":[],"member_count":1}}"#;
        let status: OperationStatus = serde_json::from_str(json).unwrap();
        assert!(matches!(
            status,
            OperationStatus::ConversationCommitted { .. }
        ));
    }

    #[test]
    fn message_routes_match_their_builders() {
        let id = MessageId::generate();
        assert_eq!(
            paths::message(id),
            paths::MESSAGE_ROUTE.replace("{message_id}", &id.to_string())
        );
    }

    fn sample_message() -> Message {
        Message {
            id: MessageId::generate(),
            conversation_id: ConversationId::generate(),
            author_id: UserId::generate(),
            document: MessageDocument::from_plain_text("hello").unwrap(),
            created_at: Timestamp::now(),
            revision: 1,
            seq: EventSeq::new(3).unwrap(),
            edited_at: None,
            deleted: false,
            thread_root: None,
            in_channel: false,
            thread: None,
            attachments: Vec::new(),
            mentions: Mentions::default(),
            reactions: Vec::new(),
            reactions_as_of: EventSeq::ZERO,
        }
    }

    #[test]
    fn attachments_ride_on_the_message_and_sends_name_them() {
        let id = AttachmentId::generate();
        let attachment = Attachment {
            id,
            name: "diagram.png".to_owned(),
            media_type: "image/png".to_owned(),
            size: 1234,
            sha256: "ab".repeat(32),
            width: Some(640),
            height: Some(480),
            has_thumbnail: true,
        };
        let mut message = sample_message();
        assert!(
            serde_json::to_value(&message)
                .unwrap()
                .get("attachments")
                .is_none()
        );
        message.attachments = vec![attachment.clone()];
        message.document = MessageDocument::empty();
        let json = serde_json::to_string(&message).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        // A file with no dimensions leaves them out.
        let plain = Attachment {
            width: None,
            height: None,
            has_thumbnail: false,
            ..attachment
        };
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("width").is_none() && json.get("has_thumbnail").is_none());

        // Sends without attachments keep their old shape; with them, the ids ride along.
        let send = SendMessageRequest {
            operation_id: OperationId::generate(),
            document: MessageDocument::empty(),
            thread_root: None,
            also_in_channel: false,
            attachment_ids: vec![id],
        };
        let json = serde_json::to_string(&send).unwrap();
        assert!(json.contains(&id.to_string()));
        assert_eq!(
            serde_json::from_str::<SendMessageRequest>(&json).unwrap(),
            send
        );
        let bare = SendMessageRequest {
            attachment_ids: vec![],
            document: MessageDocument::from_plain_text("hi").unwrap(),
            ..send
        };
        assert!(
            serde_json::to_value(&bare)
                .unwrap()
                .get("attachment_ids")
                .is_none()
        );
    }

    #[test]
    fn notification_routes_requests_and_the_status_on_a_user() {
        let (workspace, conversation) = (WorkspaceId::generate(), ConversationId::generate());
        let fill = |route: &str| {
            route
                .replace("{workspace_id}", &workspace.to_string())
                .replace("{conversation_id}", &conversation.to_string())
        };
        assert_eq!(
            paths::notification_settings(workspace),
            fill(paths::NOTIFICATION_SETTINGS_ROUTE)
        );
        assert_eq!(
            paths::conversation_notification_prefs(conversation),
            fill(paths::CONVERSATION_NOTIFICATION_ROUTE)
        );
        assert_eq!(paths::status(workspace), fill(paths::STATUS_ROUTE));
        assert_eq!(paths::inbox(workspace), fill(paths::INBOX_ROUTE));
        assert_eq!(paths::inbox_read(workspace), fill(paths::INBOX_READ_ROUTE));
        // A user with no status has no field; one with a status carries it and validates it.
        let mut user = User {
            id: UserId::generate(),
            username: Username::parse("alice").unwrap(),
            display_name: DisplayName::parse("Alice").unwrap(),
            is_bot: false,
            status: None,
            deactivated: false,
            avatar_version: 0,
        };
        assert!(serde_json::to_value(&user).unwrap().get("status").is_none());
        user.status = Some(UserStatus::new("In a meeting", None, None).unwrap());
        let json = serde_json::to_string(&user).unwrap();
        assert_eq!(serde_json::from_str::<User>(&json).unwrap(), user);
        let bad = json.replace("In a meeting", "");
        assert!(serde_json::from_str::<User>(&bad).is_err());
        // Settings default when a bootstrap predates them.
        let settings = NotificationSettings::default();
        let json = serde_json::to_string(&NotificationSettingsResponse { settings }).unwrap();
        assert_eq!(
            serde_json::from_str::<NotificationSettingsResponse>(&json)
                .unwrap()
                .settings,
            settings
        );
        let prefs = SetConversationNotificationPrefsRequest {
            level: Some(NotifyLevel::Nothing),
            muted: true,
        };
        let json = serde_json::to_string(&prefs).unwrap();
        assert_eq!(
            serde_json::from_str::<SetConversationNotificationPrefsRequest>(&json).unwrap(),
            prefs
        );
    }

    #[test]
    fn search_routes_params_and_hits_have_a_stable_shape() {
        let workspace = WorkspaceId::generate();
        assert_eq!(
            paths::search(workspace),
            paths::SEARCH_ROUTE.replace("{workspace_id}", &workspace.to_string())
        );
        assert_eq!(SearchSort::default(), SearchSort::Relevance);
        assert_eq!(
            serde_json::to_value(SearchSort::Recent).unwrap(),
            serde_json::json!("recent")
        );
        let params: SearchParams = serde_json::from_str(r#"{"q":"deploy","limit":10}"#).unwrap();
        assert_eq!(
            (params.q.as_str(), params.limit, params.offset),
            ("deploy", Some(10), None)
        );
        let hit_kind = SearchHitKind::Attachment {
            attachment_id: AttachmentId::generate(),
        };
        let json = serde_json::to_string(&hit_kind).unwrap();
        assert!(json.contains(r#""type":"attachment""#));
        assert_eq!(
            serde_json::from_str::<SearchHitKind>(&json).unwrap(),
            hit_kind
        );
        assert_eq!(
            serde_json::to_value(SearchHitKind::Message).unwrap(),
            serde_json::json!({"type": "message"})
        );
        let snippet = Snippet {
            text: "the deploy failed".to_owned(),
            highlights: vec![SnippetRange { start: 4, end: 10 }],
        };
        let json = serde_json::to_string(&snippet).unwrap();
        assert_eq!(serde_json::from_str::<Snippet>(&json).unwrap(), snippet);
    }

    #[test]
    fn custom_emoji_routes_requests_and_shape() {
        let (workspace, id) = (WorkspaceId::generate(), EmojiId::generate());
        assert_eq!(
            paths::emoji_upload(workspace, id),
            paths::EMOJI_UPLOAD_ROUTE
                .replace("{workspace_id}", &workspace.to_string())
                .replace("{emoji_id}", &id.to_string())
        );
        assert_eq!(
            paths::emoji(id),
            paths::EMOJI_ROUTE.replace("{emoji_id}", &id.to_string())
        );
        let emoji = CustomEmoji {
            id,
            workspace_id: workspace,
            name: "party_parrot".to_owned(),
            created_by: UserId::generate(),
            created_at: Timestamp::now(),
            deleted: false,
        };
        let json = serde_json::to_value(&emoji).unwrap();
        assert!(json.get("deleted").is_none());
        let deleted = CustomEmoji {
            deleted: true,
            ..emoji
        };
        let json = serde_json::to_string(&deleted).unwrap();
        assert!(json.contains(r#""deleted":true"#));
        assert_eq!(serde_json::from_str::<CustomEmoji>(&json).unwrap(), deleted);
        // A rename must be a valid name; a standard shortcode is not.
        assert!(serde_json::from_str::<RenameEmojiRequest>(r#"{"name":"Party_Time"}"#).is_ok());
        for bad in ["a", "tada", "_x", "with space", ""] {
            let body = format!(r#"{{"name":"{bad}"}}"#);
            assert!(
                serde_json::from_str::<RenameEmojiRequest>(&body).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn attachment_routes_match_their_builders() {
        let (conversation, id) = (ConversationId::generate(), AttachmentId::generate());
        let fill = |route: &str| {
            route
                .replace("{conversation_id}", &conversation.to_string())
                .replace("{attachment_id}", &id.to_string())
        };
        assert_eq!(
            paths::attachment_upload(conversation, id),
            fill(paths::ATTACHMENT_UPLOAD_ROUTE)
        );
        assert_eq!(paths::attachment(id), fill(paths::ATTACHMENT_ROUTE));
        assert_eq!(
            paths::attachment_thumbnail(id),
            fill(paths::ATTACHMENT_THUMBNAIL_ROUTE)
        );
    }

    #[test]
    fn thread_fields_ride_on_the_message_and_are_absent_for_ordinary_messages() {
        let mut message = sample_message();
        let json = serde_json::to_value(&message).unwrap();
        for field in ["thread_root", "in_channel", "thread"] {
            assert!(json.get(field).is_none(), "{field}");
        }
        message.thread_root = Some(MessageId::generate());
        message.in_channel = true;
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""in_channel":true"#));
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);

        let mut root = sample_message();
        root.thread = Some(ThreadSummary {
            reply_count: 3,
            last_reply_seq: EventSeq::new(40).unwrap(),
            last_reply_at: Timestamp::now(),
            recent_repliers: vec![UserId::generate(), UserId::generate()],
            as_of: EventSeq::new(40).unwrap(),
        });
        let json = serde_json::to_string(&root).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), root);
    }

    #[test]
    fn replies_are_sent_with_a_root_and_ordinary_sends_are_unchanged() {
        let ordinary = SendMessageRequest {
            operation_id: OperationId::generate(),
            document: MessageDocument::from_plain_text("hi").unwrap(),
            thread_root: None,
            also_in_channel: false,
            attachment_ids: vec![],
        };
        let json = serde_json::to_value(&ordinary).unwrap();
        assert!(json.get("thread_root").is_none() && json.get("also_in_channel").is_none());
        // The shape before threads still parses.
        let old = r#"{"operation_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hi"}]}]}}"#;
        let parsed = serde_json::from_str::<SendMessageRequest>(old).unwrap();
        assert_eq!(parsed.document, ordinary.document);
        assert!(parsed.thread_root.is_none() && !parsed.also_in_channel);
        let reply = SendMessageRequest {
            thread_root: Some(MessageId::generate()),
            also_in_channel: true,
            ..ordinary
        };
        let json = serde_json::to_string(&reply).unwrap();
        assert_eq!(
            serde_json::from_str::<SendMessageRequest>(&json).unwrap(),
            reply
        );
    }

    #[test]
    fn thread_routes_match_their_builders() {
        let id = MessageId::generate();
        let fill = |route: &str| route.replace("{message_id}", &id.to_string());
        assert_eq!(paths::replies(id), fill(paths::REPLIES_ROUTE));
        assert_eq!(
            paths::thread_action(id, "follow"),
            fill(paths::THREAD_FOLLOW_ROUTE)
        );
        assert_eq!(
            paths::thread_action(id, "read"),
            fill(paths::THREAD_READ_ROUTE)
        );
    }

    #[test]
    fn mentions_ride_on_the_message_and_say_who_is_notified() {
        let (a, b) = (UserId::generate(), UserId::generate());
        let mut message = sample_message();
        let json = serde_json::to_value(&message).unwrap();
        assert!(json.get("mentions").is_none());
        message.mentions = Mentions {
            users: vec![a],
            channel: false,
        };
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""mentions":{"users":["#));
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        assert!(message.mentions.includes(a) && !message.mentions.includes(b));
        message.mentions = Mentions {
            users: Vec::new(),
            channel: true,
        };
        assert!(message.mentions.includes(a) && message.mentions.includes(b));
        assert!(Mentions::default().is_empty() && !Mentions::default().includes(a));
    }

    #[test]
    fn webhook_routes_requests_and_the_bot_flag() {
        let (conversation, webhook) = (ConversationId::generate(), WebhookId::generate());
        assert_eq!(
            paths::webhooks(conversation),
            paths::WEBHOOKS_ROUTE.replace("{conversation_id}", &conversation.to_string())
        );
        assert_eq!(
            paths::webhook(webhook),
            paths::WEBHOOK_ROUTE.replace("{webhook_id}", &webhook.to_string())
        );
        assert_eq!(
            paths::hook(webhook, "s3cret"),
            paths::HOOK_ROUTE
                .replace("{webhook_id}", &webhook.to_string())
                .replace("{secret}", "s3cret")
        );
        // A person has no `is_bot` field on the wire; a bot has.
        let person = User {
            id: UserId::generate(),
            username: Username::parse("alice").unwrap(),
            display_name: DisplayName::parse("Alice").unwrap(),
            is_bot: false,
            status: None,
            deactivated: false,
            avatar_version: 0,
        };
        let json = serde_json::to_value(&person).unwrap();
        assert!(json.get("is_bot").is_none());
        let bot = User {
            is_bot: true,
            ..person
        };
        let json = serde_json::to_string(&bot).unwrap();
        assert!(json.contains(r#""is_bot":true"#));
        assert_eq!(serde_json::from_str::<User>(&json).unwrap(), bot);
        assert!(serde_json::from_str::<CreateWebhookRequest>(r#"{"name":""}"#).is_err());
        let workspace = WorkspaceId::generate();
        assert_eq!(
            paths::workspace_webhooks(workspace),
            paths::WORKSPACE_WEBHOOKS_ROUTE.replace("{workspace_id}", &workspace.to_string())
        );
        assert_eq!(
            paths::webhook_deliveries(webhook),
            paths::WEBHOOK_DELIVERIES_ROUTE.replace("{webhook_id}", &webhook.to_string())
        );
        assert_eq!(
            paths::webhook_address(webhook),
            paths::WEBHOOK_ADDRESS_ROUTE.replace("{webhook_id}", &webhook.to_string())
        );
        // Outcomes are snake_case, and one a newer server adds is read as `Unknown`.
        assert_eq!(
            serde_json::to_string(&WebhookDeliveryOutcome::RateLimited).unwrap(),
            r#""rate_limited""#
        );
        assert_eq!(
            serde_json::from_str::<WebhookDeliveryOutcome>(r#""bounced""#).unwrap(),
            WebhookDeliveryOutcome::Unknown
        );
        assert!(WebhookDeliveryOutcome::Duplicate.posted());
        assert!(!WebhookDeliveryOutcome::Archived.posted());
        assert!(
            serde_json::from_str::<IncomingWebhookRequest>(r#"{"text":"deploy done"}"#).is_ok()
        );
    }

    #[test]
    fn group_requests_validate_their_handles_and_routes_match() {
        let ok = r#"{"operation_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","handle":"design-team","name":"Design team","members":[]}"#;
        assert!(serde_json::from_str::<CreateGroupRequest>(ok).is_ok());
        for bad in ["channel", "x", "-x"] {
            assert!(
                serde_json::from_str::<CreateGroupRequest>(&ok.replace("design-team", bad))
                    .is_err(),
                "{bad}"
            );
        }
        let (workspace, group) = (WorkspaceId::generate(), GroupId::generate());
        assert_eq!(
            paths::groups(workspace),
            paths::GROUPS_ROUTE.replace("{workspace_id}", &workspace.to_string())
        );
        assert_eq!(
            paths::group(group),
            paths::GROUP_ROUTE.replace("{group_id}", &group.to_string())
        );
    }

    #[test]
    fn reaction_routes_and_requests() {
        let id = MessageId::generate();
        assert_eq!(
            paths::reactions(id),
            paths::REACTIONS_ROUTE.replace("{message_id}", &id.to_string())
        );
        let request = SetReactionRequest {
            emoji: ReactionEmoji::parse("👍").unwrap(),
            present: true,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"emoji": "👍", "present": true})
        );
        // Anything that is not an emoji is refused while parsing.
        assert!(
            serde_json::from_str::<SetReactionRequest>(r#"{"emoji":"thumbs","present":true}"#)
                .is_err()
        );
    }

    #[test]
    fn reactions_ride_on_the_message_and_old_messages_have_none() {
        let mut message = sample_message();
        let json = serde_json::to_value(&message).unwrap();
        assert!(json.get("reactions").is_none() && json.get("reactions_as_of").is_none());
        let parsed: Message = serde_json::from_value(json).unwrap();
        assert!(parsed.reactions.is_empty() && parsed.reactions_as_of == EventSeq::ZERO);

        message.reactions = vec![ReactionSummary {
            emoji: ReactionEmoji::parse("🎉").unwrap(),
            users: vec![UserId::generate()],
        }];
        message.reactions_as_of = EventSeq::new(9).unwrap();
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""reactions_as_of":9"#));
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
    }

    #[test]
    fn messages_without_edit_fields_still_parse() {
        // The shape before protocol 3, which stored caches and older servers produce.
        let mut json = serde_json::to_value(sample_message()).unwrap();
        let object = json.as_object_mut().unwrap();
        assert!(!object.contains_key("edited_at") && !object.contains_key("deleted"));
        object.remove("edited_at");
        let parsed: Message = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.edited_at, None);
        assert!(!parsed.deleted);
    }

    #[test]
    fn edited_and_deleted_messages_roundtrip() {
        let mut message = sample_message();
        message.revision = 3;
        message.edited_at = Some(Timestamp::now());
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains("edited_at"));
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
        message.deleted = true;
        let json = serde_json::to_string(&message).unwrap();
        assert!(json.contains(r#""deleted":true"#));
        assert_eq!(serde_json::from_str::<Message>(&json).unwrap(), message);
    }

    #[test]
    fn edit_requests_validate_their_document() {
        let ok = r#"{"operation_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","base_revision":1,"document":{"version":2,"blocks":[{"type":"paragraph","inlines":[{"type":"text","text":"hi"}]}]}}"#;
        let request: EditMessageRequest = serde_json::from_str(ok).unwrap();
        assert_eq!(request.base_revision, 1);
        let empty = ok.replace(r#""text":"hi""#, r#""text":"  ""#);
        assert!(serde_json::from_str::<EditMessageRequest>(&empty).is_err());
    }

    #[test]
    fn edit_committed_status_shape() {
        let status = OperationStatus::EditCommitted {
            message: sample_message(),
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["status"], "edit_committed");
        assert_eq!(
            serde_json::from_value::<OperationStatus>(json).unwrap(),
            status
        );
    }

    #[test]
    fn operation_status_shape() {
        assert_eq!(
            serde_json::to_string(&OperationStatus::NotFound).unwrap(),
            r#"{"status":"not_found"}"#
        );
    }

    #[test]
    fn requests_with_invalid_names_fail_to_parse() {
        let json = r#"{"username":"x","display_name":"X","password":"pw"}"#;
        assert!(serde_json::from_str::<RegisterRequest>(json).is_err());
    }

    #[test]
    fn conversation_routes_match_their_builders() {
        use lazy_domain::{ConversationId, UserId, WorkspaceId};
        let (ws, conv, user) = (
            WorkspaceId::generate(),
            ConversationId::generate(),
            UserId::generate(),
        );
        let fill = |route: &str| {
            route
                .replace("{workspace_id}", &ws.to_string())
                .replace("{conversation_id}", &conv.to_string())
                .replace("{user_id}", &user.to_string())
        };
        assert_eq!(paths::channels(ws), fill(paths::CHANNELS_ROUTE));
        assert_eq!(
            paths::browse_channels(ws),
            fill(paths::BROWSE_CHANNELS_ROUTE)
        );
        assert_eq!(
            paths::open_conversation(ws),
            fill(paths::OPEN_CONVERSATION_ROUTE)
        );
        assert_eq!(paths::conversation(conv), fill(paths::CONVERSATION_ROUTE));
        assert_eq!(paths::member(conv, user), fill(paths::MEMBER_ROUTE));
        for (action, route) in [
            ("join", paths::JOIN_ROUTE),
            ("leave", paths::LEAVE_ROUTE),
            ("archive", paths::ARCHIVE_ROUTE),
            ("unarchive", paths::UNARCHIVE_ROUTE),
            ("members", paths::MEMBERS_ROUTE),
            ("favorite", paths::FAVORITE_ROUTE),
            ("read", paths::READ_ROUTE),
        ] {
            assert_eq!(
                paths::conversation_action(conv, action),
                fill(route),
                "{action}"
            );
        }
    }

    #[test]
    fn create_channel_requests_validate_the_name() {
        let ok = r#"{"operation_id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2c","name":"design","visibility":"private","topic":null,"description":null}"#;
        assert!(serde_json::from_str::<CreateChannelRequest>(ok).is_ok());
        assert!(
            serde_json::from_str::<CreateChannelRequest>(&ok.replace("design", "Not A Slug"))
                .is_err()
        );
    }

    #[test]
    fn route_builders_match_patterns() {
        let id = ConversationId::generate();
        assert_eq!(
            paths::messages(id),
            paths::MESSAGES_ROUTE.replace("{conversation_id}", &id.to_string())
        );
    }

    #[test]
    fn discovery_routes_match_their_builders() {
        let workspace = WorkspaceId::generate();
        let user = UserId::generate();
        let message = MessageId::generate();
        assert_eq!(
            paths::people(workspace),
            paths::PEOPLE_ROUTE.replace("{workspace_id}", &workspace.to_string())
        );
        assert_eq!(
            paths::person(workspace, user),
            paths::PERSON_ROUTE
                .replace("{workspace_id}", &workspace.to_string())
                .replace("{user_id}", &user.to_string())
        );
        assert_eq!(
            paths::saved(workspace),
            paths::SAVED_ROUTE.replace("{workspace_id}", &workspace.to_string())
        );
        assert_eq!(
            paths::message_saved(message),
            paths::MESSAGE_SAVED_ROUTE.replace("{message_id}", &message.to_string())
        );
    }

    #[test]
    fn directory_queries_default_to_nothing_and_use_snake_case() {
        let query: BrowseChannelsQuery = serde_json::from_str("{}").unwrap();
        assert_eq!(query, BrowseChannelsQuery::default());
        let query: BrowseChannelsQuery =
            serde_json::from_str(r#"{"sort":"members","joined":false,"limit":10}"#).unwrap();
        assert_eq!(query.sort, Some(ChannelSort::Members));
        assert_eq!(query.joined, Some(false));
        assert_eq!(ChannelSort::default(), ChannelSort::Name);
        let query: SavedQuery = serde_json::from_str(r#"{"before":42}"#).unwrap();
        assert_eq!(query.before, Some(42));
    }

    #[test]
    fn a_browse_response_without_has_more_still_reads() {
        // A server older than this field sent the whole list at once.
        let response: BrowseChannelsResponse = serde_json::from_str(r#"{"channels":[]}"#).unwrap();
        assert!(!response.has_more);
    }

    #[test]
    fn saved_requests_and_pages_roundtrip() {
        let request = SetSavedRequest { saved: true };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"saved":true}"#);
        let response = SavedResponse {
            message_id: MessageId::generate(),
            saved: false,
        };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(
            serde_json::from_str::<SavedResponse>(&json).unwrap(),
            response
        );
        let page = SavedPage {
            items: Vec::new(),
            has_more: true,
        };
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<SavedPage>(&json).unwrap(), page);
    }

    #[test]
    fn people_pages_roundtrip() {
        let person = Person {
            user: User {
                id: UserId::generate(),
                username: Username::parse("alice").unwrap(),
                display_name: DisplayName::parse("Alice").unwrap(),
                is_bot: false,
                status: None,
                deactivated: false,
                avatar_version: 0,
            },
            role: WorkspaceRole::Member,
            joined_at: Timestamp::now(),
        };
        let page = PeoplePage {
            people: vec![person.clone()],
            has_more: false,
        };
        let json = serde_json::to_string(&page).unwrap();
        assert_eq!(serde_json::from_str::<PeoplePage>(&json).unwrap(), page);
        let json = serde_json::to_string(&PersonResponse {
            person: person.clone(),
        })
        .unwrap();
        assert_eq!(
            serde_json::from_str::<PersonResponse>(&json)
                .unwrap()
                .person,
            person
        );
    }

    #[test]
    fn administration_routes_match_their_builders() {
        let workspace = WorkspaceId::generate();
        let user = UserId::generate();
        let invitation = InvitationId::generate();
        let fill = |route: &str| {
            route
                .replace("{workspace_id}", &workspace.to_string())
                .replace("{user_id}", &user.to_string())
                .replace("{invitation_id}", &invitation.to_string())
        };
        assert_eq!(paths::workspace(workspace), fill(paths::WORKSPACE_ROUTE));
        assert_eq!(
            paths::person_role(workspace, user),
            fill(paths::PERSON_ROLE_ROUTE)
        );
        assert_eq!(
            paths::person_deactivated(workspace, user),
            fill(paths::PERSON_DEACTIVATED_ROUTE)
        );
        assert_eq!(
            paths::invitations(workspace),
            fill(paths::INVITATIONS_ROUTE)
        );
        assert_eq!(paths::invitation(invitation), fill(paths::INVITATION_ROUTE));
    }

    #[test]
    fn a_workspace_without_settings_reads_with_the_defaults() {
        // A server older than the settings sent just an id and a name.
        let json = r#"{"id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d","name":"Acme"}"#;
        let workspace: Workspace = serde_json::from_str(json).unwrap();
        assert_eq!(workspace.settings, WorkspaceSettings::default());
    }

    #[test]
    fn a_user_is_active_unless_the_server_says_otherwise() {
        let json = r#"{"id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d","username":"alice","display_name":"Alice"}"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert!(!user.deactivated);
        assert!(
            !serde_json::to_string(&user)
                .unwrap()
                .contains("deactivated")
        );
        let json = serde_json::to_string(&User {
            deactivated: true,
            ..user
        })
        .unwrap();
        assert!(json.contains(r#""deactivated":true"#));
    }

    #[test]
    fn registering_with_and_without_a_code_reads() {
        let plain =
            r#"{"username":"alice","display_name":"Alice","password":"correct horse battery"}"#;
        let request: RegisterRequest = serde_json::from_str(plain).unwrap();
        assert!(request.invite_code.is_none());
        assert!(
            !serde_json::to_string(&request)
                .unwrap()
                .contains("invite_code")
        );
        let with = r#"{"username":"alice","display_name":"Alice","password":"correct horse battery","invite_code":"abcdefghijklmnopqrstuvwxyz"}"#;
        let request: RegisterRequest = serde_json::from_str(with).unwrap();
        assert_eq!(
            request.invite_code.as_ref().map(|c| c.expose()),
            Some("abcdefghijklmnopqrstuvwxyz")
        );
        // The code stays out of logs.
        assert!(!format!("{request:?}").contains("abcdefghijklmnop"));
    }

    #[test]
    fn administration_bodies_roundtrip() {
        let request = UpdateWorkspaceRequest {
            name: Some(WorkspaceName::parse("Acme").unwrap()),
            settings: Some(WorkspaceSettings {
                registration: lazy_domain::workspace::RegistrationPolicy::InviteOnly,
                channel_creation: lazy_domain::workspace::ChannelCreationPolicy::AdminsOnly,
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<UpdateWorkspaceRequest>(&json).unwrap(),
            request
        );
        assert_eq!(
            serde_json::from_str::<UpdateWorkspaceRequest>("{}").unwrap(),
            UpdateWorkspaceRequest::default()
        );
        assert_eq!(
            serde_json::to_string(&SetRoleRequest {
                role: WorkspaceRole::Admin
            })
            .unwrap(),
            r#"{"role":"admin"}"#
        );
        assert_eq!(
            serde_json::to_string(&SetDeactivatedRequest { deactivated: true }).unwrap(),
            r#"{"deactivated":true}"#
        );
        let invitation = Invitation {
            id: InvitationId::generate(),
            workspace_id: WorkspaceId::generate(),
            created_by: UserId::generate(),
            created_at: Timestamp::now(),
            expires_at: Timestamp::now(),
            max_uses: Some(5),
            uses: 1,
            state: InvitationState::UsedUp,
        };
        let json = serde_json::to_string(&InvitationList {
            invitations: vec![invitation.clone()],
        })
        .unwrap();
        assert!(json.contains(r#""state":"used_up""#));
        assert_eq!(
            serde_json::from_str::<InvitationList>(&json)
                .unwrap()
                .invitations,
            std::slice::from_ref(&invitation)
        );
        let created = CreatedInvitation {
            invitation,
            code: InviteCode::new("abcdefghijklmnopqrstuvwxyz"),
        };
        assert!(!format!("{created:?}").contains("abcdefghijklmnop"));
    }

    #[test]
    fn server_info_and_invitation_previews_have_a_stable_shape() {
        let info = ServerInfoResponse {
            protocol_version: 14,
            registration: ServerRegistration::InviteOnly,
            setup_code_required: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":14,"registration":"invite_only"}"#
        );
        assert_eq!(
            serde_json::from_str::<ServerInfoResponse>(&json).unwrap(),
            info
        );
        // A server that says nothing about the setup code is not waiting for one.
        let setup = ServerInfoResponse {
            setup_code_required: true,
            registration: ServerRegistration::Open,
            ..info
        };
        let json = serde_json::to_string(&setup).unwrap();
        assert_eq!(
            json,
            r#"{"protocol_version":14,"registration":"open","setup_code_required":true}"#
        );
        assert_eq!(
            serde_json::from_str::<ServerInfoResponse>(&json).unwrap(),
            setup
        );
        for (registration, text) in [
            (ServerRegistration::Open, "open"),
            (ServerRegistration::InviteOnly, "invite_only"),
            (ServerRegistration::Closed, "closed"),
        ] {
            assert_eq!(
                serde_json::to_string(&registration).unwrap(),
                format!("\"{text}\"")
            );
        }

        let preview = InvitationPreview {
            kind: InvitationKind::Invitation,
            workspace_name: "Zebrafish Collective".into(),
            inviter: Some(DisplayName::parse("Alice Example").unwrap()),
            member_count: 12,
        };
        let json = serde_json::to_string(&preview).unwrap();
        assert_eq!(
            json,
            r#"{"kind":"invitation","workspace_name":"Zebrafish Collective","inviter":"Alice Example","member_count":12}"#
        );
        assert_eq!(
            serde_json::from_str::<InvitationPreview>(&json).unwrap(),
            preview
        );
        let setup = InvitationPreview {
            kind: InvitationKind::Setup,
            workspace_name: "Lazy".into(),
            inviter: None,
            member_count: 0,
        };
        assert_eq!(
            serde_json::to_string(&setup).unwrap(),
            r#"{"kind":"setup","workspace_name":"Lazy","member_count":0}"#
        );
        assert_eq!(paths::SERVER_INFO, "/v1/server");
        assert_eq!(paths::PREVIEW_INVITATION, "/v1/invitations/preview");
    }

    #[test]
    fn profile_and_workspace_creation_routes_and_bodies() {
        let user = UserId::generate();
        assert_eq!(
            paths::user_avatar(user, 3),
            format!(
                "{}?v=3",
                paths::USER_AVATAR_ROUTE.replace("{user_id}", &user.to_string())
            )
        );
        let json = serde_json::to_string(&CreateWorkspaceRequest {
            name: WorkspaceName::parse("Acme").unwrap(),
        })
        .unwrap();
        assert_eq!(json, r#"{"name":"Acme"}"#);
        assert!(serde_json::from_str::<CreateWorkspaceRequest>(r#"{"name":"  "}"#).is_err());
        let accept: AcceptInvitationRequest =
            serde_json::from_str(r#"{"code":"abcdefghijklmnopqrstuvwxyz"}"#).unwrap();
        assert!(!format!("{accept:?}").contains("abcdefghijklmnop"));
        let preview: PreviewInvitationRequest =
            serde_json::from_str(r#"{"code":"abcdefghijklmnopqrstuvwxyz"}"#).unwrap();
        assert!(!format!("{preview:?}").contains("abcdefghijklmnop"));
        assert_eq!(
            serde_json::from_str::<UpdateProfileRequest>("{}").unwrap(),
            UpdateProfileRequest::default()
        );
    }

    #[test]
    fn a_user_without_an_avatar_says_nothing_about_one() {
        let json = r#"{"id":"3f2b8a3e-5d8e-4f6a-9d3a-0c9c3f0a1b2d","username":"alice","display_name":"Alice"}"#;
        let user: User = serde_json::from_str(json).unwrap();
        assert_eq!(user.avatar_version, 0);
        assert!(!serde_json::to_string(&user).unwrap().contains("avatar"));
        let with = User {
            avatar_version: 2,
            ..user
        };
        assert!(
            serde_json::to_string(&with)
                .unwrap()
                .contains(r#""avatar_version":2"#)
        );
    }
}
