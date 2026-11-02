//! What crosses the network between an rtc client, the SFU, and the host application.
//!
//! - [`token`]: the join token a host application signs with a secret it shares with the SFU. It
//!   names the room, who is joining, and until when.
//! - [`ClientMessage`] and [`ServerMessage`]: JSON text frames on the SFU's WebSocket (`/rtc`).
//!
//! Serde only: no runtime, no sockets, so any client (desktop, phone, test) can share it.

pub mod token;

use serde::{Deserialize, Serialize};

pub use token::{Claims, TokenError};

/// Bumped when a message changes incompatibly; a client sends it in [`ClientMessage::Join`].
pub const SIGNAL_VERSION: u32 = 1;

/// The SFU's WebSocket path.
pub const SIGNAL_PATH: &str = "/rtc";

/// Someone in a room, as the SFU numbers them for the life of their connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ParticipantId(pub u64);

impl std::fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "p{}", self.0)
    }
}

/// A person in a room, as everyone in it sees them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Participant {
    pub id: ParticipantId,
    /// The host application's identifier for the person (from the token).
    pub identity: String,
    /// The name to show (from the token).
    pub name: String,
    #[serde(default)]
    pub audio_muted: bool,
    #[serde(default)]
    pub video_on: bool,
}

/// What a track carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Audio,
    Video,
}

/// Which participant's media arrives on a negotiated media line (`mid`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackInfo {
    pub mid: String,
    pub participant: ParticipantId,
    pub kind: TrackKind,
}

/// A client's message to the SFU.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    /// The first message: the token, and an SDP offer with the client's own (send-only) tracks.
    Join {
        version: u32,
        token: String,
        offer: String,
        #[serde(default)]
        audio_muted: bool,
        #[serde(default)]
        video_on: bool,
    },
    /// The answer to the SFU's [`ServerMessage::Offer`].
    Answer { sdp: String },
    /// The client's microphone or camera changed.
    State { audio_muted: bool, video_on: bool },
    /// Leaving; the SFU also notices a closed socket.
    Leave,
}

/// The SFU's message to a client.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// The answer to [`ClientMessage::Join`], and who is already in the room.
    Joined {
        you: ParticipantId,
        answer: String,
        participants: Vec<Participant>,
    },
    /// A renegotiation: other people's tracks to receive. `tracks` maps every receiving media
    /// line in the offer to its participant.
    Offer {
        sdp: String,
        tracks: Vec<TrackInfo>,
    },
    ParticipantJoined {
        participant: Participant,
    },
    ParticipantChanged {
        participant: Participant,
    },
    ParticipantLeft {
        id: ParticipantId,
    },
    /// The request failed or the connection is being closed; `code` is stable, `message` is for
    /// people.
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// Why the SFU refused or ended a connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The token is malformed, forged, or expired.
    Unauthorized,
    /// The room is at capacity.
    RoomFull,
    /// The client speaks a different signaling version.
    Version,
    /// The message could not be understood (or arrived out of order).
    BadRequest,
    /// Something failed on the SFU.
    Internal,
    /// An unknown code from a newer SFU.
    #[serde(other)]
    Unknown,
}

impl ClientMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signaling messages serialize")
    }
}

impl ServerMessage {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("signaling messages serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_tagged_json() {
        let join = ClientMessage::Join {
            version: SIGNAL_VERSION,
            token: "t".into(),
            offer: "v=0".into(),
            audio_muted: true,
            video_on: false,
        };
        let json = join.to_json();
        assert!(json.contains(r#""type":"join""#), "{json}");
        assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), join);

        let offer = ServerMessage::Offer {
            sdp: "v=0".into(),
            tracks: vec![TrackInfo {
                mid: "3".into(),
                participant: ParticipantId(7),
                kind: TrackKind::Video,
            }],
        };
        let json = offer.to_json();
        assert!(json.contains(r#""participant":7"#), "{json}");
        assert_eq!(serde_json::from_str::<ServerMessage>(&json).unwrap(), offer);
    }

    #[test]
    fn an_unknown_error_code_is_still_read() {
        let message: ServerMessage =
            serde_json::from_str(r#"{"type":"error","code":"from_the_future","message":"x"}"#)
                .unwrap();
        assert_eq!(
            message,
            ServerMessage::Error {
                code: ErrorCode::Unknown,
                message: "x".into()
            }
        );
    }
}
