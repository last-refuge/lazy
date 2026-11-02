//! Drop-ins (`llm-docs/DROP_INS.md`): a call in a conversation, with video. The core holds Lazy's
//! side (which conversation, who is in it, whether this person is muted or has video on); the
//! runtime runs the call through the media library and reports what happens as
//! [`DropInReport`]s. Frames never pass through the core.

use lazy_domain::{ConversationId, UserId};

/// Where a drop-in this person is in stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropInStatus {
    /// Asking the server for a way in, and knocking on the call server.
    Joining,
    /// In the room; the media path is being set up.
    Connecting,
    /// Media is flowing.
    Live,
}

/// Someone else in the drop-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropInPeer {
    /// The call server's number for them, for this call.
    pub id: u64,
    /// Who they are in Lazy, when the call server's identity is a Lazy user.
    pub user: Option<UserId>,
    pub name: String,
    pub muted: bool,
    pub video_on: bool,
}

/// The drop-in this person is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIn {
    pub conversation: ConversationId,
    pub status: DropInStatus,
    pub muted: bool,
    pub video_on: bool,
    pub peers: Vec<DropInPeer>,
    /// Who is speaking now; `None` is this person.
    pub speaking: Vec<Option<u64>>,
    /// The latest device that could not be used (camera, microphone, speakers), for people.
    pub problem: Option<String>,
}

impl DropIn {
    pub fn is_speaking(&self, peer: Option<u64>) -> bool {
        self.speaking.contains(&peer)
    }
}

/// What the runtime says about a call. `session` in [`crate::Input::DropInReport`] ties a report
/// to the join it belongs to, so a late report from a call already left is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropInReport {
    /// In the room.
    Joined,
    /// Media is flowing.
    Connected,
    /// Everyone else, as they are now.
    Peers(Vec<DropInPeer>),
    Speaking(Vec<Option<u64>>),
    DeviceProblem(String),
    /// The call ended: left (`None`), or why it could not go on.
    Ended(Option<String>),
}
