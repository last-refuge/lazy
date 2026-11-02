//! One call: the signaling socket, the str0m connection, and the UDP socket, driven by one task.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rtc_audio::opus::OpusDecoder;
use rtc_audio::{Mixer, Playback, is_speaking, level};
use rtc_proto::{
    ClientMessage, Participant, ParticipantId, SIGNAL_VERSION, ServerMessage, TrackInfo, TrackKind,
};
use str0m::change::{SdpAnswer, SdpOffer};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid, Pt};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::media::{self, Local, Notice};
use crate::{CallConfig, CallEvent, Command, Events};

const CONNECT_WITHIN: Duration = Duration::from_secs(15);
/// How long someone counts as speaking after their last loud frame.
const SPEAKING_HOLD: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
enum Ended {
    #[error("{0}")]
    Failed(String),
}

fn failed(message: impl Into<String>) -> Ended {
    Ended::Failed(message.into())
}

pub(crate) async fn run(
    config: CallConfig,
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: Events,
) {
    let frames = config.frames.clone();
    let result = session(config, &mut commands, &events).await;
    frames.clear();
    match result {
        Ok(()) => events(CallEvent::Ended(None)),
        Err(e) => {
            tracing::info!("call ended: {e}");
            events(CallEvent::Ended(Some(e.to_string())))
        }
    }
}

/// The local address to offer: the one this machine would use to reach the SFU's host.
async fn local_ip(url: &str) -> IpAddr {
    let fallback = IpAddr::from([127, 0, 0, 1]);
    let Ok(request) = url.into_client_request() else {
        return fallback;
    };
    let uri = request.uri();
    let Some(host) = uri.host() else {
        return fallback;
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = uri.port_u16().unwrap_or(443);
    let Ok(mut addrs) = tokio::net::lookup_host((host.as_str(), port)).await else {
        return fallback;
    };
    let Some(target) = addrs.next() else {
        return fallback;
    };
    let bind: SocketAddr = if target.is_ipv4() {
        "0.0.0.0:0".parse().expect("an address")
    } else {
        "[::]:0".parse().expect("an address")
    };
    std::net::UdpSocket::bind(bind)
        .and_then(|s| {
            s.connect(target)?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(fallback)
}

struct Remote {
    /// Media lines the SFU sends on, and whose they are.
    tracks: HashMap<Mid, TrackInfo>,
    audio_decoders: HashMap<ParticipantId, OpusDecoder>,
    video_decoders: HashMap<ParticipantId, std_mpsc::SyncSender<std::sync::Arc<[u8]>>>,
    participants: Vec<Participant>,
    last_keyframe_ask: HashMap<Mid, Instant>,
}

struct Sending {
    audio_mid: Mid,
    video_mid: Mid,
    audio_pt: Option<Pt>,
    video_pt: Option<Pt>,
    started: Instant,
    audio_frames: u64,
}

fn pick_pts(rtc: &mut Rtc, sending: &mut Sending) {
    if let Some(writer) = rtc.writer(sending.audio_mid) {
        sending.audio_pt = writer
            .payload_params()
            .find(|p| p.spec().codec == Codec::Opus)
            .map(|p| p.pt());
    }
    if let Some(writer) = rtc.writer(sending.video_mid) {
        let h264: Vec<_> = writer
            .payload_params()
            .filter(|p| p.spec().codec == Codec::H264)
            .collect();
        sending.video_pt = h264
            .iter()
            .find(|p| p.spec().format.packetization_mode == Some(1))
            .or(h264.first())
            .map(|p| p.pt());
    }
}

async fn session(
    config: CallConfig,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    events: &Events,
) -> Result<(), Ended> {
    str0m::crypto::from_feature_flags().install_process_default();

    let (ws, _) = tokio::time::timeout(
        CONNECT_WITHIN,
        tokio_tungstenite::connect_async(config.url.as_str()),
    )
    .await
    .map_err(|_| failed("the call server did not answer"))?
    .map_err(|e| failed(format!("could not reach the call server: {e}")))?;
    let (mut ws_out, mut ws_in) = ws.split();

    let ip = local_ip(&config.url).await;
    let bind: SocketAddr = if ip.is_ipv4() {
        "0.0.0.0:0".parse().expect("an address")
    } else {
        "[::]:0".parse().expect("an address")
    };
    let udp = UdpSocket::bind(bind)
        .await
        .map_err(|e| failed(format!("no network for media: {e}")))?;
    let local_addr = SocketAddr::new(
        ip,
        udp.local_addr()
            .map_err(|e| failed(format!("no network for media: {e}")))?
            .port(),
    );

    let mut rtc = Rtc::builder()
        .clear_codecs()
        .enable_opus(true, false)
        .enable_h264(true)
        .build(Instant::now());
    let candidate = Candidate::host(local_addr, "udp").map_err(|e| failed(e.to_string()))?;
    rtc.add_local_candidate(candidate);
    let mut change = rtc.sdp_api();
    let stream = "local".to_owned();
    let audio_mid = change.add_media(
        MediaKind::Audio,
        Direction::SendOnly,
        Some(stream.clone()),
        Some("audio".into()),
        None,
    );
    let video_mid = change.add_media(
        MediaKind::Video,
        Direction::SendOnly,
        Some(stream),
        Some("video".into()),
        None,
    );
    let (offer, pending) = change.apply().ok_or_else(|| failed("nothing to offer"))?;

    let local = Local::new(config.video_on, config.muted);
    let join = ClientMessage::Join {
        version: SIGNAL_VERSION,
        token: config.token.clone(),
        offer: offer.to_sdp_string(),
        audio_muted: config.muted,
        video_on: config.video_on,
    };
    ws_out
        .send(Message::Text(join.to_json().into()))
        .await
        .map_err(|e| failed(format!("the call server closed: {e}")))?;

    let joined = tokio::time::timeout(CONNECT_WITHIN, async {
        while let Some(message) = ws_in.next().await {
            let Ok(Message::Text(text)) = message else {
                continue;
            };
            match serde_json::from_str::<ServerMessage>(&text) {
                Ok(message @ ServerMessage::Joined { .. }) => return Ok(message),
                Ok(ServerMessage::Error { message, .. }) => return Err(failed(message)),
                _ => continue,
            }
        }
        Err(failed("the call server closed the connection"))
    })
    .await
    .map_err(|_| failed("the call server did not let us in"))??;
    let ServerMessage::Joined {
        you,
        answer,
        participants,
    } = joined
    else {
        unreachable!("matched above");
    };
    let answer =
        SdpAnswer::from_sdp_string(&answer).map_err(|e| failed(format!("bad answer: {e}")))?;
    rtc.sdp_api()
        .accept_answer(pending, answer)
        .map_err(|e| failed(format!("bad answer: {e}")))?;
    events(CallEvent::Joined { you });
    events(CallEvent::Participants(participants.clone()));

    let mut sending = Sending {
        audio_mid,
        video_mid,
        audio_pt: None,
        video_pt: None,
        started: Instant::now(),
        audio_frames: 0,
    };
    pick_pts(&mut rtc, &mut sending);

    // Local media.
    let (notice_sender, mut notices) = mpsc::unbounded_channel();
    let mut video_out = media::local_video(
        config.video.clone(),
        local.clone(),
        config.frames.clone(),
        notice_sender.clone(),
    );
    let audio_input = config.audio.clone();
    let (audio_local, audio_notices) = (local.clone(), notice_sender.clone());
    let mut audio_out = tokio::task::spawn_blocking(move || {
        media::local_audio(audio_input, audio_local, audio_notices)
    })
    .await
    .ok()
    .flatten();
    let mixer = Mixer::new();
    let mut playback = if config.speakers {
        start_playback(&mixer, config.speaker_device.clone(), events).await
    } else {
        None
    };
    let _stop = StopLocal(local.clone());

    let mut remote = Remote {
        tracks: HashMap::new(),
        audio_decoders: HashMap::new(),
        video_decoders: HashMap::new(),
        participants,
        last_keyframe_ask: HashMap::new(),
    };
    let mut speaking_since: HashMap<Option<ParticipantId>, Instant> = HashMap::new();
    let mut speaking: Vec<Option<ParticipantId>> = Vec::new();
    let mut speaking_tick = tokio::time::interval(Duration::from_millis(200));
    let mut connected = false;
    let mut buf = vec![0u8; 2000];
    let mut next_timeout;
    let connect_by = Instant::now() + CONNECT_WITHIN;

    loop {
        // Everything str0m has to say, until it next needs time to pass.
        loop {
            match rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    next_timeout = t;
                    break;
                }
                Ok(Output::Transmit(transmit)) => {
                    udp.try_send_to(&transmit.contents, transmit.destination)
                        .ok();
                }
                Ok(Output::Event(event)) => match event {
                    Event::IceConnectionStateChange(state) => match state {
                        IceConnectionState::Connected | IceConnectionState::Completed => {
                            if !connected {
                                connected = true;
                                events(CallEvent::Connected);
                            }
                        }
                        IceConnectionState::Disconnected => {
                            return Err(failed("the connection to the call server was lost"));
                        }
                        _ => {}
                    },
                    Event::MediaData(data) => {
                        let Some(track) = remote.tracks.get(&data.mid) else {
                            continue;
                        };
                        let who = track.participant;
                        match track.kind {
                            TrackKind::Audio => {
                                let decoder = match remote.audio_decoders.entry(who) {
                                    std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                                    std::collections::hash_map::Entry::Vacant(e) => {
                                        match OpusDecoder::new() {
                                            Ok(d) => e.insert(d),
                                            Err(_) => continue,
                                        }
                                    }
                                };
                                if let Ok(samples) = decoder.decode(&data.data) {
                                    if is_speaking(level(&samples)) {
                                        speaking_since.insert(Some(who), Instant::now());
                                    }
                                    mixer.push(who.0, &samples);
                                }
                            }
                            TrackKind::Video => {
                                if !data.contiguous {
                                    ask_keyframe(&mut rtc, &mut remote, data.mid);
                                }
                                if let Some(decoder) = remote.video_decoders.get(&who) {
                                    decoder.try_send(data.data).ok();
                                }
                            }
                        }
                    }
                    Event::KeyframeRequest(request) if request.mid == sending.video_mid => {
                        local.keyframe.store(true, Ordering::SeqCst);
                    }
                    _ => {}
                },
                Err(e) => return Err(failed(format!("media failed: {e}"))),
            }
        }
        if !rtc.is_alive() {
            return Err(failed("the connection to the call server was lost"));
        }
        if !connected && Instant::now() > connect_by {
            return Err(failed(
                "could not connect to the call server's media port (is UDP blocked?)",
            ));
        }

        let wait = next_timeout.saturating_duration_since(Instant::now());
        tokio::select! {
            received = udp.recv_from(&mut buf) => {
                if let Ok((n, source)) = received
                    && let Ok(contents) = buf[..n].try_into()
                {
                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            proto: Protocol::Udp,
                            source,
                            destination: local_addr,
                            contents,
                        },
                    );
                    if let Err(e) = rtc.handle_input(input) {
                        tracing::debug!("input refused: {e}");
                    }
                }
            }
            _ = tokio::time::sleep(wait) => {}
            message = ws_in.next() => {
                let text = match message {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_))) => continue,
                    _ => return Err(failed("the call server closed the connection")),
                };
                let Ok(message) = serde_json::from_str::<ServerMessage>(&text) else {
                    continue;
                };
                match message {
                    ServerMessage::Offer { sdp, tracks } => {
                        let offer = SdpOffer::from_sdp_string(&sdp)
                            .map_err(|e| failed(format!("bad offer: {e}")))?;
                        let answer = rtc
                            .sdp_api()
                            .accept_offer(offer)
                            .map_err(|e| failed(format!("bad offer: {e}")))?;
                        ws_out
                            .send(Message::Text(
                                ClientMessage::Answer { sdp: answer.to_sdp_string() }
                                    .to_json()
                                    .into(),
                            ))
                            .await
                            .map_err(|_| failed("the call server closed the connection"))?;
                        set_tracks(&mut remote, tracks, &config, &notice_sender, &mixer);
                    }
                    ServerMessage::ParticipantJoined { participant } => {
                        remote.participants.retain(|p| p.id != participant.id);
                        remote.participants.push(participant);
                        events(CallEvent::Participants(remote.participants.clone()));
                    }
                    ServerMessage::ParticipantChanged { participant } => {
                        let id = participant.id;
                        if participant.video_on {
                            let has_video = remote
                                .tracks
                                .values()
                                .any(|t| t.participant == id && t.kind == TrackKind::Video);
                            if has_video {
                                remote.video_decoders.entry(id).or_insert_with(|| {
                                    media::remote_video(id, config.frames.clone(), notice_sender.clone())
                                });
                            }
                        } else {
                            // The decoder clears the picture once it has finished what it has.
                            remote.video_decoders.remove(&id);
                            config.frames.remove(rtc_video::TileKey::Remote(id.0));
                        }
                        for p in &mut remote.participants {
                            if p.id == participant.id {
                                *p = participant.clone();
                            }
                        }
                        events(CallEvent::Participants(remote.participants.clone()));
                    }
                    ServerMessage::ParticipantLeft { id } => {
                        remote.participants.retain(|p| p.id != id);
                        remote.video_decoders.remove(&id);
                        remote.audio_decoders.remove(&id);
                        mixer.remove(id.0);
                        config.frames.remove(rtc_video::TileKey::Remote(id.0));
                        events(CallEvent::Participants(remote.participants.clone()));
                    }
                    ServerMessage::Error { message, .. } => return Err(failed(message)),
                    ServerMessage::Joined { .. } => {}
                }
            }
            command = commands.recv() => {
                let state = match command {
                    Some(Command::SetMuted(muted)) => {
                        local.muted.store(muted, Ordering::SeqCst);
                        true
                    }
                    Some(Command::SetVideo(on)) => {
                        local.video_on.store(on, Ordering::SeqCst);
                        true
                    }
                    Some(Command::SetVideoInput(input)) => {
                        local.switch_video(input);
                        false
                    }
                    Some(Command::SetAudioInput(input)) => {
                        // The old encoder thread ends (and closes its microphone) when its
                        // receiver is dropped here.
                        drop(audio_out.take());
                        let (audio_local, audio_notices) = (local.clone(), notice_sender.clone());
                        audio_out = tokio::task::spawn_blocking(move || {
                            media::local_audio(input, audio_local, audio_notices)
                        })
                        .await
                        .ok()
                        .flatten();
                        false
                    }
                    Some(Command::SetSpeakers(device)) => {
                        if config.speakers {
                            // One output at a time: the old one stops first.
                            drop(playback.take());
                            playback = start_playback(&mixer, device, events).await;
                        }
                        false
                    }
                    Some(Command::Leave) | None => {
                        ws_out.send(Message::Text(ClientMessage::Leave.to_json().into())).await.ok();
                        ws_out.close().await.ok();
                        return Ok(());
                    }
                };
                if state {
                    send_state(&mut ws_out, &local).await?;
                }
            }
            Some((encoded, captured)) = video_out.recv() => {
                if connected
                    && let Some(pt) = sending.video_pt
                    && let Some(writer) = rtc.writer(sending.video_mid)
                {
                    let ticks = captured.saturating_duration_since(sending.started).as_micros() as u64 * 9 / 100;
                    let time = MediaTime::new(ticks, Frequency::NINETY_KHZ);
                    if let Err(e) = writer.write(pt, captured, time, encoded.data) {
                        tracing::debug!("video write failed: {e}");
                    }
                }
            }
            Some((packet, loudness)) = recv_audio(&mut audio_out) => {
                let time = MediaTime::new(sending.audio_frames * 960, Frequency::FORTY_EIGHT_KHZ);
                sending.audio_frames += 1;
                if let Some(packet) = packet {
                    if is_speaking(loudness) {
                        speaking_since.insert(None, Instant::now());
                    }
                    if connected
                        && let Some(pt) = sending.audio_pt
                        && let Some(writer) = rtc.writer(sending.audio_mid)
                        && let Err(e) = writer.write(pt, Instant::now(), time, packet)
                    {
                        tracing::debug!("audio write failed: {e}");
                    }
                }
            }
            Some(notice) = notices.recv() => match notice {
                Notice::VideoFailed(message) => {
                    events(CallEvent::DeviceProblem(format!("Camera: {message}")));
                    send_state(&mut ws_out, &local).await?;
                }
                Notice::AudioFailed(message) => {
                    events(CallEvent::DeviceProblem(format!("Microphone: {message}")));
                }
                Notice::NeedKeyframe(who) => {
                    let mid = remote
                        .tracks
                        .iter()
                        .find(|(_, t)| t.participant == who && t.kind == TrackKind::Video)
                        .map(|(mid, _)| *mid);
                    if let Some(mid) = mid {
                        ask_keyframe(&mut rtc, &mut remote, mid);
                    }
                }
            },
            _ = speaking_tick.tick() => {
                let now = Instant::now();
                speaking_since.retain(|_, t| now.duration_since(*t) < SPEAKING_HOLD);
                let mut current: Vec<_> = speaking_since.keys().copied().collect();
                current.sort();
                if current != speaking {
                    speaking = current.clone();
                    events(CallEvent::Speaking(current));
                }
            }
        }
        if let Err(e) = rtc.handle_input(Input::Timeout(Instant::now())) {
            return Err(failed(format!("media failed: {e}")));
        }
    }
}

async fn recv_audio(audio: &mut Option<media::AudioOut>) -> Option<(Option<Vec<u8>>, f32)> {
    match audio {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

async fn send_state(ws_out: &mut WsSink, local: &Local) -> Result<(), Ended> {
    let state = ClientMessage::State {
        audio_muted: local.muted.load(Ordering::SeqCst),
        video_on: local.video_on.load(Ordering::SeqCst),
    };
    ws_out
        .send(Message::Text(state.to_json().into()))
        .await
        .map_err(|_| failed("the call server closed the connection"))
}

fn ask_keyframe(rtc: &mut Rtc, remote: &mut Remote, mid: Mid) {
    let now = Instant::now();
    if remote
        .last_keyframe_ask
        .get(&mid)
        .is_some_and(|t| now.duration_since(*t) < Duration::from_millis(500))
    {
        return;
    }
    if let Some(mut writer) = rtc.writer(mid)
        && writer
            .request_keyframe(None, KeyframeRequestKind::Pli)
            .is_ok()
    {
        remote.last_keyframe_ask.insert(mid, now);
    }
}

/// The SFU's new list of what it sends: start decoders for new video, stop the rest.
fn set_tracks(
    remote: &mut Remote,
    tracks: Vec<TrackInfo>,
    config: &CallConfig,
    notices: &mpsc::UnboundedSender<Notice>,
    mixer: &Mixer,
) {
    remote.tracks = tracks
        .into_iter()
        .map(|t| (Mid::from(t.mid.as_str()), t))
        .collect();
    let video: HashSet<ParticipantId> = remote
        .tracks
        .values()
        .filter(|t| t.kind == TrackKind::Video)
        .map(|t| t.participant)
        .collect();
    let audio: HashSet<ParticipantId> = remote
        .tracks
        .values()
        .filter(|t| t.kind == TrackKind::Audio)
        .map(|t| t.participant)
        .collect();
    remote.video_decoders.retain(|who, _| video.contains(who));
    for who in video {
        remote
            .video_decoders
            .entry(who)
            .or_insert_with(|| media::remote_video(who, config.frames.clone(), notices.clone()));
    }
    remote.audio_decoders.retain(|who, _| {
        let keep = audio.contains(who);
        if !keep {
            mixer.remove(who.0);
        }
        keep
    });
}

/// Stops the local threads when the session ends, however it ends.
/// Starts the speakers, or says why they could not start.
async fn start_playback(
    mixer: &Mixer,
    device: Option<String>,
    events: &Events,
) -> Option<Playback> {
    let playing = mixer.clone();
    match tokio::task::spawn_blocking(move || Playback::start(playing, device)).await {
        Ok(Ok(playback)) => Some(playback),
        Ok(Err(e)) => {
            events(CallEvent::DeviceProblem(format!("Speakers: {e}")));
            None
        }
        Err(_) => None,
    }
}

struct StopLocal(Local);

impl Drop for StopLocal {
    fn drop(&mut self) {
        self.0.stop();
    }
}
