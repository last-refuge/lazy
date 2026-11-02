//! A room on a real SFU in this process, with two clients sending test patterns and tones over
//! loopback UDP: each sees and hears the other, and leaving, muting, and turning video off reach
//! the other side.

use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rtc_audio::AudioInput;
use rtc_client::{Call, CallConfig, CallEvent};
use rtc_proto::{Claims, ParticipantId, token};
use rtc_sfu::{Sfu, SfuConfig};
use rtc_video::source::VideoInput;
use rtc_video::{FrameStore, TileKey};

const SECRET: &[u8] = b"a test secret that is long enough!!";

async fn sfu() -> String {
    let sfu = Sfu::start(SfuConfig {
        secret: SECRET.to_vec(),
        udp_bind: "127.0.0.1:0".parse().unwrap(),
        public_ip: None,
        max_per_room: 3,
    })
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, sfu.router()).await.unwrap() });
    format!("ws://{addr}/rtc")
}

fn token_for(room: &str, name: &str) -> String {
    let exp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;
    token::sign(
        &Claims {
            room: room.into(),
            identity: format!("id-{name}"),
            name: name.into(),
            exp,
        },
        SECRET,
    )
    .unwrap()
}

struct Person {
    call: Call,
    frames: FrameStore,
    events: mpsc::Receiver<CallEvent>,
    seen: Vec<CallEvent>,
}

impl Person {
    fn join(url: &str, room: &str, name: &str, color: [u8; 3], hz: f32) -> Person {
        let frames = FrameStore::new();
        let mut config = CallConfig::new(url, token_for(room, name), frames.clone());
        config.video = VideoInput::TestPattern { color };
        config.audio = AudioInput::Tone { hz };
        config.speakers = false;
        let (sender, events) = mpsc::channel();
        let call = Call::start(config, &tokio::runtime::Handle::current(), move |event| {
            sender.send(event).ok();
        });
        Person {
            call,
            frames,
            events,
            seen: Vec::new(),
        }
    }

    /// Waits until `check` holds for what has been seen so far (and the frames).
    async fn until(&mut self, what: &str, check: impl Fn(&Person) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while let Ok(event) = self.events.try_recv() {
                self.seen.push(event);
            }
            if check(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                self.seen
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn you(&self) -> ParticipantId {
        self.seen
            .iter()
            .find_map(|e| match e {
                CallEvent::Joined { you } => Some(*you),
                _ => None,
            })
            .expect("joined")
    }

    fn others(&self) -> Vec<rtc_proto::Participant> {
        self.seen
            .iter()
            .rev()
            .find_map(|e| match e {
                CallEvent::Participants(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

fn close(a: [u8; 3], b: [u8; 3]) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 30)
}

/// The background color of someone's picture (the corners, away from the moving bar most of the
/// time; either corner will do).
fn shows(frames: &FrameStore, who: ParticipantId, color: [u8; 3]) -> bool {
    frames.get(TileKey::Remote(who.0)).is_some_and(|tile| {
        let f = &tile.frame;
        close(f.pixel(2, f.height / 2), color) || close(f.pixel(f.width - 3, f.height / 2), color)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_people_see_and_hear_each_other() {
    let url = sfu().await;
    let red = [30, 40, 200];
    let green = [40, 180, 60];
    let mut ada = Person::join(&url, "room", "Ada", red, 440.0);
    ada.until("ada joined", |p| {
        p.seen.iter().any(|e| matches!(e, CallEvent::Joined { .. }))
    })
    .await;
    let mut bo = Person::join(&url, "room", "Bo", green, 660.0);
    bo.until("bo connected", |p| p.seen.contains(&CallEvent::Connected))
        .await;
    ada.until("ada connected", |p| p.seen.contains(&CallEvent::Connected))
        .await;
    let (a, b) = (ada.you(), bo.you());

    // Each knows the other.
    ada.until("ada sees bo in the list", |p| {
        p.others().iter().any(|o| o.id == b && o.name == "Bo")
    })
    .await;
    assert!(
        bo.others()
            .iter()
            .any(|o| o.id == a && o.name == "Ada" && o.video_on)
    );

    // Each sees the other's picture, in the other's color, and their own preview.
    ada.until("ada sees bo", |p| shows(&p.frames, b, green))
        .await;
    bo.until("bo sees ada", |p| shows(&p.frames, a, red)).await;
    assert!(ada.frames.get(TileKey::Local).is_some());

    // Each hears the other (the tone decodes loud enough to count as speaking).
    ada.until("ada hears bo", |p| {
        p.seen
            .iter()
            .any(|e| matches!(e, CallEvent::Speaking(s) if s.contains(&Some(b))))
    })
    .await;
    bo.until("bo hears ada", |p| {
        p.seen
            .iter()
            .any(|e| matches!(e, CallEvent::Speaking(s) if s.contains(&Some(a))))
    })
    .await;

    // Bo mutes and turns video off: Ada's list says so, and Bo's picture goes.
    bo.call.set_muted(true);
    bo.call.set_video(false);
    ada.until("bo's camera off", |p| {
        p.others()
            .iter()
            .any(|o| o.id == b && o.audio_muted && !o.video_on)
            && p.frames.get(TileKey::Remote(b.0)).is_none()
    })
    .await;
    // And back on: the picture returns.
    bo.call.set_video(true);
    ada.until("bo's camera back", |p| shows(&p.frames, b, green))
        .await;

    // Bo leaves: Ada's list empties, and Bo's call ends cleanly.
    bo.call.leave();
    bo.until("bo ended", |p| p.seen.contains(&CallEvent::Ended(None)))
        .await;
    bo.until("bo's tiles cleared", |p| p.frames.keys().is_empty())
        .await;
    ada.until("bo gone", |p| {
        p.others().is_empty() && p.frames.get(TileKey::Remote(b.0)).is_none()
    })
    .await;
    drop(ada);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bad_token_or_a_full_room_is_refused() {
    let url = sfu().await;
    let frames = FrameStore::new();
    let mut config = CallConfig::new(&url, "rtc1.forged.token", frames);
    config.video = VideoInput::TestPattern { color: [0, 0, 0] };
    config.audio = AudioInput::None;
    config.speakers = false;
    let (sender, events) = mpsc::channel();
    let _call = Call::start(config, &tokio::runtime::Handle::current(), move |e| {
        sender.send(e).ok();
    });
    let ended = tokio::task::spawn_blocking(move || {
        loop {
            match events.recv_timeout(Duration::from_secs(10)).unwrap() {
                CallEvent::Ended(reason) => return reason,
                _ => continue,
            }
        }
    })
    .await
    .unwrap();
    assert!(ended.unwrap().contains("malformed"));

    // The room holds three.
    let mut people: Vec<Person> = (0..3)
        .map(|i| Person::join(&url, "small", &format!("p{i}"), [0, 0, 0], 300.0))
        .collect();
    for p in &mut people {
        p.until("joined", |p| {
            p.seen.iter().any(|e| matches!(e, CallEvent::Joined { .. }))
        })
        .await;
    }
    let mut fourth = Person::join(&url, "small", "p3", [0, 0, 0], 300.0);
    fourth
        .until("refused", |p| {
            p.seen
                .iter()
                .any(|e| matches!(e, CallEvent::Ended(Some(m)) if m.contains("full")))
        })
        .await;
    // Another room is separate.
    let mut elsewhere = Person::join(&url, "other", "q", [0, 0, 0], 300.0);
    elsewhere
        .until("joined elsewhere", |p| {
            p.seen.iter().any(|e| matches!(e, CallEvent::Joined { .. }))
        })
        .await;
    assert!(elsewhere.others().is_empty());
}

/// Who the latest speaking report names.
fn speaking_now(p: &Person) -> Vec<Option<ParticipantId>> {
    p.seen
        .iter()
        .rev()
        .find_map(|e| match e {
            CallEvent::Speaking(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_camera_and_microphone_mid_call_reaches_the_other_side() {
    let url = sfu().await;
    let red = [30, 40, 200];
    let blue = [200, 60, 30];
    let mut ada = Person::join(&url, "switch", "Ada", red, 440.0);
    ada.until("ada joined", |p| {
        p.seen.iter().any(|e| matches!(e, CallEvent::Joined { .. }))
    })
    .await;
    let mut bo = Person::join(&url, "switch", "Bo", [40, 180, 60], 660.0);
    bo.until("bo connected", |p| p.seen.contains(&CallEvent::Connected))
        .await;
    let a = ada.you();
    bo.until("bo sees ada in red", |p| shows(&p.frames, a, red))
        .await;
    bo.until("bo hears ada", |p| speaking_now(p).contains(&Some(a)))
        .await;

    // Another "camera": Bo sees the new picture, and Ada's own preview follows.
    ada.call
        .set_video_input(VideoInput::TestPattern { color: blue });
    bo.until("bo sees ada in blue", |p| shows(&p.frames, a, blue))
        .await;
    ada.until("ada's preview is blue", |p| {
        p.frames.get(TileKey::Local).is_some_and(|tile| {
            let f = &tile.frame;
            close(f.pixel(2, f.height / 2), blue) || close(f.pixel(f.width - 3, f.height / 2), blue)
        })
    })
    .await;

    // Another "microphone": silence, then a tone again; Bo hears the change.
    ada.call.set_audio_input(AudioInput::None);
    bo.until("ada went quiet", |p| !speaking_now(p).contains(&Some(a)))
        .await;
    ada.call.set_audio_input(AudioInput::Tone { hz: 330.0 });
    bo.until("bo hears ada again", |p| speaking_now(p).contains(&Some(a)))
        .await;

    // Speakers switch without trouble even when the call has none (nothing happens).
    ada.call.set_speakers(Some("not a device".into()));
    ada.call
        .set_video_input(VideoInput::TestPattern { color: red });
    bo.until("bo sees ada in red again", |p| shows(&p.frames, a, red))
        .await;
    assert!(
        !ada.seen
            .iter()
            .any(|e| matches!(e, CallEvent::DeviceProblem(_))),
        "{:?}",
        ada.seen
    );
}
