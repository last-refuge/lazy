//! Audio for the rtc media library.
//!
//! Everything inside is 48 kHz mono, 16-bit, in 20 ms frames ([`FRAME_SAMPLES`]): what Opus
//! carries and what calls need. Devices are converted at the edges.
//!
//! - [`opus`]: the codec (libopus 1.3, transpiled to Rust, so nothing to link).
//! - [`AudioInput`]: the microphone, a test tone, or nothing, delivered as frames on a channel.
//! - [`Mixer`]: what everyone else says, each with a small buffer to ride out jitter, summed for
//!   the speakers ([`Playback`]).
//!
//! Not here yet (M2 in `llm-docs/DROP_INS.md`): echo cancellation, noise suppression, packet-loss
//! concealment, device choice. Until then, headphones.

pub mod opus;
mod resample;

#[cfg(feature = "devices")]
mod devices;

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub use resample::Resampler;

pub const SAMPLE_RATE: u32 = 48_000;
/// 20 ms.
pub const FRAME_SAMPLES: usize = 960;

/// One 20 ms frame of 48 kHz mono.
pub type AudioFrame = Vec<i16>;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no microphone was found")]
    NoInput,
    #[error("no speakers were found")]
    NoOutput,
    #[error("audio device: {0}")]
    Device(String),
    #[error("this build has no audio device support")]
    Unsupported,
    #[error("opus: {0}")]
    Codec(String),
}

/// Where a client's sound comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioInput {
    /// The system's default microphone.
    Microphone,
    /// A sine tone (for tests, and machines without a microphone).
    Tone { hz: f32 },
    /// Silence: nothing is sent.
    None,
}

/// A running source of frames. Dropping it stops the source.
pub struct Capture {
    pub frames: Receiver<AudioFrame>,
    _stop: StopOnDrop,
}

struct StopOnDrop(Option<SyncSender<()>>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        // Closing the channel tells the source's thread to stop.
        self.0.take();
    }
}

/// How many frames may wait between the source and the sender before the oldest are dropped.
const CAPTURE_QUEUE: usize = 10;

impl AudioInput {
    /// Starts the source; `None` for [`AudioInput::None`].
    pub fn start(&self) -> Result<Option<Capture>, AudioError> {
        match self {
            AudioInput::None => Ok(None),
            AudioInput::Tone { hz } => Ok(Some(tone(*hz))),
            AudioInput::Microphone => {
                #[cfg(feature = "devices")]
                {
                    devices::microphone().map(Some)
                }
                #[cfg(not(feature = "devices"))]
                {
                    Err(AudioError::Unsupported)
                }
            }
        }
    }
}

fn tone(hz: f32) -> Capture {
    let (frames_out, frames) = sync_channel(CAPTURE_QUEUE);
    let (stop, stopped) = sync_channel::<()>(0);
    thread::Builder::new()
        .name("rtc-tone".into())
        .spawn(move || {
            let mut phase = 0f32;
            let step = hz * std::f32::consts::TAU / SAMPLE_RATE as f32;
            let mut next = Instant::now();
            loop {
                match stopped.try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    _ => return,
                }
                let frame: AudioFrame = (0..FRAME_SAMPLES)
                    .map(|_| {
                        phase = (phase + step) % std::f32::consts::TAU;
                        (phase.sin() * 8_000.0) as i16
                    })
                    .collect();
                match frames_out.try_send(frame) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return,
                }
                next += Duration::from_millis(20);
                let now = Instant::now();
                if next > now {
                    thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
        })
        .expect("spawning the tone thread");
    Capture {
        frames,
        _stop: StopOnDrop(Some(stop)),
    }
}

/// Sends `frame` to the channel, dropping it if the reader is behind.
#[cfg(feature = "devices")]
fn offer(sender: &SyncSender<AudioFrame>, frame: AudioFrame) -> bool {
    !matches!(sender.try_send(frame), Err(TrySendError::Disconnected(_)))
}

/// The loudness of a frame, 0.0 (silence) to 1.0 (full scale), as root mean square.
pub fn level(frame: &[i16]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f64 = frame.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum / frame.len() as f64).sqrt() / f64::from(i16::MAX)) as f32
}

/// Whether a frame's level counts as someone speaking.
pub fn is_speaking(level: f32) -> bool {
    level > 0.02
}

/// How much a source buffers before it plays, and again after it runs dry.
const PREBUFFER: usize = FRAME_SAMPLES * 3;
/// More than this waiting is too much delay: the oldest is dropped.
const MAX_BUFFERED: usize = FRAME_SAMPLES * 10;

#[derive(Default)]
struct Source {
    samples: VecDeque<i16>,
    playing: bool,
}

/// What everyone else says, per source, summed on demand.
#[derive(Clone, Default)]
pub struct Mixer {
    sources: Arc<Mutex<HashMap<u64, Source>>>,
}

impl Mixer {
    pub fn new() -> Mixer {
        Mixer::default()
    }

    /// Adds `samples` (48 kHz mono) from `source`.
    pub fn push(&self, source: u64, samples: &[i16]) {
        let mut sources = self.sources.lock().expect("mixer lock");
        let entry = sources.entry(source).or_default();
        entry.samples.extend(samples);
        let excess = entry.samples.len().saturating_sub(MAX_BUFFERED);
        entry.samples.drain(..excess);
    }

    /// Forgets `source` (they left).
    pub fn remove(&self, source: u64) {
        self.sources.lock().expect("mixer lock").remove(&source);
    }

    /// Fills `out` (48 kHz mono) with the sum of every source that is playing.
    pub fn fill(&self, out: &mut [i16]) {
        let mut mixed = vec![0i32; out.len()];
        let mut sources = self.sources.lock().expect("mixer lock");
        for source in sources.values_mut() {
            if !source.playing {
                if source.samples.len() < PREBUFFER {
                    continue;
                }
                source.playing = true;
            }
            for slot in &mut mixed {
                match source.samples.pop_front() {
                    Some(sample) => *slot += i32::from(sample),
                    None => {
                        source.playing = false;
                        break;
                    }
                }
            }
        }
        for (o, m) in out.iter_mut().zip(mixed) {
            *o = m.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        }
    }
}

/// The speakers, playing a [`Mixer`]. Dropping it stops them.
pub struct Playback {
    _stop: StopOnDrop,
}

impl Playback {
    /// Plays `mixer` on the default output device.
    pub fn start(mixer: Mixer) -> Result<Playback, AudioError> {
        #[cfg(feature = "devices")]
        {
            devices::speakers(mixer)
        }
        #[cfg(not(feature = "devices"))]
        {
            let _ = mixer;
            Err(AudioError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tone_arrives_in_frames_and_stops_when_dropped() {
        let capture = AudioInput::Tone { hz: 440.0 }.start().unwrap().unwrap();
        let frame = capture.frames.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(frame.len(), FRAME_SAMPLES);
        assert!(is_speaking(level(&frame)));
        assert!(AudioInput::None.start().unwrap().is_none());
    }

    #[test]
    fn the_mixer_waits_for_a_little_buffer_then_sums_its_sources() {
        let mixer = Mixer::new();
        let mut out = vec![0; FRAME_SAMPLES];
        mixer.push(1, &vec![100; FRAME_SAMPLES]);
        mixer.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0), "still buffering");

        mixer.push(1, &vec![100; FRAME_SAMPLES * 2]);
        mixer.push(2, &vec![50; FRAME_SAMPLES * 3]);
        mixer.fill(&mut out);
        assert!(out.iter().all(|&s| s == 150));

        mixer.push(3, &vec![i16::MAX; FRAME_SAMPLES * 3]);
        mixer.fill(&mut out);
        assert!(out.iter().all(|&s| s == i16::MAX), "clipped, not wrapped");
    }

    #[test]
    fn the_mixer_drops_old_audio_rather_than_fall_behind() {
        let mixer = Mixer::new();
        mixer.push(1, &vec![1; MAX_BUFFERED]);
        mixer.push(1, &vec![2; FRAME_SAMPLES]);
        let mut out = vec![0; MAX_BUFFERED];
        mixer.fill(&mut out);
        assert_eq!(*out.last().unwrap(), 2);
    }
}
