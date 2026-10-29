//! The realtime WebSocket protocol. Frames are JSON text messages.
//!
//! Ordering guarantees for one workspace subscription:
//!
//! * [`ServerFrame::Event`] frames arrive in strictly increasing `seq`.
//! * Event positions are gapless in the server's log, but a subscriber receives only events it
//!   may see, so a subscriber's own sequence of positions can skip values.
//! * After [`ServerFrame::CaughtUp`], every committed event the subscriber may see is delivered
//!   exactly once, in order, as it commits.
//! * Delivery never skips an event that committed concurrently with the replay.
//!
//! Who receives an event (conversations and membership):
//!
//! * Message events go to the conversation's members only. Public channels are readable through
//!   the history API by anyone in the workspace, but live delivery is for members.
//! * [`Event::MemberAdded`] and [`Event::MemberRemoved`] also go to the person they are about,
//!   even though a removed person no longer has access to the conversation. Everything else about
//!   a conversation stops reaching them at the moment of removal, including events that committed
//!   before the removal but have not been delivered yet or are replayed later.
//! * [`Event::UserStateChanged`] goes only to the person whose state it is.
//! * A client may receive events for a conversation it does not know yet (replay can deliver a
//!   channel's older messages before the event that added the person). It ignores them; the
//!   history it fetches after [`Event::MemberAdded`] or [`Event::ConversationCreated`] covers
//!   them.

use lazy_domain::notification::{ConversationNotificationPrefs, NotificationSettings, UserStatus};
use lazy_domain::{
    ConversationId, DisplayName, EventSeq, GroupId, MessageId, OperationId, Timestamp, UserId,
    WorkspaceId, WorkspaceRole,
};
use serde::{Deserialize, Serialize};

use crate::{
    Conversation, ConversationUserState, CustomEmoji, ErrorCode, Mentions, Message,
    ReactionSummary, ThreadSummary, ThreadUserState, User, UserGroup, Workspace,
};

/// Something that happened in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// A message was committed.
    MessageCreated {
        message: Message,
        /// The operation that created it, so its sender can reconcile an optimistic message.
        operation_id: Option<OperationId>,
    },
    /// A message was edited. `message` is the new revision; `seq` is still where it was created.
    /// Delivered to the conversation's members, like the original.
    MessageEdited {
        message: Message,
        /// The operation that made the edit, so the editor can reconcile.
        operation_id: Option<OperationId>,
        /// Who the message notified before this edit, so a client can correct its mention counts
        /// without holding the earlier version.
        #[serde(default, skip_serializing_if = "Mentions::is_empty")]
        previous_mentions: Mentions,
    },
    /// The reactions on a message changed. `reactions` is the complete new list (not a change to
    /// apply), and this event's position is its `reactions_as_of`: a client applies it only if
    /// that is newer than what it holds. Delivered to the conversation's members.
    ReactionsChanged {
        conversation_id: ConversationId,
        message_id: MessageId,
        reactions: Vec<ReactionSummary>,
    },
    /// A message was deleted. `message` is the tombstone: same id, author and position, the
    /// placeholder document, `deleted` set, and a higher revision. Clients remove it from view
    /// and, if it counted as unread, stop counting it.
    MessageDeleted {
        message: Message,
        /// Who the message notified before it was deleted (the tombstone notifies nobody).
        #[serde(default, skip_serializing_if = "Mentions::is_empty")]
        had_mentions: Mentions,
    },
    /// The replies under a root changed (one was added or deleted). `summary` is the complete new
    /// summary, or `None` when no replies remain; its `as_of` is this event's position, and a
    /// client applies it only if that is newer than what it holds. Delivered to the
    /// conversation's members.
    ThreadChanged {
        conversation_id: ConversationId,
        root_id: MessageId,
        summary: Option<ThreadSummary>,
    },
    /// The recipient's own follow flag, read position, or counts for a thread changed, possibly
    /// on another device. Delivered only to that person.
    ThreadStateChanged { state: ThreadUserState },
    /// The recipient's own notification settings changed, possibly on another device. Delivered
    /// only to that person.
    NotificationSettingsChanged { settings: NotificationSettings },
    /// The recipient's own preference for one conversation changed (`level` `None` and not muted
    /// means it was reset). Delivered only to that person.
    ConversationNotificationPrefsChanged {
        prefs: ConversationNotificationPrefs,
    },
    /// Someone set or cleared their status. Delivered to everyone in the workspace. An expiring
    /// status is not announced again when it expires: clients compare `expires_at` with the time.
    UserStatusChanged {
        user_id: UserId,
        status: Option<UserStatus>,
    },
    /// The recipient saved a message, or removed it from their saved messages, possibly on another
    /// device. Delivered only to that person. Saving something already saved announces nothing.
    SavedChanged { message_id: MessageId, saved: bool },
    /// A custom emoji was created, renamed, or deleted (`deleted` set). Delivered to everyone in
    /// the workspace, so pickers and autocomplete stay current.
    EmojiChanged { emoji: CustomEmoji },
    /// A user group was created or changed (its handle, name, or members). Delivered to everyone
    /// in the workspace, so autocomplete stays current.
    GroupChanged { group: UserGroup },
    /// A user group was deleted. Messages that mentioned it keep the handle they were written
    /// with.
    GroupDeleted { group_id: GroupId },
    /// The workspace's name or settings changed. Delivered to everyone in the workspace.
    WorkspaceUpdated { workspace: Workspace },
    /// Someone's role changed. Delivered to everyone in the workspace (the person included, whose
    /// own role this is).
    MemberRoleChanged {
        user_id: UserId,
        role: WorkspaceRole,
    },
    /// Someone's membership was deactivated or reactivated. Delivered to everyone in the workspace
    /// except the person deactivated, whose connections to it end; a reactivated person learns
    /// from their next bootstrap.
    MemberDeactivated { user_id: UserId, deactivated: bool },
    /// Someone changed their display name or avatar. Delivered to every workspace the person
    /// belongs to, once in each log. (Their status and deactivation have their own events.)
    UserProfileChanged {
        user_id: UserId,
        display_name: DisplayName,
        avatar_version: u32,
    },
    /// Someone joined the workspace.
    MemberJoined { user: User },
    /// A channel, direct conversation, or group conversation was created. Delivered to its
    /// members only. Public channels the recipient has not joined appear through browsing.
    ConversationCreated { conversation: Conversation },
    /// A conversation's name, topic, description, archived flag, or member count changed.
    /// Delivered to its members.
    ConversationUpdated { conversation: Conversation },
    /// Someone was added to, or joined, a channel. `conversation` is current as of the change, so
    /// the person who was added can add it to their sidebar. Delivered to the members and to the
    /// person added.
    MemberAdded {
        conversation: Conversation,
        user_id: UserId,
    },
    /// Someone left or was removed. Delivered to the remaining members and to the person removed,
    /// who must stop showing the conversation. Anything they had not yet received about it is
    /// never delivered afterwards.
    MemberRemoved {
        conversation_id: ConversationId,
        user_id: UserId,
    },
    /// The recipient's own favorite flag or read position changed, possibly on another device.
    /// Delivered only to that person.
    UserStateChanged { state: ConversationUserState },
}

/// An event with its position in the workspace's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub workspace_id: WorkspaceId,
    pub seq: EventSeq,
    pub occurred_at: Timestamp,
    pub event: Event,
}

/// Frames sent by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Start (or restart) delivery for one workspace. `after` is the last position the client
    /// has applied. Sending it again for the same workspace replaces the previous subscription.
    Subscribe {
        workspace_id: WorkspaceId,
        after: EventSeq,
    },
}

/// Why a subscription could not be resumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotReason {
    /// The cursor is older than the retained log.
    CursorExpired,
    /// The cursor is newer than anything the server has, for example after a restore.
    CursorAhead,
}

/// Frames sent by the server.
// A frame is built, serialized or parsed, handled, and dropped; none is kept in bulk, so the size
// of the event variant next to the small control frames costs nothing worth boxing it for.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    Event(EventEnvelope),
    /// Replay finished; the live stream follows. `seq` is the log head at that moment, so the
    /// client can advance its cursor even when the newest events were not visible to it.
    CaughtUp {
        workspace_id: WorkspaceId,
        seq: EventSeq,
    },
    /// The subscription cannot resume; fetch a fresh bootstrap and subscribe again.
    SnapshotRequired {
        workspace_id: WorkspaceId,
        reason: SnapshotReason,
    },
    /// A request frame was rejected. The connection stays open unless the code is
    /// `unauthenticated`.
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use lazy_domain::{ConversationId, DisplayName, MessageDocument, MessageId, UserId, Username};

    use super::*;

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
    fn subscribe_frame_shape() {
        let ws = WorkspaceId::generate();
        let frame = ClientFrame::Subscribe {
            workspace_id: ws,
            after: EventSeq::new(9).unwrap(),
        };
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            serde_json::json!({"type": "subscribe", "workspace_id": ws.to_string(), "after": 9})
        );
    }

    #[test]
    fn event_frames_roundtrip() {
        let frame = ServerFrame::Event(EventEnvelope {
            workspace_id: WorkspaceId::generate(),
            seq: EventSeq::new(3).unwrap(),
            occurred_at: Timestamp::now(),
            event: Event::MessageCreated {
                message: sample_message(),
                operation_id: Some(OperationId::generate()),
            },
        });
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains(r#""type":"event""#));
        assert_eq!(serde_json::from_str::<ServerFrame>(&json).unwrap(), frame);
    }

    #[test]
    fn message_lifecycle_events_roundtrip() {
        let mut message = sample_message();
        message.revision = 2;
        message.edited_at = Some(Timestamp::now());
        let edited = Event::MessageEdited {
            message: message.clone(),
            operation_id: Some(OperationId::generate()),
            previous_mentions: Mentions {
                users: vec![UserId::generate()],
                channel: true,
            },
        };
        message.revision = 3;
        message.deleted = true;
        let deleted = Event::MessageDeleted {
            message,
            had_mentions: Mentions::default(),
        };
        for event in [edited, deleted] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
        let json = serde_json::to_value(&Event::MessageDeleted {
            message: sample_message(),
            had_mentions: Mentions::default(),
        })
        .unwrap();
        assert_eq!(json["type"], "message_deleted");
    }

    #[test]
    fn reaction_events_roundtrip() {
        let event = Event::ReactionsChanged {
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
            reactions: vec![ReactionSummary {
                emoji: lazy_domain::ReactionEmoji::parse("🎉").unwrap(),
                users: vec![UserId::generate(), UserId::generate()],
            }],
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"reactions_changed""#));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        // Removing the last reaction is an empty list, not a missing field.
        let cleared = Event::ReactionsChanged {
            conversation_id: ConversationId::generate(),
            message_id: MessageId::generate(),
            reactions: Vec::new(),
        };
        let json = serde_json::to_string(&cleared).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), cleared);
    }

    #[test]
    fn thread_events_roundtrip() {
        let conversation_id = ConversationId::generate();
        let root_id = MessageId::generate();
        let summary = ThreadSummary {
            reply_count: 2,
            last_reply_seq: EventSeq::new(9).unwrap(),
            last_reply_at: Timestamp::now(),
            recent_repliers: vec![UserId::generate()],
            as_of: EventSeq::new(9).unwrap(),
        };
        let state = ThreadUserState {
            root_id,
            conversation_id,
            following: true,
            last_read: EventSeq::new(5).unwrap(),
            unread_count: 2,
            mention_count: 1,
        };
        for event in [
            Event::ThreadChanged {
                conversation_id,
                root_id,
                summary: Some(summary),
            },
            Event::ThreadChanged {
                conversation_id,
                root_id,
                summary: None,
            },
            Event::ThreadStateChanged { state },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn notification_and_status_events_roundtrip() {
        use lazy_domain::notification::NotifyLevel;
        let conversation_id = ConversationId::generate();
        for event in [
            Event::NotificationSettingsChanged {
                settings: NotificationSettings::default(),
            },
            Event::ConversationNotificationPrefsChanged {
                prefs: ConversationNotificationPrefs {
                    conversation_id,
                    level: Some(NotifyLevel::All),
                    muted: false,
                },
            },
            Event::UserStatusChanged {
                user_id: UserId::generate(),
                status: Some(UserStatus::new("Out today", None, None).unwrap()),
            },
            Event::UserStatusChanged {
                user_id: UserId::generate(),
                status: None,
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn administration_events_roundtrip() {
        for event in [
            Event::WorkspaceUpdated {
                workspace: Workspace {
                    id: WorkspaceId::generate(),
                    name: "Acme".to_owned(),
                    settings: Default::default(),
                },
            },
            Event::MemberRoleChanged {
                user_id: UserId::generate(),
                role: WorkspaceRole::Admin,
            },
            Event::MemberDeactivated {
                user_id: UserId::generate(),
                deactivated: true,
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn profile_changes_roundtrip() {
        let event = Event::UserProfileChanged {
            user_id: UserId::generate(),
            display_name: DisplayName::parse("Alice N.").unwrap(),
            avatar_version: 4,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"user_profile_changed""#));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn saved_changed_roundtrips() {
        for saved in [true, false] {
            let event = Event::SavedChanged {
                message_id: MessageId::generate(),
                saved,
            };
            let json = serde_json::to_string(&event).unwrap();
            assert!(json.contains(r#""type":"saved_changed""#));
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn emoji_events_roundtrip() {
        let emoji = CustomEmoji {
            id: lazy_domain::EmojiId::generate(),
            workspace_id: WorkspaceId::generate(),
            name: "party_parrot".to_owned(),
            created_by: UserId::generate(),
            created_at: Timestamp::now(),
            deleted: false,
        };
        let event = Event::EmojiChanged { emoji };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(r#""type":"emoji_changed""#));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    #[test]
    fn group_events_roundtrip() {
        let group = UserGroup {
            id: GroupId::generate(),
            workspace_id: WorkspaceId::generate(),
            handle: lazy_domain::GroupHandle::parse("design-team").unwrap(),
            name: lazy_domain::DisplayName::parse("Design team").unwrap(),
            members: vec![UserId::generate(), UserId::generate()],
        };
        for event in [
            Event::GroupChanged {
                group: group.clone(),
            },
            Event::GroupDeleted { group_id: group.id },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn member_joined_roundtrips() {
        let event = Event::MemberJoined {
            user: User {
                id: UserId::generate(),
                username: Username::parse("zoe").unwrap(),
                display_name: DisplayName::parse("Zoë").unwrap(),
                is_bot: false,
                status: None,
                deactivated: false,
                avatar_version: 0,
            },
        };
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
    }

    fn sample_conversation() -> Conversation {
        Conversation {
            id: ConversationId::generate(),
            workspace_id: WorkspaceId::generate(),
            kind: lazy_domain::ConversationKind::Channel,
            name: Some(lazy_domain::ChannelName::parse("design").unwrap()),
            topic: Some("Pixels".into()),
            description: None,
            visibility: lazy_domain::ChannelVisibility::Private,
            archived: false,
            is_default: false,
            created_by: Some(UserId::generate()),
            participants: vec![],
            member_count: 3,
        }
    }

    #[test]
    fn conversation_events_roundtrip() {
        let conversation = sample_conversation();
        let state = ConversationUserState {
            conversation_id: conversation.id,
            favorite: true,
            last_read: EventSeq::new(12).unwrap(),
            unread_count: 4,
            mention_count: 0,
        };
        for event in [
            Event::ConversationCreated {
                conversation: conversation.clone(),
            },
            Event::ConversationUpdated {
                conversation: conversation.clone(),
            },
            Event::MemberAdded {
                conversation: conversation.clone(),
                user_id: UserId::generate(),
            },
            Event::MemberRemoved {
                conversation_id: conversation.id,
                user_id: UserId::generate(),
            },
            Event::UserStateChanged { state },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), event);
        }
    }

    #[test]
    fn direct_conversations_have_no_name_on_the_wire() {
        let mut conversation = sample_conversation();
        conversation.kind = lazy_domain::ConversationKind::Direct;
        conversation.name = None;
        conversation.participants = vec![UserId::generate(), UserId::generate()];
        let json = serde_json::to_value(&conversation).unwrap();
        assert_eq!(json["kind"], "direct");
        assert!(json["name"].is_null());
    }

    #[test]
    fn control_frames_roundtrip() {
        for frame in [
            ServerFrame::CaughtUp {
                workspace_id: WorkspaceId::generate(),
                seq: EventSeq::ZERO,
            },
            ServerFrame::SnapshotRequired {
                workspace_id: WorkspaceId::generate(),
                reason: SnapshotReason::CursorAhead,
            },
            ServerFrame::Error {
                code: ErrorCode::Forbidden,
                message: "no".into(),
            },
        ] {
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(serde_json::from_str::<ServerFrame>(&json).unwrap(), frame);
        }
    }
}
