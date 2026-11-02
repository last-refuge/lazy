//! H.264 (Constrained Baseline) through OpenH264, built from source, so no system library is
//! needed on any platform. Behind [`VideoEncoder`] and [`VideoDecoder`] so another codec (VP8,
//! AV1) can replace it without touching the rest.
//!
//! Licensing note: OpenH264's source is BSD-2-Clause; H.264 itself is covered by patents, and
//! Cisco pays the royalties only for its own prebuilt binaries. Built from source, as here, that
//! cover does not apply (see `llm-docs/DROP_INS.md`).

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode,
    UsageType,
};
use openh264::formats::{BgraSliceU8, YUVBuffer, YUVSource};

use crate::Frame;
use crate::source::FRAME_RATE;

#[derive(Debug, thiserror::Error)]
#[error("video codec: {0}")]
pub struct CodecError(String);

fn error(e: impl std::fmt::Display) -> CodecError {
    CodecError(e.to_string())
}

/// One compressed frame.
#[derive(Clone, Debug)]
pub struct Encoded {
    /// Annex B: NAL units each after a start code.
    pub data: Vec<u8>,
    pub keyframe: bool,
}

pub trait VideoEncoder: Send {
    /// Compresses `frame`; a keyframe if `keyframe` (someone asked for one) or the encoder
    /// decides. `None` when the encoder skipped the frame to keep to its bitrate.
    fn encode(&mut self, frame: &Frame, keyframe: bool) -> Result<Option<Encoded>, CodecError>;
}

pub trait VideoDecoder: Send {
    /// Decompresses one frame's data; `None` until the decoder has a picture (after a keyframe).
    fn decode(&mut self, data: &[u8]) -> Result<Option<Frame>, CodecError>;
}

/// The bitrate video is sent at.
pub const BITRATE: u32 = 700_000;

pub struct H264Encoder {
    encoder: Encoder,
}

impl H264Encoder {
    pub fn new() -> Result<H264Encoder, CodecError> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::CameraVideoRealTime)
            .bitrate(BitRate::from_bps(BITRATE))
            .max_frame_rate(FrameRate::from_hz(FRAME_RATE as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            // A keyframe every few seconds heals what loss broke, without anyone asking.
            .intra_frame_period(IntraFramePeriod::from_num_frames(FRAME_RATE * 4))
            .skip_frames(true);
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), config).map_err(error)?;
        Ok(H264Encoder { encoder })
    }
}

impl VideoEncoder for H264Encoder {
    fn encode(&mut self, frame: &Frame, keyframe: bool) -> Result<Option<Encoded>, CodecError> {
        let frame = if frame.width % 2 == 1 || frame.height % 2 == 1 {
            &frame.clone().fit_within(frame.width, frame.height)
        } else {
            frame
        };
        let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(
            &frame.bgra,
            (frame.width as usize, frame.height as usize),
        ));
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let stream = self.encoder.encode(&yuv).map_err(error)?;
        let frame_type = stream.frame_type();
        if matches!(frame_type, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = stream.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Encoded {
            data,
            keyframe: matches!(frame_type, FrameType::IDR | FrameType::I),
        }))
    }
}

pub struct H264Decoder {
    decoder: Decoder,
}

impl H264Decoder {
    pub fn new() -> Result<H264Decoder, CodecError> {
        let decoder = Decoder::with_api_config(OpenH264API::from_source(), DecoderConfig::new())
            .map_err(error)?;
        Ok(H264Decoder { decoder })
    }
}

impl VideoDecoder for H264Decoder {
    fn decode(&mut self, data: &[u8]) -> Result<Option<Frame>, CodecError> {
        let Some(yuv) = self.decoder.decode(data).map_err(error)? else {
            return Ok(None);
        };
        let (width, height) = yuv.dimensions();
        let mut bgra = vec![0; width * height * 4];
        yuv.write_rgba8(&mut bgra);
        for pixel in bgra.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        Ok(Some(Frame {
            width: width as u32,
            height: height as u32,
            bgra,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::TestPattern;

    fn close(a: [u8; 3], b: [u8; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= 24)
    }

    #[test]
    fn frames_survive_encoding_and_decoding() {
        let color = [40, 160, 220];
        let mut encoder = H264Encoder::new().unwrap();
        let mut decoder = H264Decoder::new().unwrap();
        let mut decoded = 0;
        let mut keyframes = 0;
        for n in 0..30 {
            let frame = TestPattern::frame(color, n);
            let Some(encoded) = encoder.encode(&frame, n == 15).unwrap() else {
                continue;
            };
            keyframes += usize::from(encoded.keyframe);
            if let Some(out) = decoder.decode(&encoded.data).unwrap() {
                assert_eq!((out.width, out.height), (frame.width, frame.height));
                // Far from the bar, the background's color comes through.
                let x = if (n * 4) % 288 > 160 { 20 } else { 300 };
                assert!(
                    close(out.pixel(x, 120), color),
                    "frame {n}: {:?}",
                    out.pixel(x, 120)
                );
                decoded += 1;
            }
        }
        assert!(decoded >= 25, "decoded {decoded}");
        assert!(
            keyframes >= 2,
            "the first frame and the asked-for one: {keyframes}"
        );
    }
}
