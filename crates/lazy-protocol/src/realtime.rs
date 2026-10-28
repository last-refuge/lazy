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

use lazy_domain::{ConversationId, EventSeq, OperationId, Timestamp, UserId, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::{Conversation, ConversationUserState, ErrorCode, Message, User};

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
    fn member_joined_roundtrips() {
        let event = Event::MemberJoined {
            user: User {
                id: UserId::generate(),
                username: Username::parse("zoe").unwrap(),
                display_name: DisplayName::parse("Zoë").unwrap(),
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
