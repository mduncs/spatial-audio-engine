//! Deterministic offline γ0 macro-transport pulse capture.
//!
//! It exercises public macro planning/scheduling at 100 m, 1 km, and 10 km,
//! then writes mono physical-path stems. It never opens an audio device.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fightbox_api::EnuVector3;
use fightbox_api::atmosphere::AtmosphereObservation;
use fightbox_api::macro_transport::{
    EventRole, MacroAssetTransport, MacroEmitter, MacroEventId, MacroListener, MacroTransportConfig,
};
use fightbox_api::spectral::SpectralTransfer;
use fightbox_evidence::{WavSpec, sha256_hex, write_wav};
use fightbox_runtime::{
    FrozenAtmosphere, MACRO_SPEED_OF_SOUND_MPS, MacroEventScheduleRequest, SpectralTransferFilter,
    plan_macro_transport,
};
use serde_json::{Value, json};

const SAMPLE_RATE_HZ: u32 = 48_000;
const EMISSION_FRAME: u64 = SAMPLE_RATE_HZ as u64;
const LOCAL_HORIZON_M: f32 = 600.0;
const SOURCE_FRAMES: usize = 12_000;
const SOURCE_ID: &str = "gamma0-deterministic-transport-pulse-v1";
const ATMOSPHERE_ID: &str = "gamma0-diagnostic-20c-50rh-101325pa-v1";
const DISTANCES_M: [f32; 3] = [100.0, 1_000.0, 10_000.0];

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
    let observation = AtmosphereObservation::new(20.0, 50.0, 101.325).map_err(debug_error)?;
    let atmosphere = FrozenAtmosphere::freeze(Some(observation));
    let source = source_program();
    let source_pcm = source
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect::<Vec<_>>();
    let source_descriptor = json!({
        "schema_version": "fightbox.gamma0-transport-source.v1",
        "source_id": SOURCE_ID,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "channels": 1,
        "frame_count": SOURCE_FRAMES,
        "algorithm": "fixed biphasic low-body plus seeded damped broadband pulse",
        "seed_hex": "d4179b0e60c2a351",
        "pcm_f32le_sha256": sha256_hex(&source_pcm),
    });
    let source_descriptor_bytes = serde_json::to_vec_pretty(&source_descriptor)?;
    fs::write(temp.join("source.json"), &source_descriptor_bytes)?;
    let source_wav = wav(&source)?;
    fs::write(temp.join("source-mono.wav"), &source_wav)?;

    let coefficient_bytes = atmosphere
        .absorption_db_per_meter()
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>();
    let coefficient_sha256 = sha256_hex(&coefficient_bytes);
    let mut stems = Vec::new();
    for (index, distance_m) in DISTANCES_M.into_iter().enumerate() {
        stems.push(render_distance(
            temp,
            final_output,
            index,
            distance_m,
            &source,
            &atmosphere,
        )?);
    }

    let executable = std::env::current_exe()?.canonicalize()?;
    let report = json!({
        "schema_version": "fightbox.gamma0-transport-pulse-capture.v1",
        "status": "artifact_generated",
        "evidence_class": "portable_runtime_offline_macro_schedule_and_physical_path_stems",
        "provenance": {
            "source_file_sha256": sha256_hex(include_bytes!("gamma0_capture.rs")),
            "executable": output_path(&executable),
            "executable_sha256": sha256_hex(&fs::read(&executable)?),
            "command": ["cargo", "+stable", "run", "--release", "-p", "fightbox-cli", "--example", "gamma0_capture", "--", "--output", output_path(final_output)],
        },
        "source": {
            "source_id": SOURCE_ID,
            "descriptor": output_path(&final_output.join("source.json")),
            "descriptor_sha256": sha256_hex(&source_descriptor_bytes),
            "wav": output_path(&final_output.join("source-mono.wav")),
            "wav_sha256": sha256_hex(&source_wav),
        },
        "atmosphere": {
            "observation_id": ATMOSPHERE_ID,
            "provenance": atmosphere.provenance().stable_label(),
            "temperature_c": observation.temperature_c,
            "relative_humidity_percent": observation.relative_humidity_percent,
            "pressure_pa": observation.pressure_kpa * 1_000.0,
            "absorption_db_per_meter": atmosphere.absorption_db_per_meter(),
            "coefficient_sha256": coefficient_sha256,
            "purpose": "explicit frozen diagnostic card observation, not current local weather",
        },
        "local_horizon_m": LOCAL_HORIZON_M,
        "speed_of_sound_mps": MACRO_SPEED_OF_SOUND_MPS,
        "emission_frame": EMISSION_FRAME,
        "stems": stems,
        "resources": null,
        "listening": {"status": "pending", "listener_id": "", "outcome": "pending"},
        "claims": [
            "public macro planner and event scheduler preserve one end-to-end clock at 100 m, 1 km, and 10 km",
            "each physical-path mono stem contains exactly one scheduled source pulse",
            "macro plus local distance gain and one full-distance atmosphere transfer are applied once",
            "no audio device opened",
        ],
        "non_claims": [
            "offline reconstructed ingress stems are not a live Workbench macro-ingress callback capture",
            "diagnostic atmosphere is not current weather",
            "not playback, human listening, linked backend, callback timing/RSS, device, thermal, AirPods, HRTF, or delivered-ear-SPL evidence",
            "artifact_generated is not gamma-card captured or passed status",
        ],
    });
    fs::write(
        temp.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(())
}

fn render_distance(
    temp: &Path,
    final_output: &Path,
    index: usize,
    distance_m: f32,
    source: &[f32],
    atmosphere: &FrozenAtmosphere,
) -> Result<Value, Box<dyn std::error::Error>> {
    let event_id = MacroEventId(index as u64 + 1);
    let plan = plan_macro_transport(
        MacroEmitter {
            id: event_id,
            position_enu: EnuVector3::new(distance_m, 0.0, 0.0),
            program_started_at_s: 0.0,
            asset_transport: MacroAssetTransport::DeterministicGenerator,
            recording_carries_motion: false,
        },
        MacroListener {
            position_enu: EnuVector3::new(0.0, 0.0, 0.0),
            session_time_s: 0.0,
        },
        MacroTransportConfig {
            local_horizon_m: LOCAL_HORIZON_M,
        },
        atmosphere,
    )
    .map_err(debug_error)?;
    let event = plan
        .schedule_event(MacroEventScheduleRequest {
            event_id,
            atomic_group_id: event_id.0,
            role: EventRole::StandardImpulse,
            asset_key: 0x4700 + event_id.0,
            emission_frame: EMISSION_FRAME,
            program_seek_frame: 0,
            retained_frames_after_activation: SOURCE_FRAMES as u64,
            sample_rate_hz: SAMPLE_RATE_HZ,
        })
        .map_err(debug_error)?;
    let local_delay_frames =
        (plan.local_segment.delay_s * f64::from(SAMPLE_RATE_HZ)).round() as u64;
    let arrival_frame = event
        .ingress_activation_frame
        .checked_add(local_delay_frames)
        .ok_or_else(|| io::Error::other("arrival frame overflow"))?;
    let direct_arrival =
        EMISSION_FRAME + (plan.total_delay_s * f64::from(SAMPLE_RATE_HZ)).round() as u64;
    let arrival_delta_frames = arrival_frame.abs_diff(direct_arrival);
    if arrival_delta_frames > 1 {
        return Err(io::Error::other(format!(
            "partitioned arrival differs by {arrival_delta_frames} frames"
        ))
        .into());
    }

    let mut transfer = SpectralTransfer::NEUTRAL;
    plan.publish_atmosphere(&mut transfer)
        .map_err(debug_error)?;
    let mut filter = SpectralTransferFilter::new(SAMPLE_RATE_HZ).map_err(debug_error)?;
    filter.set_transfer(transfer);
    let mut output = vec![0.0_f32; arrival_frame as usize + source.len() + 12_000];
    for (offset, sample) in source.iter().copied().enumerate() {
        output[arrival_frame as usize + offset] =
            filter.process_sample(sample) * plan.composed_distance_gain();
    }
    if output.iter().any(|sample| !sample.is_finite()) {
        return Err(io::Error::other("transport stem produced non-finite PCM").into());
    }
    let active_frames = output
        .iter()
        .enumerate()
        .filter_map(|(frame, sample)| (sample.abs() > f32::EPSILON).then_some(frame))
        .collect::<Vec<_>>();
    let first_active_frame = *active_frames
        .first()
        .ok_or_else(|| io::Error::other("transport stem is silent"))?;
    let sample_peak = output
        .iter()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    let label = match distance_m as u32 {
        100 => "100m",
        1_000 => "1km",
        10_000 => "10km",
        _ => return Err(io::Error::other("unexpected distance").into()),
    };
    let file = format!("gamma0-transport-{label}-mono.wav");
    let bytes = wav(&output)?;
    fs::write(temp.join(&file), &bytes)?;
    Ok(json!({
        "label": label,
        "file": output_path(&final_output.join(&file)),
        "sha256": sha256_hex(&bytes),
        "channels": 1,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "frame_count": output.len(),
        "emission_frame": EMISSION_FRAME,
        "ingress_activation_frame": event.ingress_activation_frame,
        "local_delay_frames": local_delay_frames,
        "arrival_frame": arrival_frame,
        "direct_arrival_frame": direct_arrival,
        "arrival_delta_frames": arrival_delta_frames,
        "arrival_time_s": arrival_frame as f64 / f64::from(SAMPLE_RATE_HZ),
        "first_active_frame": first_active_frame,
        "total_distance_m": plan.total_distance_m,
        "macro_distance_m": plan.macro_segment.distance_m,
        "local_distance_m": plan.local_segment.distance_m,
        "total_delay_s": plan.total_delay_s,
        "composed_distance_gain": plan.composed_distance_gain(),
        "total_atmosphere_gain_db": plan.total_atmosphere_gain_db,
        "sample_peak": sample_peak,
        "finite": true,
        "pulse_count": 1,
    }))
}

fn source_program() -> Vec<f32> {
    let mut state = 0xd417_9b0e_60c2_a351_u64;
    (0..SOURCE_FRAMES)
        .map(|frame| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = ((state >> 32) as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0;
            let t = frame as f32 / SAMPLE_RATE_HZ as f32;
            let body = (std::f32::consts::TAU * (70.0 + 90.0 * t) * t).sin();
            let edge = if frame < 96 {
                (std::f32::consts::PI * frame as f32 / 96.0).sin()
            } else {
                0.0
            };
            let decay = (-18.0 * t).exp();
            (0.45 * edge + 0.3 * body + 0.16 * noise) * decay
        })
        .collect()
}

fn wav(samples: &[f32]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    write_wav(
        WavSpec {
            sample_rate_hz: SAMPLE_RATE_HZ,
            channels: 1,
        },
        samples,
    )
    .map_err(|error| io::Error::other(error.as_str()).into())
}

fn parse_output() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--output")) {
        return Err(io::Error::other("usage: gamma0_capture --output ABSOLUTE_DIRECTORY").into());
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
        assert_eq!(first, source_program());
        assert_eq!(first.len(), SOURCE_FRAMES);
        assert!(first.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn canonical_ranges_preserve_one_partitioned_clock() {
        let observation = AtmosphereObservation::new(20.0, 50.0, 101.325).unwrap();
        let atmosphere = FrozenAtmosphere::freeze(Some(observation));
        for (index, distance_m) in DISTANCES_M.into_iter().enumerate() {
            let plan = plan_macro_transport(
                MacroEmitter {
                    id: MacroEventId(index as u64 + 1),
                    position_enu: EnuVector3::new(distance_m, 0.0, 0.0),
                    program_started_at_s: 0.0,
                    asset_transport: MacroAssetTransport::DeterministicGenerator,
                    recording_carries_motion: false,
                },
                MacroListener {
                    position_enu: EnuVector3::new(0.0, 0.0, 0.0),
                    session_time_s: 0.0,
                },
                MacroTransportConfig {
                    local_horizon_m: LOCAL_HORIZON_M,
                },
                &atmosphere,
            )
            .unwrap();
            assert!((plan.total_delay_s - f64::from(distance_m) / 343.0).abs() < 1.0e-10);
            assert!(
                (plan.macro_segment.delay_s + plan.local_segment.delay_s - plan.total_delay_s)
                    .abs()
                    < 1.0e-10
            );
        }
    }
}
