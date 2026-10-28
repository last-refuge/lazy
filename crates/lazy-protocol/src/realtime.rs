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

use lazy_domain::{EventSeq, OperationId, Timestamp, WorkspaceId};
use serde::{Deserialize, Serialize};

use crate::{ErrorCode, Message, User};

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
