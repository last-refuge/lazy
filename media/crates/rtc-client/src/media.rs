//! The threads around a call's connection: the local camera and microphone (read and encoded)
//! and each remote video track (decoded). They talk to the session through channels and flags,
//! never blocking it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rtc_audio::opus::OpusEncoder;
use rtc_audio::{AudioInput, level};
use rtc_proto::ParticipantId;
use rtc_video::codec::{Encoded, H264Decoder, H264Encoder, VideoDecoder, VideoEncoder};
use rtc_video::source::{VideoInput, VideoSource};
use rtc_video::{FrameStore, TileKey};
use tokio::sync::mpsc;

/// What the threads tell the session.
#[derive(Debug)]
pub(crate) enum Notice {
    /// The camera failed; video is off.
    VideoFailed(String),
    /// The microphone failed; nothing is sent.
    AudioFailed(String),
    /// A remote video decoder lost its place and needs a keyframe.
    NeedKeyframe(ParticipantId),
}

/// The camera's encoded frames, with when each was captured.
pub(crate) type VideoOut = mpsc::Receiver<(Encoded, Instant)>;
/// The microphone's encoded frames (`None` while muted) and their loudness.
pub(crate) type AudioOut = mpsc::Receiver<(Option<Vec<u8>>, f32)>;

/// Flags shared with the local threads.
#[derive(Clone)]
pub(crate) struct Local {
    pub video_on: Arc<AtomicBool>,
    pub muted: Arc<AtomicBool>,
    pub keyframe: Arc<AtomicBool>,
    pub stop: Arc<AtomicBool>,
    /// A video source to switch to, taken by the camera thread.
    pub next_video: Arc<Mutex<Option<VideoInput>>>,
}

impl Local {
    pub fn new(video_on: bool, muted: bool) -> Local {
        Local {
            video_on: Arc::new(AtomicBool::new(video_on)),
            muted: Arc::new(AtomicBool::new(muted)),
            keyframe: Arc::new(AtomicBool::new(true)),
            stop: Arc::new(AtomicBool::new(false)),
            next_video: Arc::new(Mutex::new(None)),
        }
    }

    /// Asks the camera thread to switch to `input`.
    pub fn switch_video(&self, input: VideoInput) {
        *self.next_video.lock().unwrap_or_else(|e| e.into_inner()) = Some(input);
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Reads and encodes the camera while video is on; shows the preview in `frames`.
pub(crate) fn local_video(
    mut input: VideoInput,
    local: Local,
    frames: FrameStore,
    notices: mpsc::UnboundedSender<Notice>,
) -> VideoOut {
    let (sender, receiver) = mpsc::channel(4);
    thread::Builder::new()
        .name("rtc-camera".into())
        .spawn(move || {
            let mut encoder = match H264Encoder::new() {
                Ok(encoder) => encoder,
                Err(e) => {
                    notices.send(Notice::VideoFailed(e.to_string())).ok();
                    return;
                }
            };
            let mut source: Option<Box<dyn VideoSource>> = None;
            while !local.stop.load(Ordering::SeqCst) {
                let next = local
                    .next_video
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                if let Some(next) = next {
                    // Close the old source before opening the new: a camera may allow only one.
                    input = next;
                    source = None;
                }
                if !local.video_on.load(Ordering::SeqCst) {
                    if source.take().is_some() {
                        frames.remove(TileKey::Local);
                    }
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                let reader = match &mut source {
                    Some(reader) => reader,
                    None => match input.open() {
                        Ok(opened) => {
                            local.keyframe.store(true, Ordering::SeqCst);
                            source.insert(opened)
                        }
                        Err(e) => {
                            local.video_on.store(false, Ordering::SeqCst);
                            notices.send(Notice::VideoFailed(e.to_string())).ok();
                            continue;
                        }
                    },
                };
                let frame = match reader.read() {
                    Ok(frame) => frame,
                    Err(e) => {
                        source = None;
                        frames.remove(TileKey::Local);
                        local.video_on.store(false, Ordering::SeqCst);
                        notices.send(Notice::VideoFailed(e.to_string())).ok();
                        continue;
                    }
                };
                if local.stop.load(Ordering::SeqCst) {
                    break;
                }
                let captured = Instant::now();
                frames.put(TileKey::Local, frame.mirrored());
                let keyframe = local.keyframe.swap(false, Ordering::SeqCst);
                match encoder.encode(&frame, keyframe) {
                    Ok(Some(encoded)) => {
                        if sender.try_send((encoded, captured)).is_err() && sender.is_closed() {
                            break;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("encoding failed: {e}"),
                }
            }
            frames.remove(TileKey::Local);
        })
        .expect("spawning the camera thread");
    receiver
}

/// Reads and encodes the microphone (or test tone). `None` if there is no input to read.
pub(crate) fn local_audio(
    input: AudioInput,
    local: Local,
    notices: mpsc::UnboundedSender<Notice>,
) -> Option<AudioOut> {
    let capture = match input.start() {
        Ok(Some(capture)) => capture,
        Ok(None) => return None,
        Err(e) => {
            notices.send(Notice::AudioFailed(e.to_string())).ok();
            return None;
        }
    };
    let (sender, receiver) = mpsc::channel(16);
    thread::Builder::new()
        .name("rtc-microphone-encoder".into())
        .spawn(move || {
            // The whole capture moves here: dropping it (its stop handle) stops the source.
            let capture = capture;
            let mut encoder = match OpusEncoder::new() {
                Ok(encoder) => encoder,
                Err(e) => {
                    notices.send(Notice::AudioFailed(e.to_string())).ok();
                    return;
                }
            };
            while !local.stop.load(Ordering::SeqCst) {
                let frame = match capture.frames.recv_timeout(Duration::from_millis(200)) {
                    Ok(frame) => frame,
                    Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                        notices
                            .send(Notice::AudioFailed("the microphone stopped".into()))
                            .ok();
                        return;
                    }
                };
                let loudness = level(&frame);
                let packet = if local.muted.load(Ordering::SeqCst) {
                    None
                } else {
                    match encoder.encode(&frame) {
                        Ok(packet) => Some(packet),
                        Err(e) => {
                            tracing::warn!("{e}");
                            None
                        }
                    }
                };
                if sender.try_send((packet, loudness)).is_err() && sender.is_closed() {
                    return;
                }
            }
        })
        .expect("spawning the microphone thread");
    Some(receiver)
}

/// Decodes one participant's video into `frames` until the returned sender is dropped.
pub(crate) fn remote_video(
    participant: ParticipantId,
    frames: FrameStore,
    notices: mpsc::UnboundedSender<Notice>,
) -> std_mpsc::SyncSender<Arc<[u8]>> {
    let (sender, receiver) = std_mpsc::sync_channel::<Arc<[u8]>>(30);
    thread::Builder::new()
        .name(format!("rtc-decode-{participant}"))
        .spawn(move || {
            let key = TileKey::Remote(participant.0);
            let mut decoder = match H264Decoder::new() {
                Ok(decoder) => decoder,
                Err(e) => {
                    tracing::warn!("{e}");
                    return;
                }
            };
            let mut last_ask = None::<Instant>;
            while let Ok(data) = receiver.recv() {
                match decoder.decode(&data) {
                    Ok(Some(frame)) => frames.put(key, frame),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(%participant, "decode failed: {e}");
                        if last_ask.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                            last_ask = Some(Instant::now());
                            notices.send(Notice::NeedKeyframe(participant)).ok();
                        }
                    }
                }
            }
            frames.remove(key);
        })
        .expect("spawning a decoder thread");
    sender
}
