//! Microphones and speakers through cpal. Each stream lives on its own thread (some systems tie a
//! stream to the thread that made it) until its [`StopOnDrop`] is dropped.
//!
//! On Linux, cpal's default host is PulseAudio when a server answers (PipeWire desktops run one):
//! its devices are the ones the system's sound settings show, by name. Without one it is ALSA.

use std::collections::VecDeque;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::{
    AudioDevice, AudioError, CAPTURE_QUEUE, Capture, FRAME_SAMPLES, Mixer, Playback, Resampler,
    SAMPLE_RATE, StopOnDrop, offer,
};

fn device_error(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// The inputs (`input`) or outputs to choose from. PulseAudio's monitor sources (what an output
/// plays, as an input) are not microphones and are left out.
pub(crate) fn list(input: bool) -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let devices = if input {
        host.input_devices().map(|d| d.collect::<Vec<_>>())
    } else {
        host.output_devices().map(|d| d.collect::<Vec<_>>())
    };
    let devices = match devices {
        Ok(devices) => devices,
        Err(e) => {
            tracing::warn!(error = %e, input, "could not list audio devices");
            return Vec::new();
        }
    };
    let mut listed: Vec<AudioDevice> = Vec::new();
    for device in devices {
        let (Ok(id), Ok(description)) = (device.id(), device.description()) else {
            continue;
        };
        let id = id.to_string();
        // ALSA's "null" device discards or makes silence: never what anyone means.
        if (input && id.ends_with(".monitor"))
            || id == "alsa:null"
            || listed.iter().any(|d| d.id == id)
        {
            continue;
        }
        listed.push(AudioDevice {
            id,
            name: description.name().to_owned(),
        });
    }
    listed
}

/// The device with this id when it is there, otherwise the default input or output.
fn find(host: &cpal::Host, id: Option<&str>, input: bool) -> Option<cpal::Device> {
    let chosen = id
        .and_then(|id| id.parse::<cpal::DeviceId>().ok())
        .and_then(|id| host.device_by_id(&id));
    if chosen.is_none()
        && let Some(id) = id
    {
        tracing::info!(
            device = id,
            input,
            "chosen audio device not found; using the default"
        );
    }
    chosen.or_else(|| {
        if input {
            host.default_input_device()
        } else {
            host.default_output_device()
        }
    })
}

/// Runs `open` on a new thread, keeps what it opened there until stopped, and reports whether it
/// opened.
fn on_own_thread<S: 'static>(
    name: &str,
    open: impl FnOnce() -> Result<S, AudioError> + Send + 'static,
) -> Result<StopOnDrop, AudioError> {
    let (opened_sender, opened) = sync_channel(1);
    let (stop, stopped) = sync_channel::<()>(0);
    thread::Builder::new()
        .name(name.into())
        .spawn(move || match open() {
            Ok(stream) => {
                opened_sender.send(Ok(())).ok();
                // Blocks until the StopOnDrop is dropped.
                stopped.recv().ok();
                drop(stream);
            }
            Err(e) => {
                opened_sender.send(Err(e)).ok();
            }
        })
        .map_err(device_error)?;
    opened
        .recv()
        .map_err(|_| AudioError::Device("the audio thread ended".into()))??;
    Ok(StopOnDrop(Some(stop)))
}

pub(crate) fn microphone(id: Option<String>) -> Result<Capture, AudioError> {
    let (frames_out, frames) = sync_channel(CAPTURE_QUEUE);
    let stop = on_own_thread("rtc-microphone", move || {
        let device = find(&cpal::default_host(), id.as_deref(), true).ok_or(AudioError::NoInput)?;
        let supported = device.default_input_config().map_err(device_error)?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        tracing::info!(?config, ?format, "microphone open");
        let stream = match format {
            SampleFormat::F32 => input_stream::<f32>(&device, &config, frames_out),
            SampleFormat::I16 => input_stream::<i16>(&device, &config, frames_out),
            SampleFormat::U16 => input_stream::<u16>(&device, &config, frames_out),
            SampleFormat::I32 => input_stream::<i32>(&device, &config, frames_out),
            other => Err(AudioError::Device(format!("unsupported format {other}"))),
        }?;
        stream.play().map_err(device_error)?;
        Ok(stream)
    })?;
    Ok(Capture {
        frames,
        _stop: stop,
    })
}

fn input_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    frames_out: SyncSender<Vec<i16>>,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let mut resampler = Resampler::new(config.sample_rate, SAMPLE_RATE);
    let mut mono = Vec::new();
    let mut resampled = Vec::new();
    let mut pending: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                mono.clear();
                for chunk in data.chunks(channels) {
                    let sum: f32 = chunk.iter().map(|&s| s.to_sample::<f32>()).sum();
                    mono.push(sum / chunk.len() as f32);
                }
                resampled.clear();
                resampler.process(&mono, &mut resampled);
                pending.extend(
                    resampled
                        .iter()
                        .map(|&s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16),
                );
                while pending.len() >= FRAME_SAMPLES {
                    let frame: Vec<i16> = pending.drain(..FRAME_SAMPLES).collect();
                    offer(&frames_out, frame);
                }
            },
            |e| tracing::warn!("microphone: {e}"),
            None,
        )
        .map_err(device_error)
}

pub(crate) fn speakers(mixer: Mixer, id: Option<String>) -> Result<Playback, AudioError> {
    let stop = on_own_thread("rtc-speakers", move || {
        let device =
            find(&cpal::default_host(), id.as_deref(), false).ok_or(AudioError::NoOutput)?;
        let supported = device.default_output_config().map_err(device_error)?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        tracing::info!(?config, ?format, "speakers open");
        let stream = match format {
            SampleFormat::F32 => output_stream::<f32>(&device, &config, mixer),
            SampleFormat::I16 => output_stream::<i16>(&device, &config, mixer),
            SampleFormat::U16 => output_stream::<u16>(&device, &config, mixer),
            SampleFormat::I32 => output_stream::<i32>(&device, &config, mixer),
            other => Err(AudioError::Device(format!("unsupported format {other}"))),
        }?;
        stream.play().map_err(device_error)?;
        Ok(stream)
    })?;
    Ok(Playback { _stop: stop })
}

fn output_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mixer: Mixer,
) -> Result<cpal::Stream, AudioError>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
{
    let channels = usize::from(config.channels.max(1));
    let mut resampler = Resampler::new(SAMPLE_RATE, config.sample_rate);
    let mut ready: VecDeque<f32> = VecDeque::new();
    let mut chunk = vec![0i16; FRAME_SAMPLES / 2];
    let mut floats = Vec::with_capacity(FRAME_SAMPLES / 2);
    let mut converted = Vec::new();
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let wanted = data.len() / channels;
                while ready.len() < wanted {
                    mixer.fill(&mut chunk);
                    floats.clear();
                    floats.extend(chunk.iter().map(|&s| f32::from(s) / f32::from(i16::MAX)));
                    converted.clear();
                    resampler.process(&floats, &mut converted);
                    ready.extend(converted.iter().copied());
                }
                for frame in data.chunks_mut(channels) {
                    let sample = T::from_sample(ready.pop_front().unwrap_or(0.0));
                    frame.fill(sample);
                }
            },
            |e| tracing::warn!("speakers: {e}"),
            None,
        )
        .map_err(device_error)
}
