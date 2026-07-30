//! Strict parsing of deterministic asset descriptors and regeneration of their
//! mono PCM through `fightbox_evidence::signal`.
//!
//! A descriptor binds a fixture source to either a deterministic generator or a
//! provenance-pinned WAV. This layer parses it with `deny_unknown_fields`,
//! validates cross-field rules the JSON Schema cannot express, and produces the
//! exact finite mono buffer plus its ebur128-backed analysis. The scene-owned
//! source drive is derived separately in [`crate::calibrate`].

use std::path::{Path, PathBuf};

use fightbox_evidence::{
    AnalyzedAsset, GeneratedSignal, GeneratorNormalization, SignalError, SignalKind, WavSpec,
    multitone, pink_like, sha256_hex, sine,
};
use serde::{Deserialize, Serialize};

use crate::schema::ASSET_DESCRIPTOR;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Sine,
    Multitone,
    PinkLike,
    Wav,
}

impl AssetKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Multitone => "multitone",
            Self::PinkLike => "pink_like",
            Self::Wav => "wav",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SineBlock {
    pub frequency_hz: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MultitoneBlock {
    pub frequencies_hz: Vec<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinkLikeBlock {
    pub seed: u64,
}

/// A file-backed mono program source.
///
/// The file hash is mandatory provenance. Relative paths are resolved from the
/// repository root; absolute paths allow intentionally uncommitted recordings.
/// The decoded PCM is normalized to `target_rms_dbfs` using the same dBFS
/// convention as generated assets: a full-scale-peak sine is approximately
/// -3.0103 dBFS RMS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WavBlock {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub start_frame: u64,
    #[serde(default)]
    pub r#loop: bool,
}

/// Generator block. Exactly one inner block is permitted; the kind/block match
/// is enforced after deserialization because JSON Schema cannot express it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default)]
    pub sine: Option<SineBlock>,
    #[serde(default)]
    pub multitone: Option<MultitoneBlock>,
    #[serde(default)]
    pub pink_like: Option<PinkLikeBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wav: Option<WavBlock>,
}

/// The parsed deterministic asset descriptor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetDescriptor {
    pub schema_version: String,
    pub asset_id: String,
    pub kind: AssetKind,
    pub generator: Generator,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub duration_s: f64,
    pub target_rms_dbfs: f64,
    #[serde(default)]
    pub expected_reference_rms_dbfs: Option<f64>,
    #[allow(dead_code)]
    pub calibration: Calibration,
    pub non_claims: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calibration {
    #[serde(default)]
    pub applied_gain_db: Option<f64>,
}

/// A descriptor whose kind/generator contract has been validated and whose frame
/// count has been computed.
#[derive(Clone, Debug)]
pub struct ResolvedAsset {
    pub descriptor: AssetDescriptor,
    pub frame_count: usize,
}

impl AssetDescriptor {
    /// Parse and structurally validate a descriptor from its JSON text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let descriptor: AssetDescriptor =
            serde_json::from_str(text).map_err(|e| format!("invalid asset JSON ({e})"))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    fn validate(&self) -> Result<(), String> {
        if self.schema_version != ASSET_DESCRIPTOR {
            return Err(format!(
                "schema_version must be {ASSET_DESCRIPTOR}, got {}",
                self.schema_version
            ));
        }
        if self.channels != 1 && self.channels != 2 {
            return Err("channels must be 1 or 2".into());
        }
        if self.sample_rate_hz == 0 {
            return Err("sample_rate_hz must be positive".into());
        }
        if !self.duration_s.is_finite() || self.duration_s <= 0.0 {
            return Err("duration_s must be finite and positive".into());
        }
        if !self.target_rms_dbfs.is_finite() || self.target_rms_dbfs >= 0.0 {
            return Err("target_rms_dbfs must be finite and strictly below 0 dBFS".into());
        }
        // The selected kind must carry exactly its matching generator block.
        let present: Vec<&str> = [
            self.generator.sine.is_some().then_some("sine"),
            self.generator.multitone.is_some().then_some("multitone"),
            self.generator.pink_like.is_some().then_some("pink_like"),
            self.generator.wav.is_some().then_some("wav"),
        ]
        .into_iter()
        .flatten()
        .collect();
        if present != [self.kind.as_str()] {
            return Err(format!(
                "kind {} requires exactly the generator.{} block; found {present:?}",
                self.kind.as_str(),
                self.kind.as_str()
            ));
        }
        let nyquist = self.sample_rate_hz as f64 / 2.0;
        match self.kind {
            AssetKind::Sine => {
                self.require_signal_module()?;
                let freq = self.generator.sine.unwrap().frequency_hz;
                check_frequency(freq, nyquist)?;
            }
            AssetKind::Multitone => {
                self.require_signal_module()?;
                let freqs = &self.generator.multitone.as_ref().unwrap().frequencies_hz;
                if freqs.is_empty() {
                    return Err("multitone requires at least one frequency".into());
                }
                let mut seen = std::collections::HashSet::new();
                for &freq in freqs {
                    check_frequency(freq, nyquist)?;
                    if !seen.insert(freq.to_bits()) {
                        return Err(format!("multitone frequency {freq} repeats"));
                    }
                }
            }
            AssetKind::PinkLike => {
                self.require_signal_module()?;
                let _ = self.generator.pink_like.unwrap().seed;
            }
            AssetKind::Wav => {
                if self.generator.module.is_some() {
                    return Err("kind wav does not use generator.module".into());
                }
                if self.channels != 1 {
                    return Err("wav asset descriptors must declare channels=1".into());
                }
                if self.sample_rate_hz != 48_000 {
                    return Err("wav asset descriptors must declare sample_rate_hz=48000".into());
                }
                let wav = self.generator.wav.as_ref().unwrap();
                if wav.path.is_empty() {
                    return Err("generator.wav.path must not be empty".into());
                }
                if wav.sha256.len() != 64
                    || !wav
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                {
                    return Err(
                        "generator.wav.sha256 must be 64 lowercase hexadecimal characters".into(),
                    );
                }
            }
        }
        if !self.non_claims.iter().any(|c| {
            c == "This descriptor makes no delivered-ear-SPL claim without output calibration."
        }) {
            return Err("asset descriptor must carry the no-delivered-ear-SPL non-claim".into());
        }
        Ok(())
    }

    fn require_signal_module(&self) -> Result<(), String> {
        if self.generator.module.as_deref() != Some("fightbox_evidence::signal") {
            return Err("generator.module must be fightbox_evidence::signal".into());
        }
        Ok(())
    }

    /// Resolve this descriptor to a generator-ready asset, computing frame count.
    pub fn resolve(&self) -> Result<ResolvedAsset, String> {
        let frame_count = (self.duration_s * self.sample_rate_hz as f64).round() as usize;
        if frame_count == 0 {
            return Err("duration_s rounded to zero frames".into());
        }
        Ok(ResolvedAsset {
            descriptor: self.clone(),
            frame_count,
        })
    }
}

impl ResolvedAsset {
    /// Regenerate the deterministic mono PCM and analyze it with the real
    /// ebur128-backed analyzer. The returned signal carries the generator
    /// normalization record; the analysis is the decoded, pre-drive program RMS.
    pub fn regenerate_mono(&self) -> Result<(GeneratedSignal, AnalyzedAsset), String> {
        let descriptor = &self.descriptor;
        // Phase A fixtures bind mono assets; the evidence generator supports
        // stereo duplication, but the source calibration chain operates on the
        // mono program. Reject a stereo descriptor so the one-gain chain stays
        // bound to a single channel aggregation.
        if descriptor.channels != 1 {
            return Err("Phase A asset descriptors must declare mono channels".into());
        }
        let spec = WavSpec {
            sample_rate_hz: descriptor.sample_rate_hz,
            channels: 1,
        };
        let target = descriptor.target_rms_dbfs as f32;
        let signal = match descriptor.kind {
            AssetKind::Sine => {
                let frequency = descriptor.generator.sine.unwrap().frequency_hz as f32;
                sine(spec, frequency, self.frame_count, target)
            }
            AssetKind::Multitone => {
                let frequencies: Vec<f32> = descriptor
                    .generator
                    .multitone
                    .as_ref()
                    .unwrap()
                    .frequencies_hz
                    .iter()
                    .map(|&v| v as f32)
                    .collect();
                multitone(spec, &frequencies, self.frame_count, target)
            }
            AssetKind::PinkLike => {
                let seed = descriptor.generator.pink_like.unwrap().seed;
                pink_like(spec, seed, self.frame_count, target)
            }
            AssetKind::Wav => return self.load_wav_mono(),
        }
        .map_err(map_signal_error)?;
        if !signal.samples.iter().all(|s| s.is_finite()) {
            return Err("regenerated asset PCM is not finite".into());
        }
        let analysis = signal.analyze().map_err(|e| {
            format!(
                "regenerated asset analysis failed: {}",
                asset_analysis_message(&e)
            )
        })?;
        Ok((signal, analysis))
    }

    fn load_wav_mono(&self) -> Result<(GeneratedSignal, AnalyzedAsset), String> {
        let descriptor = &self.descriptor;
        let wav = descriptor.generator.wav.as_ref().unwrap();
        let path = resolve_wav_path(&wav.path);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot read WAV asset {}: {error}", path.display()))?;
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != wav.sha256 {
            return Err(format!(
                "WAV asset {} sha256 mismatch: descriptor {}, file {}",
                path.display(),
                wav.sha256,
                actual_hash
            ));
        }

        let decoded = decode_source_wav(&bytes, &path)?;
        if decoded.sample_rate_hz != 48_000 {
            return Err(format!(
                "WAV asset {} must be 48000 Hz, got {} Hz",
                path.display(),
                decoded.sample_rate_hz
            ));
        }
        if decoded.channels != 1 {
            return Err(format!(
                "WAV asset {} must be mono, got {} channels",
                path.display(),
                decoded.channels
            ));
        }

        let source_frames = decoded.samples.len();
        let start = usize::try_from(wav.start_frame)
            .map_err(|_| "generator.wav.start_frame does not fit this platform".to_string())?;
        if start >= source_frames {
            return Err(format!(
                "generator.wav.start_frame {} is outside WAV asset with {} frames",
                wav.start_frame, source_frames
            ));
        }
        let mut samples = Vec::with_capacity(self.frame_count);
        for output_frame in 0..self.frame_count {
            let source_frame = start + output_frame;
            let sample = if wav.r#loop {
                decoded.samples[source_frame % source_frames]
            } else {
                decoded.samples.get(source_frame).copied().unwrap_or(0.0)
            };
            samples.push(sample);
        }

        let normalization =
            normalize_file_samples(&mut samples, descriptor.target_rms_dbfs as f32)?;
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        };
        // GeneratedSignal predates file-backed assets and its closed SignalKind
        // enum has no Wav member. Consumers use the public PCM/spec/normalization
        // fields, so retain the broadband compatibility tag until that shared
        // evidence type can be evolved in a separately owned change.
        let signal = GeneratedSignal {
            kind: SignalKind::PinkLike,
            spec,
            samples,
            normalization,
        };
        let analysis = signal.analyze().map_err(|error| {
            format!(
                "loaded WAV asset analysis failed: {}",
                asset_analysis_message(&error)
            )
        })?;
        Ok((signal, analysis))
    }
}

fn resolve_wav_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(path)
    }
}

struct DecodedSourceWav {
    sample_rate_hz: u32,
    channels: u16,
    samples: Vec<f32>,
}

fn decode_source_wav(bytes: &[u8], path: &Path) -> Result<DecodedSourceWav, String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!(
            "WAV asset {} has a malformed RIFF/WAVE header",
            path.display()
        ));
    }
    let mut format = None;
    let mut data = None;
    let mut position = 12usize;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32::from_le_bytes(
            bytes[position + 4..position + 8]
                .try_into()
                .expect("four-byte chunk size"),
        ) as usize;
        position += 8;
        let end = position
            .checked_add(size)
            .ok_or_else(|| format!("WAV asset {} has an oversized chunk", path.display()))?;
        if end > bytes.len() {
            return Err(format!(
                "WAV asset {} has a truncated chunk",
                path.display()
            ));
        }
        if id == b"fmt " {
            if size < 16 {
                return Err(format!(
                    "WAV asset {} has a short fmt chunk",
                    path.display()
                ));
            }
            let body = &bytes[position..end];
            format = Some((
                u16::from_le_bytes([body[0], body[1]]),
                u16::from_le_bytes([body[2], body[3]]),
                u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
                u16::from_le_bytes([body[12], body[13]]),
                u16::from_le_bytes([body[14], body[15]]),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end
            .checked_add(size & 1)
            .ok_or_else(|| format!("WAV asset {} has invalid chunk alignment", path.display()))?;
    }
    let (format_tag, channels, sample_rate_hz, block_align, bits_per_sample) =
        format.ok_or_else(|| format!("WAV asset {} is missing its fmt chunk", path.display()))?;
    let data =
        data.ok_or_else(|| format!("WAV asset {} is missing its data chunk", path.display()))?;
    if channels != 1 {
        return Err(format!(
            "WAV asset {} must be mono, got {} channels",
            path.display(),
            channels
        ));
    }
    if sample_rate_hz != 48_000 {
        return Err(format!(
            "WAV asset {} must be 48000 Hz, got {} Hz",
            path.display(),
            sample_rate_hz
        ));
    }
    let sample_bytes = match (format_tag, bits_per_sample) {
        (1, 16) => 2usize,
        (3, 32) => 4usize,
        (1, bits) => {
            return Err(format!(
                "WAV asset {} must use 16-bit integer PCM or 32-bit IEEE float PCM, got integer PCM with {bits} bits",
                path.display()
            ));
        }
        (3, bits) => {
            return Err(format!(
                "WAV asset {} must use 32-bit IEEE float PCM, got {bits} bits",
                path.display()
            ));
        }
        (tag, _) => {
            return Err(format!(
                "WAV asset {} has unsupported WAV format tag {tag}; expected 1 (integer PCM) or 3 (IEEE float PCM)",
                path.display()
            ));
        }
    };
    if usize::from(block_align) != sample_bytes * usize::from(channels) {
        return Err(format!(
            "WAV asset {} has block_align {}, expected {}",
            path.display(),
            block_align,
            sample_bytes * usize::from(channels)
        ));
    }
    if data.is_empty() || data.len() % sample_bytes != 0 {
        return Err(format!(
            "WAV asset {} has empty or incomplete sample data",
            path.display()
        ));
    }
    let samples = if sample_bytes == 2 {
        data.chunks_exact(2)
            .map(|chunk| f32::from(i16::from_le_bytes([chunk[0], chunk[1]])) / 32768.0)
            .collect()
    } else {
        let mut samples = Vec::with_capacity(data.len() / 4);
        for chunk in data.chunks_exact(4) {
            let sample = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if !sample.is_finite() {
                return Err(format!(
                    "WAV asset {} contains a non-finite sample",
                    path.display()
                ));
            }
            samples.push(sample);
        }
        samples
    };
    Ok(DecodedSourceWav {
        sample_rate_hz,
        channels,
        samples,
    })
}

fn normalize_file_samples(
    samples: &mut [f32],
    target_rms_dbfs: f32,
) -> Result<GeneratorNormalization, String> {
    let sum_squares = samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>();
    let raw_rms = (sum_squares / samples.len() as f64).sqrt() as f32;
    if raw_rms <= 0.0 {
        return Err("WAV asset selection is silent; calibration gain is undefined".into());
    }
    let raw_rms_dbfs = 20.0 * raw_rms.log10();
    let normalization_gain_db = target_rms_dbfs - raw_rms_dbfs;
    let gain = 10.0_f32.powf(normalization_gain_db / 20.0);
    for sample in samples.iter_mut() {
        *sample *= gain;
    }
    let peak = samples
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if peak > 1.0 {
        return Err(format!(
            "WAV asset calibration gain would push the absolute peak past 1.0 (peak {peak}); no silent clipping"
        ));
    }
    Ok(GeneratorNormalization {
        raw_rms_dbfs,
        target_rms_dbfs,
        normalization_gain_db,
    })
}

fn check_frequency(freq: f64, nyquist: f64) -> Result<(), String> {
    if !freq.is_finite() || freq <= 0.0 {
        return Err(format!("frequency {freq} must be finite and positive"));
    }
    if freq >= nyquist {
        return Err(format!(
            "frequency {freq} must be below Nyquist ({nyquist})"
        ));
    }
    Ok(())
}

fn map_signal_error(error: SignalError) -> String {
    format!("asset regeneration failed: {}", error.as_str())
}

fn asset_analysis_message(error: &fightbox_evidence::AssetAnalysisError) -> &'static str {
    error.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const PINK: &str = include_str!("../../../fixtures/assets/s0-calibrated-pink.json");
    const SINE: &str = include_str!("../../../fixtures/assets/s0-approach-sine-1k.json");
    const MULTITONE: &str = include_str!("../../../fixtures/assets/s3-multitone-spectral.json");
    const TEST_WAV: &[u8] = include_bytes!("../testdata/mono-48k-s16.wav");
    const TEST_WAV_SHA256: &str =
        "af32656c8e98bd9f15400108ab770a2e90792c1308136286bbdb88259277cdf7";
    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parses_all_repo_descriptors() {
        for (text, expected_kind) in [
            (PINK, AssetKind::PinkLike),
            (SINE, AssetKind::Sine),
            (MULTITONE, AssetKind::Multitone),
        ] {
            let descriptor = AssetDescriptor::parse(text).unwrap();
            assert_eq!(descriptor.kind, expected_kind);
            assert_eq!(descriptor.channels, 1);
            assert_eq!(descriptor.sample_rate_hz, 48_000);
        }
    }

    #[test]
    fn rejects_unknown_field() {
        let mut text = SINE.trim_end().to_string();
        text.pop();
        text.push_str(r#","__unknown": true}"#);
        assert!(AssetDescriptor::parse(&text).is_err());
    }

    #[test]
    fn rejects_kind_generator_mismatch() {
        // sine kind but pink_like block present.
        let bad = SINE.replace(r#""kind": "sine""#, r#""kind": "pink_like""#);
        assert!(AssetDescriptor::parse(&bad).is_err());
    }

    #[test]
    fn rejects_above_nyquist_frequency() {
        let bad = SINE.replace("1000.0", "30000.0");
        assert!(AssetDescriptor::parse(&bad).is_err());
    }

    #[test]
    fn regenerates_and_analyzes_repo_pink() {
        let descriptor = AssetDescriptor::parse(PINK).unwrap();
        let resolved = descriptor.resolve().unwrap();
        assert_eq!(resolved.frame_count, 4_800);
        let (signal, analysis) = resolved.regenerate_mono().unwrap();
        assert_eq!(signal.samples.len(), 4_800);
        let rms = analysis.analysis().program_rms_dbfs;
        // The generator normalizes to the declared -20 dBFS target.
        assert!((rms - (-20.0)).abs() < 0.05, "got {rms}");
    }

    fn wav_descriptor(
        path: &Path,
        hash: &str,
        start_frame: u64,
        looping: bool,
        frames: usize,
    ) -> String {
        serde_json::json!({
            "schema_version": "fightbox.asset-descriptor.v1",
            "asset_id": "test-file-backed-wav",
            "kind": "wav",
            "generator": {
                "wav": {
                    "path": path,
                    "sha256": hash,
                    "start_frame": start_frame,
                    "loop": looping
                }
            },
            "channels": 1,
            "sample_rate_hz": 48000,
            "duration_s": frames as f64 / 48000.0,
            "target_rms_dbfs": -40.0,
            "expected_reference_rms_dbfs": null,
            "calibration": {"applied_gain_db": null},
            "non_claims": [
                "This descriptor makes no delivered-ear-SPL claim without output calibration."
            ]
        })
        .to_string()
    }

    fn test_wav_path() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("testdata")
            .join("mono-48k-s16.wav")
    }

    fn temp_wav(bytes: &[u8]) -> PathBuf {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "fightbox-asset-test-{}-{sequence}.wav",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn parses_wav_descriptor_and_rejects_missing_hash_or_generator_mismatch() {
        let text = wav_descriptor(&test_wav_path(), TEST_WAV_SHA256, 0, false, 128);
        let descriptor = AssetDescriptor::parse(&text).unwrap();
        assert_eq!(descriptor.kind, AssetKind::Wav);
        assert_eq!(descriptor.generator.wav.unwrap().start_frame, 0);

        let mut missing_hash: serde_json::Value = serde_json::from_str(&text).unwrap();
        missing_hash["generator"]["wav"]
            .as_object_mut()
            .unwrap()
            .remove("sha256");
        assert!(
            AssetDescriptor::parse(&missing_hash.to_string())
                .unwrap_err()
                .contains("missing field `sha256`")
        );

        let mismatched = text.replace(r#""kind":"wav""#, r#""kind":"sine""#);
        assert!(
            AssetDescriptor::parse(&mismatched)
                .unwrap_err()
                .contains("requires exactly")
        );
    }

    #[test]
    fn rejects_wav_with_wrong_hash_rate_or_channels() {
        let wrong_hash = wav_descriptor(&test_wav_path(), &"0".repeat(64), 0, false, 128);
        assert!(
            AssetDescriptor::parse(&wrong_hash)
                .unwrap()
                .resolve()
                .unwrap()
                .regenerate_mono()
                .unwrap_err()
                .contains("sha256 mismatch")
        );

        for (offset, replacement, expected) in [
            (
                24usize,
                44_100u32.to_le_bytes().to_vec(),
                "must be 48000 Hz",
            ),
            (22usize, 2u16.to_le_bytes().to_vec(), "must be mono"),
        ] {
            let mut bytes = TEST_WAV.to_vec();
            bytes[offset..offset + replacement.len()].copy_from_slice(&replacement);
            let path = temp_wav(&bytes);
            let text = wav_descriptor(&path, &sha256_hex(&bytes), 0, false, 128);
            let error = AssetDescriptor::parse(&text)
                .unwrap()
                .resolve()
                .unwrap()
                .regenerate_mono()
                .unwrap_err();
            assert!(error.contains(expected), "got {error}");
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn wav_start_frame_loops_or_pads_after_end() {
        let decoded = decode_source_wav(TEST_WAV, &test_wav_path()).unwrap();
        let start = decoded.samples.len() - 6;
        let frames = 128;

        let looping = AssetDescriptor::parse(&wav_descriptor(
            &test_wav_path(),
            TEST_WAV_SHA256,
            start as u64,
            true,
            frames,
        ))
        .unwrap()
        .resolve()
        .unwrap()
        .regenerate_mono()
        .unwrap()
        .0;
        let loop_gain = 10.0_f32.powf(looping.normalization.normalization_gain_db / 20.0);
        for (output_frame, actual) in looping.samples.iter().copied().enumerate() {
            let expected =
                decoded.samples[(start + output_frame) % decoded.samples.len()] * loop_gain;
            assert!((actual - expected).abs() < 1e-7);
        }

        let padded = AssetDescriptor::parse(&wav_descriptor(
            &test_wav_path(),
            TEST_WAV_SHA256,
            start as u64,
            false,
            frames,
        ))
        .unwrap()
        .resolve()
        .unwrap()
        .regenerate_mono()
        .unwrap()
        .0;
        let pad_gain = 10.0_f32.powf(padded.normalization.normalization_gain_db / 20.0);
        for output_frame in 0..6 {
            let expected = decoded.samples[start + output_frame] * pad_gain;
            assert!((padded.samples[output_frame] - expected).abs() < 1e-7);
        }
        assert!(padded.samples[6..].iter().all(|sample| *sample == 0.0));
    }
}
