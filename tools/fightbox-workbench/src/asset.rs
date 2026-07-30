use std::path::{Path, PathBuf};

use fightbox_api::AssetAnalysis;
use fightbox_evidence::{WavSpec, analyze_decoded_asset, multitone, pink_like, sha256_hex, sine};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct AssetDescriptor {
    pub asset_id: String,
    pub kind: AssetKind,
    pub generator: Generator,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub duration_s: f64,
    pub target_rms_dbfs: f64,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Sine,
    Multitone,
    PinkLike,
    Wav,
}

#[derive(Debug, Deserialize)]
pub struct Generator {
    pub sine: Option<SineBlock>,
    pub multitone: Option<MultitoneBlock>,
    pub pink_like: Option<PinkLikeBlock>,
    pub wav: Option<WavBlock>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct SineBlock {
    pub frequency_hz: f64,
}

#[derive(Debug, Deserialize)]
pub struct MultitoneBlock {
    pub frequencies_hz: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct PinkLikeBlock {
    pub seed: u64,
}

#[derive(Debug, Deserialize)]
pub struct WavBlock {
    pub path: String,
    pub sha256: String,
    #[serde(default)]
    pub start_frame: u64,
    #[serde(default)]
    pub r#loop: bool,
}

pub struct PreparedAsset {
    pub samples: Vec<f32>,
    pub analysis: AssetAnalysis,
}

pub fn load_asset(asset_id: &str) -> Result<PreparedAsset, String> {
    let descriptor_path = repository_root()
        .join("fixtures/assets")
        .join(format!("{asset_id}.json"));
    let bytes = std::fs::read(&descriptor_path)
        .map_err(|error| format!("cannot read {}: {error}", descriptor_path.display()))?;
    let descriptor: AssetDescriptor = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid asset descriptor: {error}"))?;
    if descriptor.asset_id != asset_id
        || descriptor.channels != 1
        || descriptor.sample_rate_hz != 48_000
        || !descriptor.duration_s.is_finite()
        || descriptor.duration_s <= 0.0
    {
        return Err(format!("asset descriptor {asset_id} is incompatible"));
    }
    let frames = (descriptor.duration_s * f64::from(descriptor.sample_rate_hz)).round() as usize;
    let spec = WavSpec {
        sample_rate_hz: descriptor.sample_rate_hz,
        channels: 1,
    };
    let samples = match descriptor.kind {
        AssetKind::Sine => {
            sine(
                spec,
                descriptor
                    .generator
                    .sine
                    .ok_or("sine generator is missing")?
                    .frequency_hz as f32,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples
        }
        AssetKind::Multitone => {
            let frequencies = descriptor
                .generator
                .multitone
                .ok_or("multitone generator is missing")?
                .frequencies_hz
                .into_iter()
                .map(|frequency| frequency as f32)
                .collect::<Vec<_>>();
            multitone(
                spec,
                &frequencies,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples
        }
        AssetKind::PinkLike => {
            pink_like(
                spec,
                descriptor
                    .generator
                    .pink_like
                    .ok_or("pink_like generator is missing")?
                    .seed,
                frames,
                descriptor.target_rms_dbfs as f32,
            )
            .map_err(|error| error.as_str().to_owned())?
            .samples
        }
        AssetKind::Wav => load_wav(
            descriptor.generator.wav.ok_or("WAV generator is missing")?,
            frames,
            descriptor.target_rms_dbfs as f32,
        )?,
    };
    let analysis = analyze_decoded_asset(spec, &samples)
        .map_err(|error| format!("cannot analyze asset {asset_id}: {}", error.as_str()))?
        .into_parts()
        .0;
    Ok(PreparedAsset { samples, analysis })
}

fn load_wav(wav: WavBlock, frames: usize, target_rms_dbfs: f32) -> Result<Vec<f32>, String> {
    let path = resolve_repository_path(&wav.path);
    let bytes = std::fs::read(&path)
        .map_err(|error| format!("cannot read WAV {}: {error}", path.display()))?;
    let actual_hash = sha256_hex(&bytes);
    if actual_hash != wav.sha256 {
        return Err(format!("WAV {} sha256 mismatch", path.display()));
    }
    let source = decode_mono_wav(&bytes)?;
    let start = usize::try_from(wav.start_frame).map_err(|_| "WAV start frame is too large")?;
    if start >= source.len() {
        return Err("WAV start frame lies outside the source".into());
    }
    let mut samples = Vec::with_capacity(frames);
    for frame in 0..frames {
        let source_frame = start + frame;
        samples.push(if wav.r#loop {
            source[source_frame % source.len()]
        } else {
            source.get(source_frame).copied().unwrap_or(0.0)
        });
    }
    normalize_rms(&mut samples, target_rms_dbfs)?;
    Ok(samples)
}

fn decode_mono_wav(bytes: &[u8]) -> Result<Vec<f32>, String> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("malformed RIFF/WAVE header".into());
    }
    let mut format = None;
    let mut data = None;
    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        position += 8;
        let end = position
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or("truncated WAV chunk")?;
        if id == b"fmt " && size >= 16 {
            let body = &bytes[position..end];
            format = Some((
                u16::from_le_bytes(body[0..2].try_into().unwrap()),
                u16::from_le_bytes(body[2..4].try_into().unwrap()),
                u32::from_le_bytes(body[4..8].try_into().unwrap()),
                u16::from_le_bytes(body[14..16].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end + (size & 1);
    }
    let (tag, channels, sample_rate, bits) = format.ok_or("WAV fmt chunk is missing")?;
    if channels != 1 || sample_rate != 48_000 {
        return Err("WAV must be mono 48 kHz".into());
    }
    let data = data.ok_or("WAV data chunk is missing")?;
    match (tag, bits) {
        (1, 16) if data.len() % 2 == 0 => Ok(data
            .chunks_exact(2)
            .map(|sample| i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32768.0)
            .collect()),
        (3, 32) if data.len() % 4 == 0 => data
            .chunks_exact(4)
            .map(|sample| {
                let value = f32::from_le_bytes(sample.try_into().unwrap());
                value
                    .is_finite()
                    .then_some(value)
                    .ok_or_else(|| "non-finite WAV sample".to_owned())
            })
            .collect(),
        _ => Err("WAV must be 16-bit PCM or 32-bit float".into()),
    }
}

fn normalize_rms(samples: &mut [f32], target_rms_dbfs: f32) -> Result<(), String> {
    let rms = (samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32;
    if rms <= 0.0 {
        return Err("WAV selection is silent".into());
    }
    let gain = 10.0_f32.powf((target_rms_dbfs - 20.0 * rms.log10()) / 20.0);
    if samples.iter().any(|sample| sample.abs() * gain > 1.0) {
        return Err("WAV normalization would clip".into());
    }
    for sample in samples {
        *sample *= gain;
    }
    Ok(())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn resolve_repository_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_owned()
    } else {
        repository_root().join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_asset_loads_for_headless_input() {
        let asset = load_asset("s0-approach-sine-1k").unwrap();
        assert_eq!(asset.samples.len(), 4_800);
        assert!(asset.samples.iter().any(|sample| *sample != 0.0));
    }

    #[test]
    fn wav_asset_loads_for_headless_input() {
        let asset = load_asset("toms-diner").unwrap();
        assert!(!asset.samples.is_empty());
        assert!(asset.samples.iter().all(|sample| sample.is_finite()));
    }
}
