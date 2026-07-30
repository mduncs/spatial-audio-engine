//! Canonical 32-bit float PCM WAV writer and reader.
//!
//! Written encoding contract (the single encoding this layer produces/consumes):
//!   - RIFF/WAVE container, little-endian byte order;
//!   - one `fmt ` chunk with WAVE_FORMAT_IEEE_FLOAT (format tag `3`);
//!   - 32 bits per IEEE-754 sample, `channels` in {1, 2};
//!   - interleaved frames; a stereo frame is laid out as `L, R, L, R, ...`;
//!   - an explicit, non-zero sample rate in the `fmt ` chunk;
//!   - a single `data` chunk holding every sample as four LE bytes.
//!
//! The writer rejects NaN/Inf samples and any frame/channel mismatch. The reader
//! walks chunks (skipping anything it does not need), then rejects the wrong
//! format tag, an unsupported bit depth, truncation, frame/channel mismatch, and
//! NaN/Inf samples. There is intentionally no integer-PCM path: a quantizing
//! round trip would not be lossless for `f32` diagnostic stems.

use crate::hash::sha256_hex;

/// The only format tag this layer writes or accepts.
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const SAMPLE_BYTES: usize = 4;

/// Sample rate and channel count for a finite interleaved buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WavSpec {
    pub sample_rate_hz: u32,
    pub channels: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WavError {
    InvalidSampleRate,
    InvalidChannelCount,
    FrameChannelMismatch,
    NonFiniteSample,
    MalformedHeader,
    UnknownFormatTag(u16),
    UnsupportedBitsPerSample(u16),
    Truncated,
    /// The body does not fit the 32-bit RIFF/data size fields. The writer never
    /// truncates a size silently.
    TooLarge,
}

impl WavError {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidSampleRate => "sample rate must be non-zero",
            Self::InvalidChannelCount => "channel count must be 1 or 2",
            Self::FrameChannelMismatch => "sample count is not a whole number of frames",
            Self::NonFiniteSample => "sample is NaN or infinity",
            Self::MalformedHeader => "RIFF/WAVE header or fmt chunk is malformed",
            Self::UnknownFormatTag(_) => "fmt format tag is not IEEE float (3)",
            Self::UnsupportedBitsPerSample(_) => "bits per sample is not 32",
            Self::Truncated => "file is truncated short of its declared length",
            Self::TooLarge => "body exceeds the 32-bit RIFF/data size fields",
        }
    }
}

/// Reject the spec-level invariants shared by the writer and reader.
pub fn validate_spec(spec: WavSpec) -> Result<(), WavError> {
    if spec.sample_rate_hz == 0 {
        return Err(WavError::InvalidSampleRate);
    }
    if !matches!(spec.channels, 1 | 2) {
        return Err(WavError::InvalidChannelCount);
    }
    Ok(())
}

/// Serialize `samples` (interleaved) into canonical 32-bit float WAV bytes.
pub fn write_wav(spec: WavSpec, samples: &[f32]) -> Result<Vec<u8>, WavError> {
    validate_spec(spec)?;
    let channels = usize::from(spec.channels);
    if channels != 0 && samples.len() % channels != 0 {
        return Err(WavError::FrameChannelMismatch);
    }
    for &sample in samples {
        if !sample.is_finite() {
            return Err(WavError::NonFiniteSample);
        }
    }

    let (riff_size, data_size, byte_rate) = wav_sizes(samples.len(), spec)?;
    let block_align = (channels as u16) * (SAMPLE_BYTES as u16);

    let mut out = Vec::with_capacity(44usize.saturating_add(samples.len() * SAMPLE_BYTES));
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32_to_le(riff_size));
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&u32_to_le(16));
    out.extend_from_slice(&WAVE_FORMAT_IEEE_FLOAT.to_le_bytes());
    out.extend_from_slice(&spec.channels.to_le_bytes());
    out.extend_from_slice(&spec.sample_rate_hz.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&((SAMPLE_BYTES as u16) * 8).to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32_to_le(data_size));
    for &sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    Ok(out)
}

/// Compute the RIFF size, data-chunk size, and byte rate for a float WAV body,
/// rejecting any value that does not fit its 32-bit container field with
/// [`WavError::TooLarge`]. Pure so the boundary can be tested without allocating
/// a multi-gigabyte sample buffer.
fn wav_sizes(samples_len: usize, spec: WavSpec) -> Result<(u32, u32, u32), WavError> {
    let channels = u32::from(spec.channels);
    let data_bytes = samples_len
        .checked_mul(SAMPLE_BYTES)
        .ok_or(WavError::TooLarge)?;
    let data_size = u32::try_from(data_bytes).map_err(|_| WavError::TooLarge)?;
    // The RIFF size field follows the 8-byte "RIFF"/WAVE marker and the 36-byte
    // header, so it is `data_size + 36` and must itself fit a 32-bit field.
    let riff_size = data_size.checked_add(36).ok_or(WavError::TooLarge)?;
    let byte_rate = spec
        .sample_rate_hz
        .checked_mul(channels)
        .and_then(|v| v.checked_mul(SAMPLE_BYTES as u32))
        .ok_or(WavError::TooLarge)?;
    Ok((riff_size, data_size, byte_rate))
}

/// Parse canonical 32-bit float WAV bytes into `(spec, interleaved samples)`.
pub fn read_wav(bytes: &[u8]) -> Result<(WavSpec, Vec<f32>), WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::MalformedHeader);
    }

    let mut spec: Option<WavSpec> = None;
    let mut format_tag = 0u16;
    let mut bits_per_sample = 0u16;
    let mut fmt_seen = false;
    let mut data: Option<&[u8]> = None;

    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        pos += 8;
        if pos.checked_add(size).map_or(true, |end| end > bytes.len()) {
            return Err(WavError::Truncated);
        }
        let body = &bytes[pos..pos + size];

        if id == b"fmt " {
            if size < 16 {
                return Err(WavError::MalformedHeader);
            }
            format_tag = u16::from_le_bytes([body[0], body[1]]);
            let channels = u16::from_le_bytes([body[2], body[3]]);
            let sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits_per_sample = u16::from_le_bytes([body[14], body[15]]);
            spec = Some(WavSpec {
                sample_rate_hz: sample_rate,
                channels,
            });
            fmt_seen = true;
        } else if id == b"data" {
            data = Some(body);
        }

        // Chunks are word-aligned: an odd body length is followed by one pad byte.
        pos += size + (size & 1);
    }

    let spec = spec.ok_or(WavError::MalformedHeader)?;
    if !fmt_seen {
        return Err(WavError::MalformedHeader);
    }
    if format_tag != WAVE_FORMAT_IEEE_FLOAT {
        return Err(WavError::UnknownFormatTag(format_tag));
    }
    if bits_per_sample != (SAMPLE_BYTES as u16) * 8 {
        return Err(WavError::UnsupportedBitsPerSample(bits_per_sample));
    }
    validate_spec(spec)?;

    let data = data.ok_or(WavError::MalformedHeader)?;
    if data.len() % SAMPLE_BYTES != 0 {
        return Err(WavError::Truncated);
    }
    let channels = usize::from(spec.channels);
    let total_samples = data.len() / SAMPLE_BYTES;
    if channels != 0 && total_samples % channels != 0 {
        return Err(WavError::FrameChannelMismatch);
    }

    let mut samples = Vec::with_capacity(total_samples);
    for block in data.chunks_exact(SAMPLE_BYTES) {
        let sample = f32::from_le_bytes([block[0], block[1], block[2], block[3]]);
        if !sample.is_finite() {
            return Err(WavError::NonFiniteSample);
        }
        samples.push(sample);
    }
    Ok((spec, samples))
}

/// Deterministic content hash for a stem: SHA-256 over the canonical WAV bytes.
///
/// The hash is over the written container, not the raw sample slice, so it is
/// stable regardless of how the caller stored the PCM before capture.
pub fn stem_hash(spec: WavSpec, samples: &[f32]) -> Result<String, WavError> {
    let bytes = write_wav(spec, samples)?;
    Ok(sha256_hex(&bytes))
}

fn u32_to_le(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(spec: WavSpec, freq: f32, frames: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity(frames * usize::from(spec.channels));
        for n in 0..frames {
            let v =
                (2.0 * std::f32::consts::PI * freq * n as f32 / spec.sample_rate_hz as f32).sin();
            for _ in 0..spec.channels {
                out.push(v);
            }
        }
        out
    }

    #[test]
    fn round_trips_mono_and_stereo() {
        for channels in [1u16, 2] {
            let spec = WavSpec {
                sample_rate_hz: 48_000,
                channels,
            };
            let original = sine(spec, 1_000.0, 480);
            let bytes = write_wav(spec, &original).unwrap();
            let (read_spec, read_samples) = read_wav(&bytes).unwrap();
            assert_eq!(read_spec, spec);
            assert_eq!(read_samples, original);
        }
    }

    #[test]
    fn rejects_non_finite_on_write() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                write_wav(spec, &[bad]).unwrap_err(),
                WavError::NonFiniteSample
            );
        }
    }

    #[test]
    fn rejects_bad_spec_and_mismatch() {
        let bad_rate = WavSpec {
            sample_rate_hz: 0,
            channels: 1,
        };
        assert_eq!(
            write_wav(bad_rate, &[]).unwrap_err(),
            WavError::InvalidSampleRate
        );

        let bad_channels = WavSpec {
            sample_rate_hz: 48_000,
            channels: 6,
        };
        assert_eq!(
            write_wav(bad_channels, &[]).unwrap_err(),
            WavError::InvalidChannelCount
        );

        let stereo = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        // Odd sample count cannot form whole stereo frames.
        assert_eq!(
            write_wav(stereo, &[0.0, 0.0, 0.0]).unwrap_err(),
            WavError::FrameChannelMismatch
        );
    }

    #[test]
    fn rejects_malformed_and_truncated_input() {
        assert_eq!(read_wav(b"").unwrap_err(), WavError::MalformedHeader);
        assert_eq!(
            read_wav(b"RIFXXXXX").unwrap_err(),
            WavError::MalformedHeader
        );

        // A valid float-WAV header whose data chunk is truncated mid-sample.
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        let mut bytes = write_wav(spec, &[0.0, 0.0]).unwrap();
        bytes.truncate(bytes.len() - 1);
        assert_eq!(read_wav(&bytes).unwrap_err(), WavError::Truncated);
    }

    #[test]
    fn rejects_non_float_format_tag() {
        // Hand-assembled PCM (tag 1) header; reader must refuse it.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&u32_to_le(36));
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&u32_to_le(16));
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&u32_to_le(0));
        assert!(matches!(
            read_wav(&bytes).unwrap_err(),
            WavError::UnknownFormatTag(1)
        ));
    }

    #[test]
    fn stem_hash_is_stable() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        let samples = sine(spec, 1_000.0, 480);
        let a = stem_hash(spec, &samples).unwrap();
        let b = stem_hash(spec, &samples).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn wav_sizes_reject_bodies_beyond_the_32_bit_field() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        // Exactly representable: the largest body whose RIFF size still fits u32.
        let max_samples = (u32::MAX as usize - 36) / SAMPLE_BYTES;
        let (riff, data, _rate) = wav_sizes(max_samples, spec).unwrap();
        assert_eq!(data, (max_samples * SAMPLE_BYTES) as u32);
        assert_eq!(riff, data + 36);
        assert!(riff <= u32::MAX);

        // One sample past the limit: data no longer fits the 32-bit data field
        // once it exceeds u32::MAX bytes.
        let over = (u32::MAX as usize / SAMPLE_BYTES) + 1;
        assert_eq!(wav_sizes(over, spec).unwrap_err(), WavError::TooLarge);

        // A length large enough that len * SAMPLE_BYTES overflows usize itself.
        assert_eq!(wav_sizes(usize::MAX, spec).unwrap_err(), WavError::TooLarge);
    }

    #[test]
    fn wav_sizes_reject_byte_rate_overflow() {
        // A pathological sample rate with two channels overflows byte_rate even
        // for an empty body; the helper must reject it instead of wrapping.
        let spec = WavSpec {
            sample_rate_hz: u32::MAX,
            channels: 2,
        };
        assert_eq!(wav_sizes(0, spec).unwrap_err(), WavError::TooLarge);
    }

    #[test]
    fn write_wav_round_trip_still_fits_after_checked_arithmetic() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let samples = sine(spec, 1_000.0, 480);
        let bytes = write_wav(spec, &samples).unwrap();
        let (read_spec, read_samples) = read_wav(&bytes).unwrap();
        assert_eq!(read_spec, spec);
        assert_eq!(read_samples, samples);
    }
}
