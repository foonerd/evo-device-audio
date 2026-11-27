// Copyright (c) 2026 Stefan Arentz; modifications (c) 2026 Just a Nerd
// SPDX-License-Identifier: Apache-2.0 AND MIT
//! AAC-LC decoding via `symphonia`.
//!
//! The buffered-audio payload is raw AAC-LC (no ADTS). We build the decoder
//! from an AudioSpecificConfig for the negotiated format and feed each frame
//! directly, converting the decoded samples to interleaved `i16` PCM.

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{
    CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_AAC,
};
use symphonia::core::formats::Packet;

pub struct AacDecoder {
    decoder: Box<dyn Decoder>,
    sample_rate: u32,
    channels: u8,
}

#[derive(Debug)]
pub enum DecodeError {
    Config,
    Decode(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Config => write!(f, "unsupported AAC configuration"),
            DecodeError::Decode(e) => write!(f, "AAC decode error: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

fn make_decoder(
    sample_rate: u32,
    channels: u8,
) -> Result<Box<dyn Decoder>, DecodeError> {
    let asc = audio_specific_config(sample_rate, channels)
        .ok_or(DecodeError::Config)?;
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_AAC)
        .with_sample_rate(sample_rate)
        .with_extra_data(asc.into_boxed_slice());
    symphonia::default::get_codecs()
        .make(&params, &DecoderOptions::default())
        .map_err(|e| DecodeError::Decode(e.to_string()))
}

impl AacDecoder {
    /// Build a decoder for AAC-LC at the given rate and channel count.
    pub fn new(
        sample_rate: u32,
        channels: u8,
    ) -> Result<AacDecoder, DecodeError> {
        Ok(AacDecoder {
            decoder: make_decoder(sample_rate, channels)?,
            sample_rate,
            channels,
        })
    }

    /// Decode one raw AAC-LC frame to interleaved `i16` PCM.
    ///
    /// A malformed frame is an error, never a panic: the decoder library has
    /// been seen to index out of bounds on corrupted input, so the call is
    /// guarded, and after such a failure the decoder is rebuilt because its
    /// state can no longer be trusted. A frame comes from the network.
    pub fn decode(&mut self, frame: &[u8]) -> Result<Vec<i16>, DecodeError> {
        let attempt =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.decode_frame(frame)
            }));
        match attempt {
            Ok(result) => result,
            Err(_) => {
                self.decoder = make_decoder(self.sample_rate, self.channels)?;
                Err(DecodeError::Decode(
                    "the decoder failed on a malformed frame".into(),
                ))
            }
        }
    }

    fn decode_frame(&mut self, frame: &[u8]) -> Result<Vec<i16>, DecodeError> {
        let packet = Packet::new_from_slice(0, 0, 0, frame);
        let decoded = self
            .decoder
            .decode(&packet)
            .map_err(|e| DecodeError::Decode(e.to_string()))?;
        let mut samples = SampleBuffer::<i16>::new(
            decoded.capacity() as u64,
            *decoded.spec(),
        );
        samples.copy_interleaved_ref(decoded);
        Ok(samples.samples().to_vec())
    }
}

/// The 2-byte AudioSpecificConfig for AAC-LC (object type 2): 5 bits object
/// type, 4 bits sample-rate index, 4 bits channel config.
fn audio_specific_config(sample_rate: u32, channels: u8) -> Option<Vec<u8>> {
    let freq_index: u8 = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        _ => return None,
    };
    if channels == 0 || channels > 7 {
        return None;
    }
    let object_type = 2u8; // AAC-LC
    let byte0 = (object_type << 3) | (freq_index >> 1);
    let byte1 = ((freq_index & 1) << 7) | (channels << 3);
    Some(vec![byte0, byte1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_specific_config_for_44100_stereo() {
        assert_eq!(audio_specific_config(44100, 2), Some(vec![0x12, 0x10]));
        assert_eq!(audio_specific_config(48000, 2), Some(vec![0x11, 0x90]));
        assert_eq!(audio_specific_config(11025, 2), None);
    }

    #[test]
    fn decodes_golden_aac_frames_to_pcm() {
        // Raw AAC-LC frames (ADTS stripped) from an ffmpeg-encoded 440/660 Hz
        // sine, and the config to decode them. Generated once, committed.
        let fmt = include_str!("../tests/data/aac_fmt.txt");
        let mut parts = fmt.split_whitespace();
        let rate: u32 = parts.next().unwrap().parse().unwrap();
        let channels: u8 = parts.next().unwrap().parse().unwrap();

        let frames_bin = include_bytes!("../tests/data/aac_frames.bin");
        // File is a sequence of [u32 LE length][frame].
        let mut decoder = AacDecoder::new(rate, channels).unwrap();
        let mut pos = 0;
        let mut decoded_any = false;
        while pos + 4 <= frames_bin.len() {
            let len = u32::from_le_bytes(
                frames_bin[pos..pos + 4].try_into().unwrap(),
            ) as usize;
            pos += 4;
            let frame = &frames_bin[pos..pos + len];
            pos += len;
            if let Ok(pcm) = decoder.decode(frame) {
                if !pcm.is_empty() {
                    // A full AAC frame is 1024 samples/channel.
                    assert_eq!(pcm.len() % channels as usize, 0);
                    decoded_any = true;
                }
            }
        }
        assert!(
            decoded_any,
            "decoder produced no PCM from the golden frames"
        );
    }

    /// A 207-byte frame, found by corrupting real ones, on which the AAC
    /// library indexes out of bounds. It must come back as an error.
    const PANICKING_FRAME: &str = concat!(
        "d08c4cea5c8e12f14259aedf9a44d1204ecac42c380fb471c7c44f005a2ef887",
        "e767002fca06a1db508204f67d6143f7528b410d781a2d52af7a9c2065d51f22",
        "91f6ca1f7d60e7d7b33d45241650815b64a10cfba7b62c3facb77affdd29ea3a",
        "d014195790424ab53adf0ef842dfc11e3177ecc10110628a1ce086afc393c9c9",
        "8cedaa02daec79d9939fe7e5d16a6b27f2125afefa43c961297b2e5c09df7db4",
        "92e47e4c5f744390f44d04347d0954cfa467fb04a0796abb263b10f30ba22f0e",
        "ee7e714c3e81cc2a2e20da0f20ad94",
    );

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn a_frame_that_panics_the_library_is_an_error_and_decoding_goes_on() {
        let frame = unhex(PANICKING_FRAME);
        let mut decoder = AacDecoder::new(44100, 2).unwrap();
        let outcome = decoder.decode(&frame);
        assert!(
            matches!(&outcome, Err(DecodeError::Decode(why)) if why.contains("malformed")),
            "expected the guarded failure, got {outcome:?}"
        );
        // The decoder was rebuilt: it takes a good frame next.
        let good = include_bytes!("../tests/data/tone_lr.frames");
        let frames = crate::sender::parse_frames(good);
        assert!(decoder.decode(frames[5]).is_ok());
    }
}
