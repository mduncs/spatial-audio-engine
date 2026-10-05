//! Deterministic offline γ7 owner-home aperture stem capture.
//!
//! This exercises the public enclosure, composed spectral-filter, and shared
//! diffuse-field implementations. It writes evidence only; it never opens an
//! audio device and is not a linked-backend, device, or listening pass.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fightbox_api::{
    EnuVector3,
    diffuse::DiffuseFieldProfile,
    enclosure::{
        AcousticZone, AcousticZoneId, AcousticZoneKind, AxisAlignedZoneBounds, EXTERIOR_ZONE_ID,
        EnclosureProvenance, StaticPortal, StaticPortalId, StaticPortalState,
    },
    spectral::{SPECTRAL_BAND_COUNT, SpectralTransfer},
};
use fightbox_evidence::{WavSpec, sha256_hex, write_wav};
use fightbox_runtime::{
    EnclosureAuthority, EnclosureEvaluation, EnclosureScene, SharedDiffuseField,
    SpectralTransferFilter,
};
use serde_json::{Value, json};

const SAMPLE_RATE_HZ: u32 = 48_000;
const BLOCK_FRAMES: usize = 128;
const STATE_SECONDS: usize = 4;
const NAMED_STATE_COUNT: usize = 4;
const TAIL_SECONDS: usize = 4;
const ACTIVE_FRAMES: usize = SAMPLE_RATE_HZ as usize * STATE_SECONDS * NAMED_STATE_COUNT;
const TOTAL_FRAMES: usize = ACTIVE_FRAMES + SAMPLE_RATE_HZ as usize * TAIL_SECONDS;
const FADE_FRAMES: usize = 960;
const LISTENER_SPEED_MPS: f32 = 1.4;
const HOME: AcousticZoneId = AcousticZoneId(1);
const DOOR: StaticPortalId = StaticPortalId(1);
const SOURCE_ID: &str = "gamma7-deterministic-broadband-probe-v1";
const NOISE_SEED: u64 = 0x8f4d_2b31_c7a6_1905;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = parse_output()?;
    let temp = prepare_atomic_output(&output)?;
    let result = capture(&temp, &output);
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&temp);
        return Err(error);
    }
    fs::rename(&temp, &output)?;
    let report = output.join("report.json");
    let report_sha256 = sha256_hex(&fs::read(&report)?);
    println!(
        "{}",
        serde_json::to_string(&json!({
            "status": "artifact_generated",
            "report": output_path(&report),
            "sha256": report_sha256,
        }))?
    );
    Ok(())
}

fn parse_output() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--output")) {
        return Err(io::Error::other("usage: gamma7_capture --output ABSOLUTE_DIRECTORY").into());
    }
    let output = args
        .next()
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("--output requires a directory"))?;
    if args.next().is_some() || !output.is_absolute() {
        return Err(io::Error::other("--output must be the only argument and be absolute").into());
    }
    if output.exists() {
        return Err(
            io::Error::other(format!("output already exists: {}", output.display())).into(),
        );
    }
    Ok(output)
}

fn prepare_atomic_output(output: &Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let parent = output
        .parent()
        .ok_or_else(|| io::Error::other("output has no parent"))?;
    if !parent.is_dir() {
        return Err(io::Error::other("output parent must already exist").into());
    }
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::other("output name is not UTF-8"))?;
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    if temp.exists() {
        return Err(io::Error::other(format!(
            "temporary output already exists: {}",
            temp.display()
        ))
        .into());
    }
    fs::create_dir(&temp)?;
    Ok(temp)
}

fn capture(temp: &Path, final_output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source = source_program();
    let source_descriptor = source_descriptor(&source);
    let source_descriptor_bytes = serde_json::to_vec_pretty(&source_descriptor)?;
    fs::write(temp.join("source.json"), &source_descriptor_bytes)?;
    let source_descriptor_sha256 = sha256_hex(&source_descriptor_bytes);

    let source_wav = write_wav(
        WavSpec {
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: 1,
        },
        &source,
    )
    .map_err(|error| io::Error::other(error.as_str()))?;
    fs::write(temp.join("source-mono.wav"), &source_wav)?;
    let source_wav_sha256 = sha256_hex(&source_wav);

    let open_scene = EnclosureScene::new(vec![home()], vec![door()]).map_err(debug_error)?;
    let closed_scene = EnclosureScene::new(vec![home()], vec![]).map_err(debug_error)?;
    let mut filter = SpectralTransferFilter::new(SAMPLE_RATE_HZ).map_err(debug_error)?;
    let mut diffuse =
        SharedDiffuseField::new(SAMPLE_RATE_HZ, DiffuseFieldProfile::OFF).map_err(debug_error)?;
    let mut installed_profile = DiffuseFieldProfile::OFF;
    let mut stereo = Vec::with_capacity(TOTAL_FRAMES * 2);
    let mut named_states = Vec::new();
    let mut previous_doorway_gain: Option<[f32; SPECTRAL_BAND_COUNT]> = None;
    let mut maximum_doorway_target_step_db = 0.0_f32;

    for block_start in (0..TOTAL_FRAMES).step_by(BLOCK_FRAMES) {
        let (state, evaluation) = evaluation_for_block(block_start, &open_scene, &closed_scene)?;
        if block_start % (STATE_SECONDS * SAMPLE_RATE_HZ as usize) == 0
            && block_start < ACTIVE_FRAMES
        {
            named_states.push(state_observation(state, block_start, evaluation));
        }
        if state == "doorway" {
            if let Some(previous) = previous_doorway_gain {
                for (before, after) in previous.into_iter().zip(evaluation.enclosure_gain_db) {
                    maximum_doorway_target_step_db =
                        maximum_doorway_target_step_db.max((after - before).abs());
                }
            }
            previous_doorway_gain = Some(evaluation.enclosure_gain_db);
        }
        let mut transfer = SpectralTransfer::NEUTRAL;
        evaluation.publish(&mut transfer).map_err(debug_error)?;
        filter.set_transfer_smoothed(transfer);
        if evaluation.diffuse_field != installed_profile {
            diffuse
                .set_profile(evaluation.diffuse_field)
                .map_err(debug_error)?;
            installed_profile = evaluation.diffuse_field;
        }

        let input = &source[block_start..block_start + BLOCK_FRAMES];
        let mut direct = [0.0_f32; BLOCK_FRAMES];
        for (output, &sample) in direct.iter_mut().zip(input) {
            *output = filter.process_sample(sample);
        }
        let mut diffuse_left = [0.0_f32; BLOCK_FRAMES];
        let mut diffuse_right = [0.0_f32; BLOCK_FRAMES];
        diffuse
            .process_block(input, &mut diffuse_left, &mut diffuse_right)
            .map_err(debug_error)?;
        for frame in 0..BLOCK_FRAMES {
            let centered = direct[frame] * std::f32::consts::FRAC_1_SQRT_2;
            stereo.push(centered + diffuse_left[frame]);
            stereo.push(centered + diffuse_right[frame]);
        }
    }

    if named_states.len() != NAMED_STATE_COUNT {
        return Err(io::Error::other("did not capture exactly four named states").into());
    }
    let expected_authorities = [
        EnclosureAuthority::Exterior,
        EnclosureAuthority::ZoneBoundary,
        EnclosureAuthority::OpenPortal(DOOR),
        EnclosureAuthority::OpenPortal(DOOR),
    ];
    for (state, expected) in named_states.iter().zip(expected_authorities) {
        if state["authority"] != authority_name(expected) {
            return Err(io::Error::other(format!(
                "unexpected authority for state {}: {}",
                state["name"], state["authority"]
            ))
            .into());
        }
    }
    if maximum_doorway_target_step_db > 0.25 {
        return Err(io::Error::other(format!(
            "doorway target changed {maximum_doorway_target_step_db:.6} dB in one block"
        ))
        .into());
    }
    if stereo.iter().any(|sample| !sample.is_finite()) {
        return Err(io::Error::other("capture produced non-finite PCM").into());
    }
    let sample_peak = stereo
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    if sample_peak <= 0.0 || sample_peak > 0.5 {
        return Err(io::Error::other(format!(
            "capture sample peak {sample_peak:.6} is outside (0, 0.5]"
        ))
        .into());
    }

    let stem_wav = write_wav(
        WavSpec {
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: 2,
        },
        &stereo,
    )
    .map_err(|error| io::Error::other(error.as_str()))?;
    fs::write(temp.join("gamma7-owner-home-aperture.wav"), &stem_wav)?;
    let stem_sha256 = sha256_hex(&stem_wav);
    let executable = std::env::current_exe()?.canonicalize()?;
    let executable_sha256 = sha256_hex(&fs::read(&executable)?);

    let report = json!({
        "schema_version": "fightbox.gamma7-owner-home-capture.v1",
        "status": "artifact_generated",
        "evidence_class": "portable_runtime_offline_stem",
        "provenance": {
            "source_file_sha256": sha256_hex(include_bytes!("gamma7_capture.rs")),
            "executable": output_path(&executable),
            "executable_sha256": executable_sha256,
            "command": ["cargo", "+stable", "run", "--release", "-p", "fightbox-cli", "--example", "gamma7_capture", "--", "--output", output_path(final_output)],
        },
        "source": {
            "source_id": SOURCE_ID,
            "descriptor": output_path(&final_output.join("source.json")),
            "descriptor_sha256": source_descriptor_sha256,
            "wav": output_path(&final_output.join("source-mono.wav")),
            "wav_sha256": source_wav_sha256,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "channels": 1,
            "frame_count": TOTAL_FRAMES,
        },
        "stem": {
            "file": output_path(&final_output.join("gamma7-owner-home-aperture.wav")),
            "sha256": stem_sha256,
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "channels": 2,
            "frame_count": TOTAL_FRAMES,
            "duration_seconds": TOTAL_FRAMES as f64 / SAMPLE_RATE_HZ as f64,
            "sample_peak": sample_peak,
            "finite": true,
        },
        "timeline": {
            "state_duration_seconds": STATE_SECONDS,
            "named_states": named_states,
            "tail": {"start_frame": ACTIVE_FRAMES, "duration_seconds": TAIL_SECONDS, "source_silent": true},
            "doorway_listener_speed_mps": LISTENER_SPEED_MPS,
            "doorway_max_adjacent_target_step_db": maximum_doorway_target_step_db,
            "doorway_step_limit_db": 0.25,
        },
        "mechanical_authority": {
            "report": "/path/to/spatial-audio/evidence/wave17-gamma7-mechanical-20260811T123028Z/gamma7-mechanical-report.json",
            "sha256": "76a7213c969dab0aa3524abf6bad443b3688d57d5137340dea0c0eb684c13e02",
        },
        "resources": null,
        "listening": {"status": "pending", "listener_id": "", "outcome": "pending"},
        "claims": [
            "deterministic source and stereo stem generated through public enclosure, SpectralTransferFilter, and SharedDiffuseField implementations",
            "four named states and canonical 1.4 m/s doorway target continuity",
            "no audio device opened",
        ],
        "non_claims": [
            "not a callback timing, RSS, linked Steam Audio, package, bake, route, device, thermal, AirPods, head-pose, HRTF, or human listening result",
            "sample peak is not an oversampled true-peak measurement",
            "artifact_generated is not gamma-card captured or passed status",
        ],
    });
    fs::write(
        temp.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn source_program() -> Vec<f32> {
    let mut state = NOISE_SEED;
    (0..TOTAL_FRAMES)
        .map(|frame| {
            if frame >= ACTIVE_FRAMES {
                return 0.0;
            }
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
            let remaining = ACTIVE_FRAMES - frame;
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
        "frame_count": TOTAL_FRAMES,
        "active_frames": ACTIVE_FRAMES,
        "tail_silence_frames": TOTAL_FRAMES - ACTIVE_FRAMES,
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

fn evaluation_for_block(
    block_start: usize,
    open_scene: &EnclosureScene,
    closed_scene: &EnclosureScene,
) -> Result<(&'static str, EnclosureEvaluation), Box<dyn std::error::Error>> {
    let state_frames = STATE_SECONDS * SAMPLE_RATE_HZ as usize;
    let (name, scene, listener, source) = match block_start / state_frames {
        0 => (
            "exterior",
            open_scene,
            EnuVector3::new(-5.0, 5.0, 1.5),
            EnuVector3::new(-10.0, 5.0, 1.5),
        ),
        1 => (
            "closed_facade",
            closed_scene,
            EnuVector3::new(5.0, 5.0, 1.5),
            EnuVector3::new(-5.0, 5.0, 1.5),
        ),
        2 => (
            "open_window",
            open_scene,
            EnuVector3::new(5.0, 5.0, 1.5),
            EnuVector3::new(-5.0, 5.0, 1.5),
        ),
        3 => {
            let elapsed = (block_start - state_frames * 3) as f32 / SAMPLE_RATE_HZ as f32;
            let listener_x = -1.4 + LISTENER_SPEED_MPS * elapsed;
            (
                "doorway",
                open_scene,
                EnuVector3::new(listener_x, 5.0, 1.5),
                EnuVector3::new(5.0, 5.0, 1.5),
            )
        }
        _ => (
            "tail",
            open_scene,
            EnuVector3::new(4.2, 5.0, 1.5),
            EnuVector3::new(5.0, 5.0, 1.5),
        ),
    };
    Ok((
        name,
        scene.evaluate(listener, source, 0.0).map_err(debug_error)?,
    ))
}

fn state_observation(name: &str, start_frame: usize, evaluation: EnclosureEvaluation) -> Value {
    json!({
        "name": name,
        "start_frame": start_frame,
        "start_seconds": start_frame as f64 / SAMPLE_RATE_HZ as f64,
        "authority": authority_name(evaluation.authority),
        "gain_4khz_db": evaluation.enclosure_gain_db[5],
        "diffuse_wet_gain": evaluation.diffuse_field.wet_gain,
    })
}

fn authority_name(authority: EnclosureAuthority) -> &'static str {
    match authority {
        EnclosureAuthority::Exterior => "Exterior",
        EnclosureAuthority::SameZone => "SameZone",
        EnclosureAuthority::ZoneBoundary => "ZoneBoundary",
        EnclosureAuthority::OpenPortal(_) => "OpenPortal",
        EnclosureAuthority::ClosedPortal(_) => "ClosedPortal",
    }
}

fn home() -> AcousticZone {
    AcousticZone {
        id: HOME,
        kind: AcousticZoneKind::Interior,
        bounds: AxisAlignedZoneBounds {
            minimum: EnuVector3::new(0.0, 0.0, 0.0),
            maximum: EnuVector3::new(10.0, 10.0, 3.0),
        },
        priority: 1,
        transition_depth_m: 1.0,
        boundary_gain_db: [-8.0, -9.0, -11.0, -14.0, -18.0, -23.0, -28.0, -32.0],
        diffuse_field: DiffuseFieldProfile::SMALL_INTERIOR,
        provenance: EnclosureProvenance::AuthoredStatic,
    }
}

fn door() -> StaticPortal {
    StaticPortal {
        id: DOOR,
        zone_a: EXTERIOR_ZONE_ID,
        zone_b: HOME,
        center: EnuVector3::new(0.0, 5.0, 1.2),
        normal: EnuVector3::new(1.0, 0.0, 0.0),
        up: EnuVector3::new(0.0, 0.0, 1.0),
        half_width_m: 0.55,
        half_height_m: 1.2,
        edge_transition_m: 0.1,
        state: StaticPortalState::Open,
        open_gain_db: [-0.5, -0.5, -0.7, -1.0, -1.3, -1.8, -2.5, -3.0],
        closed_gain_db: [-6.0, -7.0, -9.0, -12.0, -16.0, -21.0, -27.0, -32.0],
        provenance: EnclosureProvenance::AuthoredStatic,
    }
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
    fn deterministic_source_contract_is_frozen() {
        let source = source_program();
        assert_eq!(source.len(), TOTAL_FRAMES);
        assert!(source[ACTIVE_FRAMES..].iter().all(|sample| *sample == 0.0));
        let pcm_bytes = source
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(
            sha256_hex(&pcm_bytes),
            "dc82311d329605af327840083c04d6ab6387d3c74f3b250cde8544cc516e881e"
        );
    }

    #[test]
    fn named_state_authorities_match_the_gamma7_contract() {
        let open_scene = EnclosureScene::new(vec![home()], vec![door()]).unwrap();
        let closed_scene = EnclosureScene::new(vec![home()], vec![]).unwrap();
        let state_frames = STATE_SECONDS * SAMPLE_RATE_HZ as usize;
        let expected = [
            ("exterior", "Exterior"),
            ("closed_facade", "ZoneBoundary"),
            ("open_window", "OpenPortal"),
            ("doorway", "OpenPortal"),
        ];
        for (index, (expected_name, expected_authority)) in expected.into_iter().enumerate() {
            let (name, evaluation) =
                evaluation_for_block(index * state_frames, &open_scene, &closed_scene).unwrap();
            assert_eq!(name, expected_name);
            assert_eq!(authority_name(evaluation.authority), expected_authority);
        }
    }
}
