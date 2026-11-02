//! Opus at 48 kHz mono, through `unsafe-libopus` (libopus 1.3.1 transpiled to Rust: the reference
//! codec, with nothing to compile or link). The wrappers own the codec state and are the only
//! place its `unsafe` calls are made.

use unsafe_libopus::{
    OPUS_APPLICATION_VOIP, OPUS_OK, OPUS_SET_BITRATE_REQUEST, OPUS_SET_INBAND_FEC_REQUEST,
    OPUS_SET_PACKET_LOSS_PERC_REQUEST, OpusDecoder as RawDecoder, OpusEncoder as RawEncoder,
    opus_decode, opus_decoder_create, opus_decoder_destroy, opus_encode, opus_encoder_create,
    opus_encoder_ctl, opus_encoder_destroy,
};

use crate::{AudioError, FRAME_SAMPLES, SAMPLE_RATE};

/// The bitrate speech is sent at.
pub const BITRATE: i32 = 32_000;
/// A packet's size limit (what libopus recommends for one frame).
const MAX_PACKET: usize = 1_275;
/// The longest frame a packet may decode to (120 ms).
const MAX_FRAME: usize = 5_760;

pub struct OpusEncoder {
    raw: *mut RawEncoder,
}

// SAFETY: the state is owned by this value and only touched through `&mut self`.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new() -> Result<OpusEncoder, AudioError> {
        let mut status = 0;
        // SAFETY: valid arguments; the result is checked before use.
        let raw = unsafe {
            opus_encoder_create(SAMPLE_RATE as i32, 1, OPUS_APPLICATION_VOIP, &mut status)
        };
        if status != OPUS_OK || raw.is_null() {
            return Err(AudioError::Codec(format!(
                "encoder create failed ({status})"
            )));
        }
        // SAFETY: `raw` is a live encoder; these requests take one i32 each.
        unsafe {
            opus_encoder_ctl!(raw, OPUS_SET_BITRATE_REQUEST, BITRATE);
            opus_encoder_ctl!(raw, OPUS_SET_INBAND_FEC_REQUEST, 1);
            opus_encoder_ctl!(raw, OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5);
        }
        Ok(OpusEncoder { raw })
    }

    /// Compresses one 20 ms frame.
    pub fn encode(&mut self, frame: &[i16]) -> Result<Vec<u8>, AudioError> {
        if frame.len() != FRAME_SAMPLES {
            return Err(AudioError::Codec(format!(
                "a frame is {FRAME_SAMPLES} samples, not {}",
                frame.len()
            )));
        }
        let mut out = vec![0u8; MAX_PACKET];
        // SAFETY: `frame` holds FRAME_SAMPLES samples and `out` MAX_PACKET bytes, as passed.
        let written = unsafe {
            opus_encode(
                self.raw,
                frame.as_ptr(),
                FRAME_SAMPLES as i32,
                out.as_mut_ptr(),
                MAX_PACKET as i32,
            )
        };
        if written < 0 {
            return Err(AudioError::Codec(format!("encode failed ({written})")));
        }
        out.truncate(written as usize);
        Ok(out)
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create and dropped once.
        unsafe { opus_encoder_destroy(self.raw) }
    }
}

pub struct OpusDecoder {
    raw: *mut RawDecoder,
}

// SAFETY: as for the encoder.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<OpusDecoder, AudioError> {
        let mut status = 0;
        // SAFETY: valid arguments; the result is checked before use.
        let raw = unsafe { opus_decoder_create(SAMPLE_RATE as i32, 1, &mut status) };
        if status != OPUS_OK || raw.is_null() {
            return Err(AudioError::Codec(format!(
                "decoder create failed ({status})"
            )));
        }
        Ok(OpusDecoder { raw })
    }

    /// Decompresses one packet into 48 kHz mono samples.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>, AudioError> {
        let mut out = vec![0i16; MAX_FRAME];
        // SAFETY: `packet` and `out` are valid for the lengths passed.
        let samples = unsafe {
            opus_decode(
                self.raw,
                packet.as_ptr(),
                packet.len() as i32,
                out.as_mut_ptr(),
                MAX_FRAME as i32,
                0,
            )
        };
        if samples < 0 {
            return Err(AudioError::Codec(format!("decode failed ({samples})")));
        }
        out.truncate(samples as usize);
        Ok(out)
    }

    /// A guess at a lost 20 ms frame (packet-loss concealment).
    pub fn conceal(&mut self) -> Result<Vec<i16>, AudioError> {
        let mut out = vec![0i16; FRAME_SAMPLES];
        // SAFETY: a null packet asks for concealment; `out` holds FRAME_SAMPLES.
        let samples = unsafe {
            opus_decode(
                self.raw,
                std::ptr::null(),
                0,
                out.as_mut_ptr(),
                FRAME_SAMPLES as i32,
                0,
            )
        };
        if samples < 0 {
            return Err(AudioError::Codec(format!("conceal failed ({samples})")));
        }
        out.truncate(samples as usize);
        Ok(out)
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create and dropped once.
        unsafe { opus_decoder_destroy(self.raw) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{is_speaking, level};

    #[test]
    fn speech_survives_the_codec() {
        let mut encoder = OpusEncoder::new().unwrap();
        let mut decoder = OpusDecoder::new().unwrap();
        let mut phase = 0f32;
        let mut last = Vec::new();
        for _ in 0..25 {
            let frame: Vec<i16> = (0..FRAME_SAMPLES)
                .map(|_| {
                    phase += 440.0 * std::f32::consts::TAU / 48_000.0;
                    (phase.sin() * 8_000.0) as i16
                })
                .collect();
            let packet = encoder.encode(&frame).unwrap();
            assert!(packet.len() < 200, "about 32 kbit/s: {}", packet.len());
            last = decoder.decode(&packet).unwrap();
            assert_eq!(last.len(), FRAME_SAMPLES);
        }
        let heard = level(&last);
        assert!(is_speaking(heard) && heard < 0.3, "{heard}");
        assert_eq!(decoder.conceal().unwrap().len(), FRAME_SAMPLES);
        assert!(encoder.encode(&[0; 10]).is_err());
    }
}
