//! Identity newtypes. Each wraps a UUID (or a sequence number) so that, for example, a
//! [`MessageId`] can never be passed where a [`ConversationId`] is expected.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident, $generate:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// Generates a fresh identity.
            pub fn generate() -> Self {
                Self($generate)
            }

            /// Wraps an existing UUID, for example one read back from storage.
            pub const fn from_uuid(uuid: Uuid) -> Self {
                Self(uuid)
            }

            /// Returns the underlying UUID.
            pub const fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

uuid_id!(
    /// A person with an account.
    UserId,
    Uuid::now_v7()
);
uuid_id!(
    /// A workspace (organization).
    WorkspaceId,
    Uuid::now_v7()
);
uuid_id!(
    /// A channel or, later, a direct or group conversation.
    ConversationId,
    Uuid::now_v7()
);
uuid_id!(
    /// A user group: a named set of people that a message can mention together.
    GroupId,
    Uuid::now_v7()
);
uuid_id!(
    /// A custom emoji of a workspace. Chosen by the uploading client, so a retried upload is the
    /// same upload.
    EmojiId,
    Uuid::now_v7()
);
uuid_id!(
    /// A file uploaded to be attached to a message. Chosen by the uploading client, so a retried
    /// upload is the same upload.
    AttachmentId,
    Uuid::now_v7()
);
uuid_id!(
    /// An incoming webhook: an address that posts into one channel as a bot.
    WebhookId,
    Uuid::now_v7()
);
uuid_id!(
    /// A GitHub subscription: one repository's activity followed in one channel (D32).
    GithubSubscriptionId,
    Uuid::now_v7()
);
uuid_id!(
    /// A webhook connector's subscription: one team's or project's activity followed in one
    /// channel (D34).
    ConnectorSubscriptionId,
    Uuid::now_v7()
);
uuid_id!(
    /// An invitation to join a workspace. Assigned by the server; it names the invitation, and the
    /// separate code is what a person types.
    InvitationId,
    Uuid::now_v7()
);
uuid_id!(
    /// A message. Assigned by the server when the message is committed.
    MessageId,
    Uuid::now_v7()
);
uuid_id!(
    /// A client-assigned identity for one submitted mutation. Retrying the same mutation reuses
    /// the same `OperationId`; a new mutation never does.
    OperationId,
    Uuid::new_v4()
);

/// Position of an event in one workspace's durable event log.
///
/// Sequence numbers are gapless and assigned in commit order: if event `n + 1` is visible to a
/// reader, event `n` is too. Clients persist the last applied value as their replay cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct EventSeq(i64);

impl EventSeq {
    /// The position before the first event.
    pub const ZERO: EventSeq = EventSeq(0);

    /// Returns `None` for a negative value.
    pub const fn new(value: i64) -> Option<Self> {
        if value >= 0 { Some(Self(value)) } else { None }
    }

    /// Returns the raw value, for storage and comparisons.
    pub const fn get(self) -> i64 {
        self.0
    }

    /// Whether this is [`EventSeq::ZERO`]; lets serialization skip a position that was never set.
    pub fn is_zero(&self) -> bool {
        self.0 == 0
    }
}

impl Default for EventSeq {
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error for a negative sequence number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("event sequence number must not be negative")]
pub struct NegativeSeq;

impl TryFrom<i64> for EventSeq {
    type Error = NegativeSeq;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        Self::new(value).ok_or(NegativeSeq)
    }
}

impl From<EventSeq> for i64 {
    fn from(seq: EventSeq) -> i64 {
        seq.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_roundtrip_through_json_as_plain_strings() {
        let id = MessageId::generate();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<MessageId>(&json).unwrap(), id);
    }

    #[test]
    fn event_seq_rejects_negative_values_on_the_wire() {
        assert!(serde_json::from_str::<EventSeq>("-1").is_err());
        assert_eq!(serde_json::from_str::<EventSeq>("7").unwrap().get(), 7);
    }

    #[test]
    fn event_seq_orders_numerically() {
        assert!(EventSeq::new(2).unwrap() < EventSeq::new(10).unwrap());
    }
}
