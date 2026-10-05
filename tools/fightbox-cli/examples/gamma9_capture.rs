//! Deterministic offline γ9 spectral-composition stem capture.
//!
//! This writes isolated named-stage and one-composed-filter evidence through
//! public Fightbox APIs. It never opens an audio device.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fightbox_api::spectral::{
    MAX_COMBINED_SPECTRAL_GAIN_DB, MIN_COMBINED_SPECTRAL_GAIN_DB, SPECTRAL_BAND_COUNT,
    SpectralStage, SpectralTransfer,
};
use fightbox_evidence::{WavSpec, sha256_hex, write_wav};
use fightbox_runtime::SpectralTransferFilter;
use serde_json::{Value, json};

const SAMPLE_RATE_HZ: u32 = 48_000;
const FRAMES: usize = SAMPLE_RATE_HZ as usize * 4;
const FADE_FRAMES: usize = 960;
const SOURCE_ID: &str = "gamma9-deterministic-broadband-probe-v1";
const NOISE_SEED: u64 = 0x49c7_2d15_a803_9ef1;

#[derive(Clone, Copy)]
struct StageSpec {
    name: &'static str,
    stage: SpectralStage,
    gain_db: [f32; SPECTRAL_BAND_COUNT],
}

const STAGES: [StageSpec; 5] = [
    StageSpec {
        name: "directivity",
        stage: SpectralStage::Directivity,
        gain_db: [-1.0, -1.5, -2.0, -2.5, -3.0, -4.0, -5.0, -6.0],
    },
    StageSpec {
        name: "atmosphere",
        stage: SpectralStage::Atmosphere,
        gain_db: [-0.1, -0.2, -0.4, -0.8, -1.6, -3.2, -6.4, -12.8],
    },
    StageSpec {
        name: "ground",
        stage: SpectralStage::Ground,
        gain_db: [1.0, 0.5, 0.0, -0.5, -1.0, -1.5, -2.0, -2.5],
    },
    StageSpec {
        name: "occlusion",
        stage: SpectralStage::Occlusion,
        gain_db: [-2.0, -3.0, -5.0, -8.0, -12.0, -18.0, -24.0, -30.0],
    },
    StageSpec {
        name: "enclosure",
        stage: SpectralStage::Enclosure,
        gain_db: [-1.0, -2.0, -3.0, -5.0, -8.0, -12.0, -18.0, -24.0],
    },
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = parse_output()?;
    let temp = prepare_atomic_output(&output)?;
    if let Err(error) = capture(&temp, &output) {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    fs::rename(&temp, &output)?;
    let report = output.join("report.json");
    println!(
        "{}",
        serde_json::to_string(&json!({
            "status": "artifact_generated",
            "report": output_path(&report),
            "sha256": sha256_hex(&fs::read(report)?),
        }))?
    );
    Ok(())
}

fn capture(temp: &Path, final_output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source = source_program();
    let source_descriptor = source_descriptor(&source);
    let source_descriptor_bytes = serde_json::to_vec_pretty(&source_descriptor)?;
    fs::write(temp.join("source.json"), &source_descriptor_bytes)?;
    let source_descriptor_sha256 = sha256_hex(&source_descriptor_bytes);
    let source_wav = wav(1, &source)?;
    fs::write(temp.join("source-mono.wav"), &source_wav)?;

    let neutral = SpectralTransfer::NEUTRAL;
    let forward = combined_transfer(STAGES.into_iter())?;
    let reverse = combined_transfer(STAGES.into_iter().rev())?;
    if forward != reverse {
        return Err(io::Error::other("stage order changed the composed transfer").into());
    }
    verify_composition(forward)?;

    let mut stems = Vec::new();
    stems.push(write_stem(temp, final_output, "neutral", neutral, &source)?);
    for spec in STAGES {
        let transfer = SpectralTransfer::NEUTRAL
            .with_stage(spec.stage, spec.gain_db)
            .map_err(debug_error)?;
        stems.push(write_stem(
            temp,
            final_output,
            &format!("stage-{}", spec.name),
            transfer,
            &source,
        )?);
    }
    stems.push(write_stem(
        temp,
        final_output,
        "combined-once",
        forward,
        &source,
    )?);

    let executable = std::env::current_exe()?.canonicalize()?;
    let report = json!({
        "schema_version": "fightbox.gamma9-spectral-composition-capture.v1",
        "status": "artifact_generated",
        "evidence_class": "portable_runtime_offline_stems",
        "provenance": {
            "source_file_sha256": sha256_hex(include_bytes!("gamma9_capture.rs")),
            "executable": output_path(&executable),
            "executable_sha256": sha256_hex(&fs::read(&executable)?),
            "command": ["cargo", "+stable", "run", "--release", "-p", "fightbox-cli", "--example", "gamma9_capture", "--", "--output", output_path(final_output)],
        },
        "source": {
            "source_id": SOURCE_ID,
            "descriptor": output_path(&final_output.join("source.json")),
            "descriptor_sha256": source_descriptor_sha256,
            "wav": output_path(&final_output.join("source-mono.wav")),
            "wav_sha256": sha256_hex(&source_wav),
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "channels": 1,
            "frame_count": FRAMES,
        },
        "composition": {
            "stage_order": STAGES.map(|spec| spec.name),
            "stages": STAGES.map(stage_json),
            "combined_gain_db": forward.combined_gain_db(),
            "order_independent": true,
            "one_filter_per_stem": true,
            "minimum_combined_gain_db": MIN_COMBINED_SPECTRAL_GAIN_DB,
            "maximum_combined_gain_db": MAX_COMBINED_SPECTRAL_GAIN_DB,
        },
        "stems": stems,
        "resources": null,
        "listening": {"status": "pending", "listener_id": "", "outcome": "pending"},
        "claims": [
            "all five named stage curves remain visible",
            "composition is order independent and bounded before one public SpectralTransferFilter application",
            "isolated stage, neutral, and combined-once stems are deterministic portable-runtime evidence",
            "no audio device opened",
        ],
        "non_claims": [
            "diagnostic curves are not a measured atmosphere, ground, material, package, bake, route, or physical scene",
            "not callback timing, RSS, linked backend, device, thermal, AirPods, HRTF, true-peak, playback, or human listening evidence",
            "artifact_generated is not gamma-card captured or passed status",
        ],
    });
    fs::write(
        temp.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn write_stem(
    temp: &Path,
    final_output: &Path,
    label: &str,
    transfer: SpectralTransfer,
    source: &[f32],
) -> Result<Value, Box<dyn std::error::Error>> {
    let mut filter = SpectralTransferFilter::new(SAMPLE_RATE_HZ).map_err(debug_error)?;
    filter.set_transfer(transfer);
    let mut stereo = Vec::with_capacity(source.len() * 2);
    for &sample in source {
        let centered = filter.process_sample(sample) * std::f32::consts::FRAC_1_SQRT_2;
        stereo.extend_from_slice(&[centered, centered]);
    }
    if stereo.iter().any(|sample| !sample.is_finite()) {
        return Err(io::Error::other(format!("{label} produced non-finite PCM")).into());
    }
    let sample_peak = stereo
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if sample_peak <= 0.0 || sample_peak > 0.5 {
        return Err(io::Error::other(format!(
            "{label} sample peak {sample_peak:.6} is outside (0, 0.5]"
        ))
        .into());
    }
    let file = format!("gamma9-{label}.wav");
    let bytes = wav(2, &stereo)?;
    fs::write(temp.join(&file), &bytes)?;
    Ok(json!({
        "label": label,
        "file": output_path(&final_output.join(&file)),
        "sha256": sha256_hex(&bytes),
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "channels": 2,
        "frame_count": FRAMES,
        "sample_peak": sample_peak,
        "finite": true,
        "transfer_combined_gain_db": transfer.combined_gain_db(),
    }))
}

fn combined_transfer(
    stages: impl Iterator<Item = StageSpec>,
) -> Result<SpectralTransfer, Box<dyn std::error::Error>> {
    let mut transfer = SpectralTransfer::NEUTRAL;
    for spec in stages {
        transfer
            .set_stage(spec.stage, spec.gain_db)
            .map_err(debug_error)?;
    }
    Ok(transfer)
}

fn verify_composition(transfer: SpectralTransfer) -> Result<(), Box<dyn std::error::Error>> {
    for spec in STAGES {
        if transfer.stage_gain_db(spec.stage) != spec.gain_db {
            return Err(io::Error::other(format!("{} stage was not retained", spec.name)).into());
        }
    }
    for (band, actual) in transfer.combined_gain_db().into_iter().enumerate() {
        let expected = STAGES
            .iter()
            .map(|spec| f64::from(spec.gain_db[band]))
            .sum::<f64>()
            .clamp(
                f64::from(MIN_COMBINED_SPECTRAL_GAIN_DB),
                f64::from(MAX_COMBINED_SPECTRAL_GAIN_DB),
            ) as f32;
        if actual.to_bits() != expected.to_bits() {
            return Err(io::Error::other(format!(
                "combined band {band} was {actual}, expected {expected}"
            ))
            .into());
        }
    }
    Ok(())
}

fn source_program() -> Vec<f32> {
    let mut state = NOISE_SEED;
    (0..FRAMES)
        .map(|frame| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = ((state >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0;
            let t = frame as f32 / SAMPLE_RATE_HZ as f32;
            let tones = 0.035 * (std::f32::consts::TAU * 110.0 * t).sin()
                + 0.03 * (std::f32::consts::TAU * 338.0 * t).sin()
                + 0.025 * (std::f32::consts::TAU * 1_000.0 * t).sin()
                + 0.02 * (std::f32::consts::TAU * 4_000.0 * t).sin();
            let fade_in = (frame as f32 / FADE_FRAMES as f32).clamp(0.0, 1.0);
            let remaining = FRAMES - frame;
            let fade_out = (remaining as f32 / FADE_FRAMES as f32).clamp(0.0, 1.0);
            (tones + 0.025 * noise) * fade_in.min(fade_out)
        })
        .collect()
}

fn source_descriptor(source: &[f32]) -> Value {
    let pcm_bytes = source
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
    json!({
        "schema_version": "fightbox.deterministic-probe-source.v1",
        "source_id": SOURCE_ID,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "channels": 1,
        "frame_count": FRAMES,
        "noise": {"algorithm": "xorshift64", "seed_hex": format!("{NOISE_SEED:016x}"), "amplitude": 0.025},
        "tones": [
            {"frequency_hz": 110.0, "amplitude": 0.035},
            {"frequency_hz": 338.0, "amplitude": 0.03},
            {"frequency_hz": 1000.0, "amplitude": 0.025},
            {"frequency_hz": 4000.0, "amplitude": 0.02},
        ],
        "fade_frames": FADE_FRAMES,
        "pcm_f32le_sha256": sha256_hex(&pcm_bytes),
    })
}

fn stage_json(spec: StageSpec) -> Value {
    json!({"name": spec.name, "stage": format!("{:?}", spec.stage), "gain_db": spec.gain_db})
}

fn wav(channels: u16, samples: &[f32]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    write_wav(
        WavSpec {
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels,
        },
        samples,
    )
    .map_err(|error| io::Error::other(error.as_str()).into())
}

fn parse_output() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--output")) {
        return Err(io::Error::other("usage: gamma9_capture --output ABSOLUTE_DIRECTORY").into());
    }
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("--output requires a directory"))?;
    if args.next().is_some() || !output.is_absolute() || output.exists() {
        return Err(io::Error::other("output must be one absent absolute directory").into());
    }
    Ok(output)
}

fn prepare_atomic_output(output: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let parent = output
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| io::Error::other("output parent must exist"))?;
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("output name is not UTF-8"))?;
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    if temp.exists() {
        return Err(io::Error::other("temporary output already exists").into());
    }
    fs::create_dir(&temp)?;
    Ok(temp)
}

fn debug_error(error: impl std::fmt::Debug) -> io::Error {
    io::Error::other(format!("{error:?}"))
}

fn output_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_source_repeats_exactly() {
        let first = source_program();
        let second = source_program();
        assert_eq!(first, second);
        assert_eq!(first.len(), FRAMES);
        assert!(first.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn all_named_stages_compose_once_independent_of_order() {
        let forward = combined_transfer(STAGES.into_iter()).unwrap();
        let reverse = combined_transfer(STAGES.into_iter().rev()).unwrap();
        assert_eq!(forward, reverse);
        verify_composition(forward).unwrap();
    }
}
