//! `Debug` for `Input` that cannot leak what the person typed (see `redact`).
// Generated from the enum's variants and then kept by hand: every variant is listed, so a
// new one cannot be added without deciding what may be printed of it.

use std::fmt;

use crate::Input;
use crate::redact::*;

impl fmt::Debug for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Input::Register {
                username,
                display_name,
                password,
                invite_code,
            } => f
                .debug_struct("Register")
                .field("username", &Text(username))
                .field("display_name", &Text(display_name))
                .field("password", &Text(password))
                .field("invite_code", &Text(invite_code))
                .finish(),
            Input::SignIn { username, password } => f
                .debug_struct("SignIn")
                .field("username", &Text(username))
                .field("password", &Text(password))
                .finish(),
            Input::SignOut => f.write_str("SignOut"),
            Input::RetryLoad => f.write_str("RetryLoad"),
            Input::SelectConversation(v0) => f.debug_tuple("SelectConversation").field(v0).finish(),
            Input::SetDraft { conversation, text } => f
                .debug_struct("SetDraft")
                .field("conversation", conversation)
                .field("text", &Text(text))
                .finish(),
            Input::SendMessage { conversation, text } => f
                .debug_struct("SendMessage")
                .field("conversation", conversation)
                .field("text", &Text(text))
                .finish(),
            Input::RetryOperation {
                conversation,
                operation,
            } => f
                .debug_struct("RetryOperation")
                .field("conversation", conversation)
                .field("operation", operation)
                .finish(),
            Input::LoadOlder { conversation } => f
                .debug_struct("LoadOlder")
                .field("conversation", conversation)
                .finish(),
            Input::DismissNotice => f.write_str("DismissNotice"),
            Input::EditMessage {
                conversation,
                message,
                text,
            } => f
                .debug_struct("EditMessage")
                .field("conversation", conversation)
                .field("message", message)
                .field("text", &Text(text))
                .finish(),
            Input::CancelEdit {
                conversation,
                message,
            } => f
                .debug_struct("CancelEdit")
                .field("conversation", conversation)
                .field("message", message)
                .finish(),
            Input::RetryEdit {
                conversation,
                message,
            } => f
                .debug_struct("RetryEdit")
                .field("conversation", conversation)
                .field("message", message)
                .finish(),
            Input::DeleteMessage {
                conversation,
                message,
            } => f
                .debug_struct("DeleteMessage")
                .field("conversation", conversation)
                .field("message", message)
                .finish(),
            Input::OpenPermalink(v0) => f.debug_tuple("OpenPermalink").field(v0).finish(),
            Input::ClearJumpTarget => f.write_str("ClearJumpTarget"),
            Input::OpenThread { conversation, root } => f
                .debug_struct("OpenThread")
                .field("conversation", conversation)
                .field("root", root)
                .finish(),
            Input::CloseThread => f.write_str("CloseThread"),
            Input::LoadOlderReplies { root } => f
                .debug_struct("LoadOlderReplies")
                .field("root", root)
                .finish(),
            Input::SendReply {
                conversation,
                root,
                text,
                also_in_channel,
            } => f
                .debug_struct("SendReply")
                .field("conversation", conversation)
                .field("root", root)
                .field("text", &Text(text))
                .field("also_in_channel", also_in_channel)
                .finish(),
            Input::SetThreadDraft { root, text } => f
                .debug_struct("SetThreadDraft")
                .field("root", root)
                .field("text", &Text(text))
                .finish(),
            Input::SetThreadFollow { root, following } => f
                .debug_struct("SetThreadFollow")
                .field("root", root)
                .field("following", following)
                .finish(),
            Input::ThreadAtBottom { root, at_bottom } => f
                .debug_struct("ThreadAtBottom")
                .field("root", root)
                .field("at_bottom", at_bottom)
                .finish(),
            Input::AttachFiles { target, files } => f
                .debug_struct("AttachFiles")
                .field("target", target)
                .field("files", &Count("files", files.len()))
                .finish(),
            Input::RemoveAttachment { target, attachment } => f
                .debug_struct("RemoveAttachment")
                .field("target", target)
                .field("attachment", attachment)
                .finish(),
            Input::CancelAttachment { target, attachment } => f
                .debug_struct("CancelAttachment")
                .field("target", target)
                .field("attachment", attachment)
                .finish(),
            Input::RetryAttachment { target, attachment } => f
                .debug_struct("RetryAttachment")
                .field("target", target)
                .field("attachment", attachment)
                .finish(),
            Input::DiscardOperation {
                conversation,
                operation,
            } => f
                .debug_struct("DiscardOperation")
                .field("conversation", conversation)
                .field("operation", operation)
                .finish(),
            Input::DownloadAttachment {
                attachment: _,
                destination: _,
            } => f
                .debug_struct("DownloadAttachment")
                .field("attachment", &Named("Attachment"))
                .field("destination", &Named("path"))
                .finish(),
            Input::CancelDownload(v0) => f.debug_tuple("CancelDownload").field(v0).finish(),
            Input::DismissDownload(v0) => f.debug_tuple("DismissDownload").field(v0).finish(),
            Input::LoadThumbnail(v0) => f.debug_tuple("LoadThumbnail").field(v0).finish(),
            Input::LoadFullImage(v0) => f.debug_tuple("LoadFullImage").field(v0).finish(),
            Input::ReleaseFullImage(v0) => f.debug_tuple("ReleaseFullImage").field(v0).finish(),
            Input::OpenSearch => f.write_str("OpenSearch"),
            Input::SetSearchQuery { text } => f
                .debug_struct("SetSearchQuery")
                .field("text", &Text(text))
                .finish(),
            Input::RunSearch => f.write_str("RunSearch"),
            Input::LoadMoreSearch => f.write_str("LoadMoreSearch"),
            Input::SetSearchSort(v0) => f.debug_tuple("SetSearchSort").field(v0).finish(),
            Input::CloseSearch => f.write_str("CloseSearch"),
            Input::OpenSearchHit(v0) => f.debug_tuple("OpenSearchHit").field(v0).finish(),
            Input::AddCustomEmoji { name, file: _ } => f
                .debug_struct("AddCustomEmoji")
                .field("name", &Text(name))
                .field("file", &Named("LocalFile"))
                .finish(),
            Input::RenameCustomEmoji { id, name } => f
                .debug_struct("RenameCustomEmoji")
                .field("id", id)
                .field("name", &Text(name))
                .finish(),
            Input::DeleteCustomEmoji(v0) => f.debug_tuple("DeleteCustomEmoji").field(v0).finish(),
            Input::LoadEmojiImage(v0) => f.debug_tuple("LoadEmojiImage").field(v0).finish(),
            Input::CreateWebhook { conversation, name } => f
                .debug_struct("CreateWebhook")
                .field("conversation", conversation)
                .field("name", &Text(name))
                .finish(),
            Input::DeleteWebhook(v0) => f.debug_tuple("DeleteWebhook").field(v0).finish(),
            Input::LoadWebhooks(v0) => f.debug_tuple("LoadWebhooks").field(v0).finish(),
            Input::LoadWorkspaceWebhooks(v0) => {
                f.debug_tuple("LoadWorkspaceWebhooks").field(v0).finish()
            }
            Input::LoadWebhookDeliveries(v0) => {
                f.debug_tuple("LoadWebhookDeliveries").field(v0).finish()
            }
            Input::NewWebhookAddress(v0) => f.debug_tuple("NewWebhookAddress").field(v0).finish(),
            Input::LoadGithub(v0) => f.debug_tuple("LoadGithub").field(v0).finish(),
            Input::StartGithubSetup { workspace, .. } => f
                .debug_struct("StartGithubSetup")
                .field("workspace", workspace)
                .finish_non_exhaustive(),
            Input::DismissGithubSetup => f.write_str("DismissGithubSetup"),
            Input::GithubSubscribe {
                conversation,
                events,
                ..
            } => f
                .debug_struct("GithubSubscribe")
                .field("conversation", conversation)
                .field("events", events)
                .finish_non_exhaustive(),
            Input::GithubUnsubscribe(v0) => f.debug_tuple("GithubUnsubscribe").field(v0).finish(),
            Input::RetryGithubDelivery {
                workspace,
                delivery,
            } => f
                .debug_struct("RetryGithubDelivery")
                .field("workspace", workspace)
                .field("delivery", delivery)
                .finish(),
            Input::DisconnectGithub(v0) => f.debug_tuple("DisconnectGithub").field(v0).finish(),
            Input::LoadConnector {
                workspace,
                provider,
            } => f
                .debug_struct("LoadConnector")
                .field("workspace", workspace)
                .field("provider", provider)
                .finish(),
            Input::ConnectConnector {
                workspace,
                provider,
            } => f
                .debug_struct("ConnectConnector")
                .field("workspace", workspace)
                .field("provider", provider)
                .finish(),
            Input::SetConnectorSecret {
                workspace,
                provider,
                ..
            } => f
                .debug_struct("SetConnectorSecret")
                .field("workspace", workspace)
                .field("provider", provider)
                .finish_non_exhaustive(),
            Input::GenerateConnectorSecret {
                workspace,
                provider,
            } => f
                .debug_struct("GenerateConnectorSecret")
                .field("workspace", workspace)
                .field("provider", provider)
                .finish(),
            Input::DismissConnectorSecret => f.write_str("DismissConnectorSecret"),
            Input::ConnectorSubscribe {
                conversation,
                provider,
                events,
                ..
            } => f
                .debug_struct("ConnectorSubscribe")
                .field("conversation", conversation)
                .field("provider", provider)
                .field("events", events)
                .finish_non_exhaustive(),
            Input::ConnectorUnsubscribe(v0) => {
                f.debug_tuple("ConnectorUnsubscribe").field(v0).finish()
            }
            Input::RetryConnectorDelivery {
                workspace,
                delivery,
            } => f
                .debug_struct("RetryConnectorDelivery")
                .field("workspace", workspace)
                .field("delivery", delivery)
                .finish(),
            Input::DisconnectConnector {
                workspace,
                provider,
            } => f
                .debug_struct("DisconnectConnector")
                .field("workspace", workspace)
                .field("provider", provider)
                .finish(),
            Input::DismissCreatedWebhook => f.write_str("DismissCreatedWebhook"),
            Input::SetDraftSelection {
                conversation,
                anchor,
                head,
            } => f
                .debug_struct("SetDraftSelection")
                .field("conversation", conversation)
                .field("anchor", anchor)
                .field("head", head)
                .finish(),
            Input::SetThreadDraftSelection { root, anchor, head } => f
                .debug_struct("SetThreadDraftSelection")
                .field("root", root)
                .field("anchor", anchor)
                .field("head", head)
                .finish(),
            Input::CreateGroup {
                handle,
                name,
                members,
            } => f
                .debug_struct("CreateGroup")
                .field("handle", &Text(handle))
                .field("name", &Text(name))
                .field("members", members)
                .finish(),
            Input::UpdateGroup {
                id,
                handle,
                name,
                members,
            } => f
                .debug_struct("UpdateGroup")
                .field("id", id)
                .field("handle", &OptText(handle))
                .field("name", &OptText(name))
                .field("members", members)
                .finish(),
            Input::DeleteGroup(v0) => f.debug_tuple("DeleteGroup").field(v0).finish(),
            Input::ToggleReaction {
                conversation,
                message,
                emoji,
            } => f
                .debug_struct("ToggleReaction")
                .field("conversation", conversation)
                .field("message", message)
                .field("emoji", emoji)
                .finish(),
            Input::RecordEmojiUse(v0) => f.debug_tuple("RecordEmojiUse").field(v0).finish(),
            Input::SetSkinTone(v0) => f.debug_tuple("SetSkinTone").field(v0).finish(),
            Input::CreateChannel {
                name,
                visibility,
                topic,
                description,
            } => f
                .debug_struct("CreateChannel")
                .field("name", &Text(name))
                .field("visibility", visibility)
                .field("topic", &Text(topic))
                .field("description", &Text(description))
                .finish(),
            Input::OpenConversation { participants } => f
                .debug_struct("OpenConversation")
                .field("participants", participants)
                .finish(),
            Input::BrowseChannels => f.write_str("BrowseChannels"),
            Input::JoinChannel(v0) => f.debug_tuple("JoinChannel").field(v0).finish(),
            Input::LeaveChannel(v0) => f.debug_tuple("LeaveChannel").field(v0).finish(),
            Input::AddMembers { id, users } => f
                .debug_struct("AddMembers")
                .field("id", id)
                .field("users", users)
                .finish(),
            Input::RemoveMember { id, user } => f
                .debug_struct("RemoveMember")
                .field("id", id)
                .field("user", user)
                .finish(),
            Input::UpdateConversation {
                id,
                name,
                topic,
                description,
            } => f
                .debug_struct("UpdateConversation")
                .field("id", id)
                .field("name", &OptText(name))
                .field("topic", &OptText(topic))
                .field("description", &OptText(description))
                .finish(),
            Input::ArchiveConversation(v0) => {
                f.debug_tuple("ArchiveConversation").field(v0).finish()
            }
            Input::UnarchiveConversation(v0) => {
                f.debug_tuple("UnarchiveConversation").field(v0).finish()
            }
            Input::SetFavorite { id, favorite } => f
                .debug_struct("SetFavorite")
                .field("id", id)
                .field("favorite", favorite)
                .finish(),
            Input::DismissAction(v0) => f.debug_tuple("DismissAction").field(v0).finish(),
            Input::WindowActive(v0) => f.debug_tuple("WindowActive").field(v0).finish(),
            Input::TimelineAtBottom {
                conversation,
                at_bottom,
            } => f
                .debug_struct("TimelineAtBottom")
                .field("conversation", conversation)
                .field("at_bottom", at_bottom)
                .finish(),
            Input::Started(v0) => f.debug_tuple("Started").field(v0).finish(),
            Input::Authenticated { user: _ } => f
                .debug_struct("Authenticated")
                .field("user", &Named("User"))
                .finish(),
            Input::AuthFailed(v0) => f.debug_tuple("AuthFailed").field(&Req(v0)).finish(),
            Input::SessionExpired => f.write_str("SessionExpired"),
            Input::BootstrapLoaded(v0) => f
                .debug_tuple("BootstrapLoaded")
                .field(&Count("workspaces", v0.workspaces.len()))
                .finish(),
            Input::BootstrapFailed(v0) => f.debug_tuple("BootstrapFailed").field(&Req(v0)).finish(),
            Input::HistoryLoaded {
                conversation,
                generation,
                result,
            } => f
                .debug_struct("HistoryLoaded")
                .field("conversation", conversation)
                .field("generation", generation)
                .field("result", &ResReq(result, "HistoryPage"))
                .finish(),
            Input::SubmissionPersisted { operation } => f
                .debug_struct("SubmissionPersisted")
                .field("operation", operation)
                .finish(),
            Input::SubmissionPersistFailed { operation, message } => f
                .debug_struct("SubmissionPersistFailed")
                .field("operation", operation)
                .field("message", &Text(message))
                .finish(),
            Input::SendResult { operation, result } => f
                .debug_struct("SendResult")
                .field("operation", operation)
                .field("result", &ResReq(result, "SendMessageResponse"))
                .finish(),
            Input::EditResult { operation, result } => f
                .debug_struct("EditResult")
                .field("operation", operation)
                .field("result", &ResReq(result, "MessageResponse"))
                .finish(),
            Input::GroupCommandResult { operation, result } => f
                .debug_struct("GroupCommandResult")
                .field("operation", operation)
                .field("result", &ResReq(result, "GroupResponse"))
                .finish(),
            Input::MembersFetched {
                conversation,
                generation,
                result,
            } => f
                .debug_struct("MembersFetched")
                .field("conversation", conversation)
                .field("generation", generation)
                .field("result", &ResReq(result, "ConversationMembersResponse"))
                .finish(),
            Input::RepliesLoaded {
                root,
                generation,
                result,
            } => f
                .debug_struct("RepliesLoaded")
                .field("root", root)
                .field("generation", generation)
                .field("result", &ResReq(result, "HistoryPage"))
                .finish(),
            Input::ThreadFollowResult { root, result } => f
                .debug_struct("ThreadFollowResult")
                .field("root", root)
                .field("result", &ResReq(result, "ThreadStateResponse"))
                .finish(),
            Input::ThreadReadResult {
                root,
                up_to,
                result,
            } => f
                .debug_struct("ThreadReadResult")
                .field("root", root)
                .field("up_to", up_to)
                .field("result", &ResReq(result, "ThreadStateResponse"))
                .finish(),
            Input::ReactionResult {
                message,
                emoji,
                result,
            } => f
                .debug_struct("ReactionResult")
                .field("message", message)
                .field("emoji", emoji)
                .field("result", &ResReq(result, "MessageResponse"))
                .finish(),
            Input::MessageFetched { message, result } => f
                .debug_struct("MessageFetched")
                .field("message", message)
                .field("result", &ResReq(result, "MessageResponse"))
                .finish(),
            Input::FileStaged {
                attachment,
                generation,
                result,
            } => f
                .debug_struct("FileStaged")
                .field("attachment", attachment)
                .field("generation", generation)
                .field("result", &ResXfer(result, "StagedFile"))
                .finish(),
            Input::UploadProgress {
                attachment,
                generation,
                sent,
                total,
            } => f
                .debug_struct("UploadProgress")
                .field("attachment", attachment)
                .field("generation", generation)
                .field("sent", sent)
                .field("total", total)
                .finish(),
            Input::UploadResult {
                attachment,
                generation,
                result,
            } => f
                .debug_struct("UploadResult")
                .field("attachment", attachment)
                .field("generation", generation)
                .field("result", &ResXfer(result, "AttachmentResponse"))
                .finish(),
            Input::DownloadProgress {
                attachment,
                received,
                total,
            } => f
                .debug_struct("DownloadProgress")
                .field("attachment", attachment)
                .field("received", received)
                .field("total", total)
                .finish(),
            Input::DownloadResult { attachment, result } => f
                .debug_struct("DownloadResult")
                .field("attachment", attachment)
                .field("result", &ResXfer(result, "PathBuf"))
                .finish(),
            Input::SearchResult { generation, result } => f
                .debug_struct("SearchResult")
                .field("generation", generation)
                .field("result", &ResReq(result, "SearchResponse"))
                .finish(),
            Input::CustomEmojiUploaded { emoji, result } => f
                .debug_struct("CustomEmojiUploaded")
                .field("emoji", emoji)
                .field("result", &ResXfer(result, "CustomEmojiResponse"))
                .finish(),
            Input::EmojiImageLoaded { emoji, result } => f
                .debug_struct("EmojiImageLoaded")
                .field("emoji", emoji)
                .field("result", &ResXfer(result, "ImageData"))
                .finish(),
            Input::ImageLoaded {
                attachment,
                full,
                result,
            } => f
                .debug_struct("ImageLoaded")
                .field("attachment", attachment)
                .field("full", full)
                .field("result", &ResXfer(result, "ImageData"))
                .finish(),
            Input::DeleteResult { message, result } => f
                .debug_struct("DeleteResult")
                .field("message", message)
                .field("result", &ResReq(result, "MessageResponse"))
                .finish(),
            Input::OperationStatusResult { operation, result } => f
                .debug_struct("OperationStatusResult")
                .field("operation", operation)
                .field("result", &ResReq(result, "OperationStatus"))
                .finish(),
            Input::Socket(v0) => f.debug_tuple("Socket").field(v0).finish(),
            Input::Frame(v0) => f.debug_tuple("Frame").field(&Frame(v0)).finish(),
            Input::StorageProblem(v0) => f.debug_tuple("StorageProblem").field(&Text(v0)).finish(),
            Input::ConversationCommandResult { operation, result } => f
                .debug_struct("ConversationCommandResult")
                .field("operation", operation)
                .field("result", &ResReq(result, "ConversationResponse"))
                .finish(),
            Input::ActionResult { kind, result } => f
                .debug_struct("ActionResult")
                .field("kind", kind)
                .field("result", &ResReq(result, "ActionOutcome"))
                .finish(),
            Input::MarkReadResult {
                conversation,
                up_to,
                result,
            } => f
                .debug_struct("MarkReadResult")
                .field("conversation", conversation)
                .field("up_to", up_to)
                .field("result", &ResReq(result, "ConversationUserState"))
                .finish(),
            Input::Clock(v0) => f.debug_tuple("Clock").field(v0).finish(),
            Input::SetNotificationSettings(v0) => {
                f.debug_tuple("SetNotificationSettings").field(v0).finish()
            }
            Input::SetConversationNotificationPrefs {
                conversation,
                level,
                muted,
            } => f
                .debug_struct("SetConversationNotificationPrefs")
                .field("conversation", conversation)
                .field("level", level)
                .field("muted", muted)
                .finish(),
            Input::PauseNotifications { until } => f
                .debug_struct("PauseNotifications")
                .field("until", until)
                .finish(),
            Input::NotificationSettingsResult { workspace, result } => f
                .debug_struct("NotificationSettingsResult")
                .field("workspace", workspace)
                .field("result", &ResReq(result, "NotificationSettingsResponse"))
                .finish(),
            Input::ConversationPrefsResult {
                conversation,
                result,
            } => f
                .debug_struct("ConversationPrefsResult")
                .field("conversation", conversation)
                .field(
                    "result",
                    &ResReq(result, "ConversationNotificationPrefsResponse"),
                )
                .finish(),
            Input::SetStatus(v0) => f
                .debug_tuple("SetStatus")
                .field(&OptNamed(v0, "UserStatus"))
                .finish(),
            Input::StatusResult { workspace, result } => f
                .debug_struct("StatusResult")
                .field("workspace", workspace)
                .field("result", &ResReq(result, "StatusResponse"))
                .finish(),
            Input::NotificationActivated {
                conversation,
                message,
            } => f
                .debug_struct("NotificationActivated")
                .field("conversation", conversation)
                .field("message", message)
                .finish(),
            Input::OpenInbox => f.write_str("OpenInbox"),
            Input::LoadInbox { unread_only } => f
                .debug_struct("LoadInbox")
                .field("unread_only", unread_only)
                .finish(),
            Input::LoadMoreInbox => f.write_str("LoadMoreInbox"),
            Input::MarkInboxRead => f.write_str("MarkInboxRead"),
            Input::CloseInbox => f.write_str("CloseInbox"),
            Input::InboxLoaded { generation, result } => f
                .debug_struct("InboxLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "InboxPage"))
                .finish(),
            Input::InboxReadResult { result } => f
                .debug_struct("InboxReadResult")
                .field("result", &ResReq(result, "InboxReadResponse"))
                .finish(),
            Input::SetMessageSaved { message_id, saved } => f
                .debug_struct("SetMessageSaved")
                .field("message_id", message_id)
                .field("saved", saved)
                .finish(),
            Input::MessageSavedResult { message_id, result } => f
                .debug_struct("MessageSavedResult")
                .field("message_id", message_id)
                .field("result", &ResReq(result, "SavedResponse"))
                .finish(),
            Input::OpenSaved => f.write_str("OpenSaved"),
            Input::CloseSaved => f.write_str("CloseSaved"),
            Input::LoadMoreSaved => f.write_str("LoadMoreSaved"),
            Input::SavedLoaded { generation, result } => f
                .debug_struct("SavedLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "SavedPage"))
                .finish(),
            Input::OpenPeople => f.write_str("OpenPeople"),
            Input::ClosePeople => f.write_str("ClosePeople"),
            Input::SetPeopleQuery(v0) => f.debug_tuple("SetPeopleQuery").field(&Text(v0)).finish(),
            Input::LoadMorePeople => f.write_str("LoadMorePeople"),
            Input::PeopleLoaded { generation, result } => f
                .debug_struct("PeopleLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "PeoplePage"))
                .finish(),
            Input::OpenProfile { user_id } => f
                .debug_struct("OpenProfile")
                .field("user_id", user_id)
                .finish(),
            Input::CloseProfile => f.write_str("CloseProfile"),
            Input::ProfileLoaded { generation, result } => f
                .debug_struct("ProfileLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "PersonResponse"))
                .finish(),
            Input::SetBrowseQuery(v0) => f.debug_tuple("SetBrowseQuery").field(&Text(v0)).finish(),
            Input::SetBrowseJoined(v0) => f.debug_tuple("SetBrowseJoined").field(v0).finish(),
            Input::SetBrowseSort(v0) => f.debug_tuple("SetBrowseSort").field(v0).finish(),
            Input::LoadMoreBrowse => f.write_str("LoadMoreBrowse"),
            Input::CloseBrowse => f.write_str("CloseBrowse"),
            Input::BrowseLoaded { generation, result } => f
                .debug_struct("BrowseLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "BrowseChannelsResponse"))
                .finish(),
            Input::ChooseQuickSwitcher(_) => f
                .debug_tuple("ChooseQuickSwitcher")
                .field(&Named("QuickSwitcherTarget"))
                .finish(),
            Input::UpdateWorkspace { name, settings } => f
                .debug_struct("UpdateWorkspace")
                .field("name", &OptText(name))
                .field("settings", settings)
                .finish(),
            Input::SetMemberRole { user_id, role } => f
                .debug_struct("SetMemberRole")
                .field("user_id", user_id)
                .field("role", role)
                .finish(),
            Input::SetMemberDeactivated {
                user_id,
                deactivated,
            } => f
                .debug_struct("SetMemberDeactivated")
                .field("user_id", user_id)
                .field("deactivated", deactivated)
                .finish(),
            Input::SetPeopleIncludeDeactivated(v0) => f
                .debug_tuple("SetPeopleIncludeDeactivated")
                .field(v0)
                .finish(),
            Input::OpenInvitations => f.write_str("OpenInvitations"),
            Input::CloseInvitations => f.write_str("CloseInvitations"),
            Input::CreateInvitation {
                expires_in_secs,
                max_uses,
            } => f
                .debug_struct("CreateInvitation")
                .field("expires_in_secs", expires_in_secs)
                .field("max_uses", max_uses)
                .finish(),
            Input::RevokeInvitation(v0) => f.debug_tuple("RevokeInvitation").field(v0).finish(),
            Input::DismissCreatedInvitation => f.write_str("DismissCreatedInvitation"),
            Input::SwitchWorkspace(v0) => f.debug_tuple("SwitchWorkspace").field(v0).finish(),
            Input::CreateWorkspace { name } => f
                .debug_struct("CreateWorkspace")
                .field("name", &Text(name))
                .finish(),
            Input::AcceptInvitation { code } => f
                .debug_struct("AcceptInvitation")
                .field("code", &Text(code))
                .finish(),
            Input::UpdateProfile { display_name } => f
                .debug_struct("UpdateProfile")
                .field("display_name", &OptText(display_name))
                .finish(),
            Input::SetAvatar { source } => f
                .debug_struct("SetAvatar")
                .field("source", &Source(source))
                .finish(),
            Input::RemoveAvatar => f.write_str("RemoveAvatar"),
            Input::LoadAvatar(v0) => f.debug_tuple("LoadAvatar").field(v0).finish(),
            Input::SetAppearance(v0) => f.debug_tuple("SetAppearance").field(v0).finish(),
            Input::AvatarLoaded {
                user,
                version,
                result,
            } => f
                .debug_struct("AvatarLoaded")
                .field("user", user)
                .field("version", version)
                .field("result", &ResXfer(result, "ImageData"))
                .finish(),
            Input::InvitationsLoaded { generation, result } => f
                .debug_struct("InvitationsLoaded")
                .field("generation", generation)
                .field("result", &ResReq(result, "InvitationList"))
                .finish(),
            Input::SetServerUrl(v0) => f.debug_tuple("SetServerUrl").field(&Text(v0)).finish(),
            Input::CheckServer => f.write_str("CheckServer"),
            Input::ServerChecked { generation, result } => f
                .debug_struct("ServerChecked")
                .field("generation", generation)
                .field("result", &ResText(result, "protocol version"))
                .finish(),
            Input::ServerSwitched { generation, result } => f
                .debug_struct("ServerSwitched")
                .field("generation", generation)
                .field("result", &ResText(result, "StartupState"))
                .finish(),
            Input::ServerFacts { url, facts } => f
                .debug_struct("ServerFacts")
                .field("url", &Text(url))
                .field("facts", facts)
                .finish(),
            Input::PreviewInvitation { code } => f
                .debug_struct("PreviewInvitation")
                .field("code", &Text(code))
                .finish(),
            Input::InvitationPreviewed { generation, result } => f
                .debug_struct("InvitationPreviewed")
                .field("generation", generation)
                .field(
                    "result",
                    &match result {
                        Ok(preview) => format!(
                            "Ok(InvitationPreview {{ kind: {:?}, member_count: {} }})",
                            preview.kind, preview.member_count
                        ),
                        Err(failure) => format!("Err({failure:?})"),
                    },
                )
                .finish(),
        }
    }
}
