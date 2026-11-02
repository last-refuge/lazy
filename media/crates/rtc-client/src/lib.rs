//! The rtc media library's client: joins a room on an SFU, sends a camera and a microphone, and
//! receives everyone else.
//!
//! ```no_run
//! # fn run(handle: &tokio::runtime::Handle) {
//! use rtc_client::{Call, CallConfig};
//! let frames = rtc_video::FrameStore::new();
//! let call = Call::start(
//!     CallConfig::new("wss://rtc.example.com/rtc", "rtc1.…", frames.clone()),
//!     handle,
//!     |event| println!("{event:?}"),
//! );
//! call.set_muted(true);
//! call.leave();
//! # }
//! ```
//!
//! What happens where:
//! - A Tokio task owns the str0m connection, the UDP socket and the signaling WebSocket.
//! - The camera (or test pattern) is read and encoded on its own thread; so is the microphone.
//! - Each remote video track is decoded on its own thread into the [`FrameStore`], which the
//!   host draws from; remote audio is decoded into a [`Mixer`] that the speakers play.
//!
//! Not here yet (`llm-docs/DROP_INS.md`, M2): reconnecting after a network change (ICE restart),
//! TURN and TCP for restrictive networks, echo cancellation.

mod media;
mod session;

use std::sync::Arc;

use rtc_audio::AudioInput;
use rtc_proto::{Participant, ParticipantId};
use rtc_video::FrameStore;
use rtc_video::source::VideoInput;
use tokio::sync::mpsc;

/// How to join.
#[derive(Clone)]
pub struct CallConfig {
    /// The SFU's signaling address (`ws://` or `wss://`, ending in [`rtc_proto::SIGNAL_PATH`]).
    pub url: String,
    pub token: String,
    /// The camera, or a test pattern. Sent only while video is on.
    pub video: VideoInput,
    /// Whether video starts on.
    pub video_on: bool,
    pub audio: AudioInput,
    /// Whether the microphone starts muted.
    pub muted: bool,
    /// Whether to play the others on the speakers.
    pub speakers: bool,
    /// Which speakers ([`rtc_audio::AudioDevice::id`]); `None` is the system's default.
    pub speaker_device: Option<String>,
    /// Where frames go: [`rtc_video::TileKey::Local`] for this client's own preview, and
    /// [`rtc_video::TileKey::Remote`] for each participant.
    pub frames: FrameStore,
}

impl CallConfig {
    /// The camera and microphone, both on, played on the speakers.
    pub fn new(url: impl Into<String>, token: impl Into<String>, frames: FrameStore) -> Self {
        CallConfig {
            url: url.into(),
            token: token.into(),
            video: VideoInput::Camera { name: None },
            video_on: true,
            audio: AudioInput::Microphone { device: None },
            muted: false,
            speakers: true,
            speaker_device: None,
            frames,
        }
    }
}

/// What a call reports, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum CallEvent {
    /// The SFU accepted the join; `you` is this client's number in the room.
    Joined { you: ParticipantId },
    /// Media is flowing (the network path to the SFU is up).
    Connected,
    /// Everyone else in the room, as it is now.
    Participants(Vec<Participant>),
    /// Who is speaking now (`None` in the set means this client).
    Speaking(Vec<Option<ParticipantId>>),
    /// A device could not be used; the call goes on without it.
    DeviceProblem(String),
    /// The call is over: left (`None`), or why it ended.
    Ended(Option<String>),
}

/// A call in progress. Dropping it leaves.
pub struct Call {
    commands: mpsc::UnboundedSender<Command>,
}

#[derive(Debug)]
pub(crate) enum Command {
    SetMuted(bool),
    SetVideo(bool),
    SetVideoInput(VideoInput),
    SetAudioInput(AudioInput),
    SetSpeakers(Option<String>),
    Leave,
}

pub(crate) type Events = Arc<dyn Fn(CallEvent) + Send + Sync>;

impl Call {
    /// Joins on `handle`'s runtime; `events` is called (on that runtime) with everything that
    /// happens, ending with [`CallEvent::Ended`].
    pub fn start(
        config: CallConfig,
        handle: &tokio::runtime::Handle,
        events: impl Fn(CallEvent) + Send + Sync + 'static,
    ) -> Call {
        let (commands, receiver) = mpsc::unbounded_channel();
        let events: Events = Arc::new(events);
        handle.spawn(session::run(config, receiver, events));
        Call { commands }
    }

    pub fn set_muted(&self, muted: bool) {
        self.commands.send(Command::SetMuted(muted)).ok();
    }

    pub fn set_video(&self, on: bool) {
        self.commands.send(Command::SetVideo(on)).ok();
    }

    /// Switches the camera (or other video source); used from the next frame, or when video is
    /// next turned on.
    pub fn set_video_input(&self, input: VideoInput) {
        self.commands.send(Command::SetVideoInput(input)).ok();
    }

    /// Switches the microphone (or other sound source).
    pub fn set_audio_input(&self, input: AudioInput) {
        self.commands.send(Command::SetAudioInput(input)).ok();
    }

    /// Switches the speakers ([`rtc_audio::AudioDevice::id`]; `None` is the system's default).
    /// Nothing changes if the call was started without speakers.
    pub fn set_speakers(&self, device: Option<String>) {
        self.commands.send(Command::SetSpeakers(device)).ok();
    }

    pub fn leave(&self) {
        self.commands.send(Command::Leave).ok();
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        self.commands.send(Command::Leave).ok();
    }
}
