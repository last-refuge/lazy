//! `Debug` for `Effect` that cannot leak what the person typed (see `redact`).
// Generated from the enum's variants and then kept by hand: every variant is listed, so a
// new one cannot be added without deciding what may be printed of it.

use std::fmt;

use crate::Effect;
use crate::redact::*;

impl fmt::Debug for Effect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Effect::Register(_) => f
                .debug_tuple("Register")
                .field(&Named("RegisterRequest"))
                .finish(),
            Effect::Login(_) => f
                .debug_tuple("Login")
                .field(&Named("LoginRequest"))
                .finish(),
            Effect::SignOut => f.write_str("SignOut"),
            Effect::ForgetSession => f.write_str("ForgetSession"),
            Effect::LoadBootstrap => f.write_str("LoadBootstrap"),
            Effect::Subscribe { workspace, after } => f
                .debug_struct("Subscribe")
                .field("workspace", workspace)
                .field("after", after)
                .finish(),
            Effect::FetchHistory {
                conversation,
                before,
                limit,
                generation,
            } => f
                .debug_struct("FetchHistory")
                .field("conversation", conversation)
                .field("before", before)
                .field("limit", limit)
                .field("generation", generation)
                .finish(),
            Effect::PersistSubmission(v0) => f.debug_tuple("PersistSubmission").field(v0).finish(),
            Effect::SendMessage {
                operation,
                conversation,
                document: _,
                thread_root,
                also_in_channel,
                attachments,
            } => f
                .debug_struct("SendMessage")
                .field("operation", operation)
                .field("conversation", conversation)
                .field("document", &Named("MessageDocument"))
                .field("thread_root", thread_root)
                .field("also_in_channel", also_in_channel)
                .field("attachments", attachments)
                .finish(),
            Effect::StageFile {
                attachment,
                generation,
                source,
            } => f
                .debug_struct("StageFile")
                .field("attachment", attachment)
                .field("generation", generation)
                .field("source", &Source(source))
                .finish(),
            Effect::UploadFile {
                attachment,
                conversation,
                generation,
                name,
                media_type,
                size,
            } => f
                .debug_struct("UploadFile")
                .field("attachment", attachment)
                .field("conversation", conversation)
                .field("generation", generation)
                .field("name", &Text(name))
                .field("media_type", &Text(media_type))
                .field("size", size)
                .finish(),
            Effect::CancelUpload { attachment } => f
                .debug_struct("CancelUpload")
                .field("attachment", attachment)
                .finish(),
            Effect::ReleaseFiles { attachments } => f
                .debug_struct("ReleaseFiles")
                .field("attachments", attachments)
                .finish(),
            Effect::DownloadAttachment {
                attachment: _,
                destination: _,
            } => f
                .debug_struct("DownloadAttachment")
                .field("attachment", &Named("Attachment"))
                .field("destination", &Named("path"))
                .finish(),
            Effect::CancelDownload { attachment } => f
                .debug_struct("CancelDownload")
                .field("attachment", attachment)
                .finish(),
            Effect::FetchImage { attachment, full } => f
                .debug_struct("FetchImage")
                .field("attachment", attachment)
                .field("full", full)
                .finish(),
            Effect::Search {
                workspace,
                generation,
                params: _,
            } => f
                .debug_struct("Search")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("params", &Named("SearchParams"))
                .finish(),
            Effect::CancelSearch => f.write_str("CancelSearch"),
            Effect::PersistRecentSearches(v0) => f
                .debug_tuple("PersistRecentSearches")
                .field(&Count("texts", v0.len()))
                .finish(),
            Effect::UploadEmoji {
                workspace,
                emoji,
                name: _,
                source,
            } => f
                .debug_struct("UploadEmoji")
                .field("workspace", workspace)
                .field("emoji", emoji)
                .field("name", &Named("EmojiName"))
                .field("source", &Source(source))
                .finish(),
            Effect::RenameEmoji { emoji, request: _ } => f
                .debug_struct("RenameEmoji")
                .field("emoji", emoji)
                .field("request", &Named("RenameEmojiRequest"))
                .finish(),
            Effect::DeleteEmoji { emoji } => {
                f.debug_struct("DeleteEmoji").field("emoji", emoji).finish()
            }
            Effect::FetchEmojiImage { emoji } => f
                .debug_struct("FetchEmojiImage")
                .field("emoji", emoji)
                .finish(),
            Effect::ForgetEmojiImages { emojis } => f
                .debug_struct("ForgetEmojiImages")
                .field("emojis", emojis)
                .finish(),
            Effect::ForgetAttachments { attachments } => f
                .debug_struct("ForgetAttachments")
                .field("attachments", attachments)
                .finish(),
            Effect::FetchReplies {
                root,
                before,
                limit,
                generation,
            } => f
                .debug_struct("FetchReplies")
                .field("root", root)
                .field("before", before)
                .field("limit", limit)
                .field("generation", generation)
                .finish(),
            Effect::SetThreadFollow { root, following } => f
                .debug_struct("SetThreadFollow")
                .field("root", root)
                .field("following", following)
                .finish(),
            Effect::MarkThreadRead { root, up_to } => f
                .debug_struct("MarkThreadRead")
                .field("root", root)
                .field("up_to", up_to)
                .finish(),
            Effect::CreateWebhook {
                conversation,
                request: _,
            } => f
                .debug_struct("CreateWebhook")
                .field("conversation", conversation)
                .field("request", &Named("CreateWebhookRequest"))
                .finish(),
            Effect::DeleteWebhook { webhook } => f
                .debug_struct("DeleteWebhook")
                .field("webhook", webhook)
                .finish(),
            Effect::LoadWebhooks { conversation } => f
                .debug_struct("LoadWebhooks")
                .field("conversation", conversation)
                .finish(),
            Effect::PersistView(v0) => f.debug_tuple("PersistView").field(v0).finish(),
            Effect::PersistThreadDrafts(v0) => f
                .debug_tuple("PersistThreadDrafts")
                .field(&Count("drafts", v0.len()))
                .finish(),
            Effect::EditMessage {
                message,
                request: _,
            } => f
                .debug_struct("EditMessage")
                .field("message", message)
                .field("request", &Named("EditMessageRequest"))
                .finish(),
            Effect::CreateGroup {
                workspace,
                request: _,
            } => f
                .debug_struct("CreateGroup")
                .field("workspace", workspace)
                .field("request", &Named("CreateGroupRequest"))
                .finish(),
            Effect::UpdateGroup { group, request: _ } => f
                .debug_struct("UpdateGroup")
                .field("group", group)
                .field("request", &Named("UpdateGroupRequest"))
                .finish(),
            Effect::DeleteGroup { group } => {
                f.debug_struct("DeleteGroup").field("group", group).finish()
            }
            Effect::FetchMembers {
                conversation,
                generation,
            } => f
                .debug_struct("FetchMembers")
                .field("conversation", conversation)
                .field("generation", generation)
                .finish(),
            Effect::SetReaction {
                message,
                request: _,
            } => f
                .debug_struct("SetReaction")
                .field("message", message)
                .field("request", &Named("SetReactionRequest"))
                .finish(),
            Effect::PersistEmojiPrefs(_) => f
                .debug_tuple("PersistEmojiPrefs")
                .field(&Named("EmojiPrefs"))
                .finish(),
            Effect::FetchMessage { message } => f
                .debug_struct("FetchMessage")
                .field("message", message)
                .finish(),
            Effect::DeleteMessage { message } => f
                .debug_struct("DeleteMessage")
                .field("message", message)
                .finish(),
            Effect::QueryOperationStatus { operation } => f
                .debug_struct("QueryOperationStatus")
                .field("operation", operation)
                .finish(),
            Effect::SettleSubmission { operation } => f
                .debug_struct("SettleSubmission")
                .field("operation", operation)
                .finish(),
            Effect::PersistDrafts(v0) => f
                .debug_tuple("PersistDrafts")
                .field(&Count("drafts", v0.len()))
                .finish(),
            Effect::PersistCache(v0) => f.debug_tuple("PersistCache").field(v0).finish(),
            Effect::CreateChannel {
                workspace,
                request: _,
            } => f
                .debug_struct("CreateChannel")
                .field("workspace", workspace)
                .field("request", &Named("CreateChannelRequest"))
                .finish(),
            Effect::OpenConversation {
                workspace,
                request: _,
            } => f
                .debug_struct("OpenConversation")
                .field("workspace", workspace)
                .field("request", &Named("OpenConversationRequest"))
                .finish(),
            Effect::JoinChannel { conversation } => f
                .debug_struct("JoinChannel")
                .field("conversation", conversation)
                .finish(),
            Effect::LeaveChannel { conversation } => f
                .debug_struct("LeaveChannel")
                .field("conversation", conversation)
                .finish(),
            Effect::AddMembers {
                conversation,
                request: _,
            } => f
                .debug_struct("AddMembers")
                .field("conversation", conversation)
                .field("request", &Named("AddMembersRequest"))
                .finish(),
            Effect::RemoveMember { conversation, user } => f
                .debug_struct("RemoveMember")
                .field("conversation", conversation)
                .field("user", user)
                .finish(),
            Effect::UpdateConversation {
                conversation,
                request: _,
            } => f
                .debug_struct("UpdateConversation")
                .field("conversation", conversation)
                .field("request", &Named("UpdateConversationRequest"))
                .finish(),
            Effect::ArchiveConversation { conversation } => f
                .debug_struct("ArchiveConversation")
                .field("conversation", conversation)
                .finish(),
            Effect::UnarchiveConversation { conversation } => f
                .debug_struct("UnarchiveConversation")
                .field("conversation", conversation)
                .finish(),
            Effect::SetFavorite {
                conversation,
                favorite,
            } => f
                .debug_struct("SetFavorite")
                .field("conversation", conversation)
                .field("favorite", favorite)
                .finish(),
            Effect::MarkRead {
                conversation,
                up_to,
            } => f
                .debug_struct("MarkRead")
                .field("conversation", conversation)
                .field("up_to", up_to)
                .finish(),
            Effect::DiscardCache => f.write_str("DiscardCache"),
            Effect::SetNotificationSettings {
                workspace,
                settings,
            } => f
                .debug_struct("SetNotificationSettings")
                .field("workspace", workspace)
                .field("settings", settings)
                .finish(),
            Effect::SetConversationNotificationPrefs {
                conversation,
                request: _,
            } => f
                .debug_struct("SetConversationNotificationPrefs")
                .field("conversation", conversation)
                .field("request", &Named("SetConversationNotificationPrefsRequest"))
                .finish(),
            Effect::SetStatus {
                workspace,
                request: _,
            } => f
                .debug_struct("SetStatus")
                .field("workspace", workspace)
                .field("request", &Named("SetStatusRequest"))
                .finish(),
            Effect::LoadInbox {
                workspace,
                generation,
                query: _,
            } => f
                .debug_struct("LoadInbox")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("query", &Named("InboxQuery"))
                .finish(),
            Effect::CancelInbox => f.write_str("CancelInbox"),
            Effect::MarkInboxRead { workspace } => f
                .debug_struct("MarkInboxRead")
                .field("workspace", workspace)
                .finish(),
            Effect::SetMessageSaved {
                message,
                request: _,
            } => f
                .debug_struct("SetMessageSaved")
                .field("message", message)
                .field("request", &Named("SetSavedRequest"))
                .finish(),
            Effect::LoadSaved {
                workspace,
                generation,
                query: _,
            } => f
                .debug_struct("LoadSaved")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("query", &Named("SavedQuery"))
                .finish(),
            Effect::CancelSaved => f.write_str("CancelSaved"),
            Effect::LoadPeople {
                workspace,
                generation,
                query: _,
            } => f
                .debug_struct("LoadPeople")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("query", &Named("PeopleQuery"))
                .finish(),
            Effect::CancelPeople => f.write_str("CancelPeople"),
            Effect::LoadProfile {
                workspace,
                generation,
                user,
            } => f
                .debug_struct("LoadProfile")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("user", user)
                .finish(),
            Effect::CancelProfile => f.write_str("CancelProfile"),
            Effect::LoadBrowse {
                workspace,
                generation,
                query: _,
            } => f
                .debug_struct("LoadBrowse")
                .field("workspace", workspace)
                .field("generation", generation)
                .field("query", &Named("BrowseChannelsQuery"))
                .finish(),
            Effect::CancelBrowse => f.write_str("CancelBrowse"),
            Effect::CreateWorkspace {
                workspace,
                request: _,
            } => f
                .debug_struct("CreateWorkspace")
                .field("workspace", workspace)
                .field("request", &Named("CreateWorkspaceRequest"))
                .finish(),
            Effect::AcceptInvitation { request: _ } => f
                .debug_struct("AcceptInvitation")
                .field("request", &Named("AcceptInvitationRequest"))
                .finish(),
            Effect::UpdateProfile { request: _ } => f
                .debug_struct("UpdateProfile")
                .field("request", &Named("UpdateProfileRequest"))
                .finish(),
            Effect::SetAvatar { source } => f
                .debug_struct("SetAvatar")
                .field("source", &Source(source))
                .finish(),
            Effect::RemoveAvatar => f.write_str("RemoveAvatar"),
            Effect::FetchAvatar { user, version } => f
                .debug_struct("FetchAvatar")
                .field("user", user)
                .field("version", version)
                .finish(),
            Effect::ForgetAvatars { users } => f
                .debug_struct("ForgetAvatars")
                .field("users", users)
                .finish(),
            Effect::PersistActiveWorkspace(v0) => {
                f.debug_tuple("PersistActiveWorkspace").field(v0).finish()
            }
            Effect::PersistAppearance(v0) => f.debug_tuple("PersistAppearance").field(v0).finish(),
            Effect::PersistRecentConversations(v0) => f
                .debug_tuple("PersistRecentConversations")
                .field(v0)
                .finish(),
            Effect::UpdateWorkspace {
                workspace,
                request: _,
            } => f
                .debug_struct("UpdateWorkspace")
                .field("workspace", workspace)
                .field("request", &Named("UpdateWorkspaceRequest"))
                .finish(),
            Effect::SetMemberRole {
                workspace,
                user,
                request: _,
            } => f
                .debug_struct("SetMemberRole")
                .field("workspace", workspace)
                .field("user", user)
                .field("request", &Named("SetRoleRequest"))
                .finish(),
            Effect::SetMemberDeactivated {
                workspace,
                user,
                request: _,
            } => f
                .debug_struct("SetMemberDeactivated")
                .field("workspace", workspace)
                .field("user", user)
                .field("request", &Named("SetDeactivatedRequest"))
                .finish(),
            Effect::LoadInvitations {
                workspace,
                generation,
            } => f
                .debug_struct("LoadInvitations")
                .field("workspace", workspace)
                .field("generation", generation)
                .finish(),
            Effect::CancelInvitations => f.write_str("CancelInvitations"),
            Effect::CreateInvitation {
                workspace,
                request: _,
            } => f
                .debug_struct("CreateInvitation")
                .field("workspace", workspace)
                .field("request", &Named("CreateInvitationRequest"))
                .finish(),
            Effect::RevokeInvitation { invitation } => f
                .debug_struct("RevokeInvitation")
                .field("invitation", invitation)
                .finish(),
            Effect::Unsubscribe { workspace } => f
                .debug_struct("Unsubscribe")
                .field("workspace", workspace)
                .finish(),
            Effect::Notify(_) => f
                .debug_tuple("Notify")
                .field(&Named("DesktopNotification"))
                .finish(),
            Effect::ClearNotifications { conversation } => f
                .debug_struct("ClearNotifications")
                .field("conversation", conversation)
                .finish(),
            Effect::RetractNotification { message } => f
                .debug_struct("RetractNotification")
                .field("message", message)
                .finish(),
            Effect::SetBadge(v0) => f.debug_tuple("SetBadge").field(v0).finish(),
        }
    }
}
