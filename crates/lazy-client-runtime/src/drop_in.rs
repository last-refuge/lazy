//! Running a drop-in: asking the server for a join token, then running the call with the media
//! library (`rtc-client`) on this runtime, and reporting what happens to the core as
//! [`Input::DropInReport`]. Frames go straight to the [`rtc_video::FrameStore`] the UI draws from.

use std::sync::Mutex;

use lazy_client_core::{
    DeviceChoices, DeviceKind, DropInPeer, DropInReport, Input, MediaDevice, MediaDevices,
};
use lazy_domain::{ConversationId, UserId};
use lazy_protocol::api::paths;
use rtc_audio::AudioInput;
use rtc_client::{Call, CallConfig, CallEvent};
use rtc_video::source::VideoInput;

/// What a drop-in sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DropInMedia {
    /// The camera, microphone, and speakers.
    #[default]
    Devices,
    /// A moving test pattern for video, and no sound in or out: for machines without a camera,
    /// and for tests. `LAZY_DROP_IN_TEST_MEDIA=1` chooses it.
    TestPattern,
}

impl DropInMedia {
    pub fn from_env() -> DropInMedia {
        if std::env::var("LAZY_DROP_IN_TEST_MEDIA").is_ok_and(|v| v == "1") {
            DropInMedia::TestPattern
        } else {
            DropInMedia::Devices
        }
    }
}

/// The drop-in this runtime runs: the join it belongs to, what the person wants, and the call
/// once it has started.
#[derive(Default)]
pub(crate) struct DropInSlot {
    state: Mutex<Option<Slot>>,
}

struct Slot {
    session: u64,
    muted: bool,
    video_on: bool,
    devices: DeviceChoices,
    /// The test pattern's color, when the call sends one instead of the camera.
    test_color: Option<[u8; 3]>,
    call: Option<Call>,
}

/// The second camera offered with [`DropInMedia::TestPattern`], in another color, so switching
/// cameras can be tried (and tested) without one.
pub(crate) const SECOND_TEST_CAMERA: &str = "Second test pattern";
const FIRST_TEST_CAMERA: &str = "Test pattern";

/// What the call sends for the chosen camera.
fn video_input(test_color: Option<[u8; 3]>, camera: Option<&str>) -> VideoInput {
    match test_color {
        Some(color) if camera == Some(SECOND_TEST_CAMERA) => VideoInput::TestPattern {
            color: color.map(|c| 255 - c),
        },
        Some(color) => VideoInput::TestPattern { color },
        None => VideoInput::Camera {
            name: camera.map(str::to_owned),
        },
    }
}

/// The devices to choose from (blocking: it asks the system).
pub(crate) fn list_devices(media: DropInMedia) -> MediaDevices {
    match media {
        DropInMedia::TestPattern => MediaDevices {
            cameras: [FIRST_TEST_CAMERA, SECOND_TEST_CAMERA]
                .map(|name| MediaDevice {
                    id: name.into(),
                    name: name.into(),
                })
                .into(),
            ..MediaDevices::default()
        },
        DropInMedia::Devices => {
            let audio = |devices: Vec<rtc_audio::AudioDevice>| {
                devices
                    .into_iter()
                    .map(|d| MediaDevice {
                        id: d.id,
                        name: d.name,
                    })
                    .collect()
            };
            MediaDevices {
                // A camera is chosen by its name.
                cameras: rtc_video::source::cameras()
                    .into_iter()
                    .map(|name| MediaDevice {
                        id: name.clone(),
                        name,
                    })
                    .collect(),
                microphones: audio(rtc_audio::microphones()),
                speakers: audio(rtc_audio::speakers()),
            }
        }
    }
}

impl DropInSlot {
    /// Remembers a new join (leaving any call of an older one).
    pub fn want(&self, session: u64, muted: bool, video_on: bool, devices: DeviceChoices) {
        let old = self.lock().replace(Slot {
            session,
            muted,
            video_on,
            devices,
            test_color: None,
            call: None,
        });
        drop(old);
    }

    /// Uses this device in `session`'s call from now on.
    pub fn set_device(&self, session: u64, kind: DeviceKind, id: Option<String>) {
        let mut state = self.lock();
        let Some(slot) = state.as_mut().filter(|s| s.session == session) else {
            return;
        };
        slot.devices.set(kind, id);
        let Some(call) = &slot.call else {
            return;
        };
        match kind {
            DeviceKind::Camera => {
                call.set_video_input(video_input(slot.test_color, slot.devices.camera.as_deref()));
            }
            // Test calls have no sound in or out.
            DeviceKind::Microphone if slot.test_color.is_none() => {
                call.set_audio_input(AudioInput::Microphone {
                    device: slot.devices.microphone.clone(),
                });
            }
            DeviceKind::Speakers if slot.test_color.is_none() => {
                call.set_speakers(slot.devices.speakers.clone());
            }
            DeviceKind::Microphone | DeviceKind::Speakers => {}
        }
    }

    /// Leaves `session`'s call, if it is the current one.
    pub fn leave(&self, session: u64) {
        let mut state = self.lock();
        if state.as_ref().is_some_and(|s| s.session == session) {
            state.take();
        }
    }

    /// Leaves whatever call is running.
    pub fn leave_any(&self) {
        self.lock().take();
    }

    pub fn set_muted(&self, session: u64, muted: bool) {
        if let Some(slot) = self.lock().as_mut().filter(|s| s.session == session) {
            slot.muted = muted;
            if let Some(call) = &slot.call {
                call.set_muted(muted);
            }
        }
    }

    pub fn set_video(&self, session: u64, on: bool) {
        if let Some(slot) = self.lock().as_mut().filter(|s| s.session == session) {
            slot.video_on = on;
            if let Some(call) = &slot.call {
                call.set_video(on);
            }
        }
    }

    /// Starts the call for `session` with `start`, unless it was left meanwhile. `start` gets
    /// the wanted (muted, video on, devices); `test_color` is the test pattern's, when it sends
    /// one.
    pub fn start(
        &self,
        session: u64,
        test_color: Option<[u8; 3]>,
        start: impl FnOnce(bool, bool, &DeviceChoices) -> Call,
    ) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.as_mut().filter(|s| s.session == session) else {
            return false;
        };
        slot.test_color = test_color;
        slot.call = Some(start(slot.muted, slot.video_on, &slot.devices));
        true
    }

    /// Forgets `session`'s call after it ended by itself.
    pub fn ended(&self, session: u64) {
        self.leave(session);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Slot>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the core hears of a call event.
pub(crate) fn report(event: CallEvent) -> DropInReport {
    match event {
        CallEvent::Joined { .. } => DropInReport::Joined,
        CallEvent::Connected => DropInReport::Connected,
        CallEvent::Participants(people) => DropInReport::Peers(
            people
                .into_iter()
                .map(|p| DropInPeer {
                    id: p.id.0,
                    user: p.identity.parse::<UserId>().ok(),
                    name: p.name,
                    muted: p.audio_muted,
                    video_on: p.video_on,
                })
                .collect(),
        ),
        CallEvent::Speaking(who) => {
            DropInReport::Speaking(who.into_iter().map(|p| p.map(|p| p.0)).collect())
        }
        CallEvent::DeviceProblem(problem) => DropInReport::DeviceProblem(problem),
        CallEvent::Ended(reason) => DropInReport::Ended(reason),
    }
}

/// The test pattern's color for a join: varied by the token (which names the person), so two
/// test clients look different.
fn pattern_color(token: &str) -> [u8; 3] {
    const COLORS: [[u8; 3]; 6] = [
        [180, 110, 40],
        [60, 150, 60],
        [50, 70, 190],
        [150, 60, 150],
        [40, 160, 170],
        [170, 150, 40],
    ];
    // The claims (room, person, expiry) sit between the token's dots.
    let claims = token.split('.').nth(1).unwrap_or(token);
    let seed = claims.bytes().fold(0usize, |hash, b| {
        hash.wrapping_mul(31).wrapping_add(usize::from(b))
    });
    COLORS[seed % COLORS.len()]
}

impl crate::runtime::Inner {
    pub(crate) async fn join_drop_in(
        self: std::sync::Arc<Self>,
        session: u64,
        conversation: ConversationId,
    ) {
        let path = paths::drop_in(conversation);
        let joined = self
            .api
            .post_empty::<lazy_protocol::JoinDropInResponse>(&path)
            .await;
        let joined = match joined {
            Ok(joined) => joined,
            Err(e) => {
                self.drop_in.ended(session);
                self.emit(Input::DropInReport {
                    session,
                    report: DropInReport::Ended(Some(e.message())),
                })
                .await;
                return;
            }
        };
        let media = self.config.drop_in_media;
        let frames = self.config.frames.clone();
        let (events, mut reports) = tokio::sync::mpsc::unbounded_channel();
        let handle = self.handle.clone();
        let test_color = (media == DropInMedia::TestPattern).then(|| pattern_color(&joined.token));
        let started = self
            .drop_in
            .start(session, test_color, |muted, video_on, devices| {
                let mut config = CallConfig::new(joined.url, joined.token, frames);
                config.muted = muted;
                config.video_on = video_on;
                config.video = video_input(test_color, devices.camera.as_deref());
                if test_color.is_some() {
                    config.audio = AudioInput::None;
                    config.speakers = false;
                } else {
                    config.audio = AudioInput::Microphone {
                        device: devices.microphone.clone(),
                    };
                    config.speaker_device = devices.speakers.clone();
                }
                Call::start(config, &handle, move |event| {
                    events.send(event).ok();
                })
            });
        if !started {
            return;
        }
        while let Some(event) = reports.recv().await {
            let ended = matches!(event, CallEvent::Ended(_));
            if ended {
                self.drop_in.ended(session);
            }
            self.emit(Input::DropInReport {
                session,
                report: report(event),
            })
            .await;
            if ended {
                break;
            }
        }
    }
}
