//! `fightbox phase-a s0` — render the free-field approach through the real S0
//! backend.
//!
//! For every declared trajectory distance, the command renders the calibrated
//! mono PCM through Steam Audio's direct/binaural chain with air absorption
//! enabled, plus a same-pose 100 m control with air absorption disabled. It
//! writes one finite stereo WAV per trajectory point plus separate
//! air-enabled/air-disabled 100 m WAVs, completes the capture provenance, and
//! records per-distance PCM RMS, backend distance attenuation, air coefficients,
//! and a bounded high-band energy measurement. The actual PCM must be monotonic
//! nondecreasing from 100 m to 1 m, the inverse-distance contribution is about
//! +40 dB (tolerance stated), and enabled air absorption does not increase
//! high-band energy at the same 100 m pose.

use std::path::Path;

use fightbox_steam_audio::{
    AudioConfig, BackendError, EnuVector3, ListenerPose, S0RenderOutput, S0RenderRequest, render_s0,
};

use crate::asset::{AssetDescriptor, ResolvedAsset};
use crate::atomicio::{
    self, AtomicDir, WrittenWav, validate_output_path, write_json_atomic, write_json_string_atomic,
};
use crate::bundle::{BundleFile, BundleManifest, S0Metrics, S0TrajectoryMetric};
use crate::error::{CliError, Result};
use crate::fixture::{Fixture, Gate, distance};
use crate::metrics::{
    calibration_payload, from_channel_metrics, high_band_rms, stereo_channel_metric,
};
use crate::provenance::{
    self, ENGINE_IDENTITY, NO_DELIVERED_EAR_SPL_NONCLAIM, REMAINING_PHASE_A_GATES_NONCLAIM,
    SdkBinary, UNCOMMITTED_SOURCE_NONCLAIM,
};

/// Run the S0 capture. `out` is the bundle directory to create atomically.
pub fn run(fixture_path: &Path, out: &Path) -> Result<()> {
    require_linked()?;
    let fixture = load_fixture(fixture_path)?;
    let asset = load_asset(&fixture)?;
    let out = validate_output_path(out)?;
    let sdk = SdkBinary::detect();
    // An artifact-producing capture is invalid unless the actual dylib path and
    // checksum were established from STEAM_AUDIO_SDK_DIR at runtime.
    sdk.require_established()?;

    let input_mono = regenerate_input(&asset)?;
    let calibration = crate::calibrate::CalibratedSource::derive_from_analysis(
        &fixture,
        &analyze(&asset, &input_mono)?,
    )?;
    let canonical = calibration.assert_canonical_85db_minus_20()?;

    let audio = AudioConfig {
        sample_rate_hz: asset.descriptor.sample_rate_hz as i32,
        frame_size: 128,
    };

    let mut trajectory_metrics = Vec::new();
    let mut written_wavs: Vec<(String, WrittenWav)> = Vec::new();

    let dir = AtomicDir::create(out.clone())?;
    let temp = dir.temp_path().to_path_buf();

    for (index, point) in fixture.listener.trajectory_m.iter().enumerate() {
        let listener = ListenerPose {
            position_enu: to_enu(*point),
            ahead_enu: to_enu(fixture.listener.forward_enu),
            up_enu: to_enu(fixture.listener.up_enu),
        };
        let distance_m = distance(fixture.source.position_m, *point) as f32;
        let file_name = format!("approach-{index:02}-{}m.wav", meters_label(distance_m));
        let output = render_one(
            &audio,
            &fixture,
            &listener,
            &input_mono,
            calibration.drive.linear_gain(),
            true,
        )?;
        let written = atomicio::write_stereo_wav(
            &temp,
            &file_name,
            audio.sample_rate_hz,
            &output.stereo.interleaved,
        )?;
        let channel =
            stereo_channel_metric(audio.sample_rate_hz as u32, &output.stereo.interleaved)?;
        trajectory_metrics.push(S0TrajectoryMetric {
            distance_m,
            index,
            air_absorption_enabled: true,
            channel,
            distance_attenuation: output.distance_attenuation,
            air_absorption: output.air_absorption,
            relative_direction_steam: [
                output.relative_direction_steam.x,
                output.relative_direction_steam.y,
                output.relative_direction_steam.z,
            ],
        });
        written_wavs.push((file_name, written));
    }

    // 100 m control with air absorption disabled, same pose as the 100 m
    // trajectory point. The fixture's first trajectory point is at 100 m.
    let control_pose = fixture.listener.trajectory_m[0];
    let control_listener = ListenerPose {
        position_enu: to_enu(control_pose),
        ahead_enu: to_enu(fixture.listener.forward_enu),
        up_enu: to_enu(fixture.listener.up_enu),
    };
    let control_100m_air_enabled = render_one(
        &audio,
        &fixture,
        &control_listener,
        &input_mono,
        calibration.drive.linear_gain(),
        true,
    )?;
    let control_100m_air_disabled = render_one(
        &audio,
        &fixture,
        &control_listener,
        &input_mono,
        calibration.drive.linear_gain(),
        false,
    )?;
    let enabled_written = atomicio::write_stereo_wav(
        &temp,
        "control-100m-air-enabled.wav",
        audio.sample_rate_hz,
        &control_100m_air_enabled.stereo.interleaved,
    )?;
    let disabled_written = atomicio::write_stereo_wav(
        &temp,
        "control-100m-air-disabled.wav",
        audio.sample_rate_hz,
        &control_100m_air_disabled.stereo.interleaved,
    )?;
    let control_metric = S0TrajectoryMetric {
        distance_m: distance(fixture.source.position_m, control_pose) as f32,
        index: 0,
        air_absorption_enabled: false,
        channel: stereo_channel_metric(
            audio.sample_rate_hz as u32,
            &control_100m_air_disabled.stereo.interleaved,
        )?,
        distance_attenuation: control_100m_air_disabled.distance_attenuation,
        air_absorption: control_100m_air_disabled.air_absorption,
        relative_direction_steam: [
            control_100m_air_disabled.relative_direction_steam.x,
            control_100m_air_disabled.relative_direction_steam.y,
            control_100m_air_disabled.relative_direction_steam.z,
        ],
    };

    // Assertions on actual PCM and backend output (authority note §ν properties).
    let inverse = assert_monotonic_and_inverse_distance(&trajectory_metrics)?;
    let high_band = assert_high_band_does_not_increase(
        audio.sample_rate_hz as u32,
        &control_100m_air_enabled.stereo.interleaved,
        &control_100m_air_disabled.stereo.interleaved,
    )?;

    // Record the exact input fixture bytes and asset descriptor.
    let fixture_bytes = std::fs::read(fixture_path).map_err(|e| {
        CliError::new(format!(
            "cannot read fixture {}: {e}",
            fixture_path.display()
        ))
    })?;
    let fixture_hash = fightbox_evidence::sha256_hex(&fixture_bytes);
    atomicio::write_bytes_plain(&temp.join("fixture.json"), &fixture_bytes)?;
    let asset_text = serialize_asset(&asset.descriptor)?;
    let asset_hash = fightbox_evidence::sha256_hex(asset_text.as_bytes());
    atomicio::write_bytes_plain(&temp.join("asset-descriptor.json"), asset_text.as_bytes())?;

    let cal_payload = calibration_payload(
        fightbox_api::SceneCalibration::DEFAULT_REFERENCE_SPL_DB,
        fightbox_api::SceneCalibration::DEFAULT_REFERENCE_PCM_RMS_DBFS,
        fightbox_api::SceneCalibration::REFERENCE_DISTANCE_M,
        canonical.program_rms_dbfs,
        canonical.target_source_rms_dbfs,
        canonical.drive_gain_db,
        canonical.linear_gain,
    );

    let metrics = S0Metrics {
        schema_version: S0Metrics::SCHEMA.into(),
        fixture_id: fixture.fixture_id.clone(),
        sample_rate_hz: audio.sample_rate_hz as u32,
        frame_count_per_distance: input_mono.len(),
        calibration: cal_payload,
        trajectory: trajectory_metrics,
        control_100m_air_disabled: control_metric,
        inverse_distance_100m_to_1m_db: inverse.delta_db,
        inverse_distance_tolerance_db: inverse.tolerance_db,
        high_band_energy: high_band,
        claims: vec![format!(
            "S0 approach PCM is monotonic nondecreasing from 100 m to 1 m within {tol:.1} dB tolerance.",
            tol = inverse.tolerance_db
        )],
        non_claims: s0_non_claims(),
    };
    write_json_atomic(&temp.join("metrics.json"), &metrics)?;

    // Build the manifest with file hashes and sizes.
    let mut files = Vec::new();
    for (name, written) in &written_wavs {
        files.push(BundleFile {
            name: name.clone(),
            kind: "approach_wav".into(),
            content_sha256: written.content_sha256.clone(),
            size_bytes: file_size(&temp.join(name))?,
        });
    }
    files.push(bundle_file(
        &temp,
        "control-100m-air-enabled.wav",
        "control_wav",
        &enabled_written,
    )?);
    files.push(bundle_file(
        &temp,
        "control-100m-air-disabled.wav",
        "control_wav",
        &disabled_written,
    )?);
    files.push(bundle_file_text(&temp, "metrics.json", "metrics")?);
    files.push(BundleFile {
        name: "fixture.json".into(),
        kind: "fixture".into(),
        content_sha256: fixture_hash,
        size_bytes: file_size(&temp.join("fixture.json"))?,
    });
    files.push(BundleFile {
        name: "asset-descriptor.json".into(),
        kind: "asset_descriptor".into(),
        content_sha256: asset_hash.clone(),
        size_bytes: file_size(&temp.join("asset-descriptor.json"))?,
    });

    // Provenance sidecar (authority note §ν). capture-provenance.json is an
    // IMMUTABLE bundle input: write it before the manifest and index it so the
    // manifest binds it (a verifier that re-parses it can cross-check the SDK
    // version, dylib checksum, and host facts against the bytes on disk).
    // listening-record.json is intentionally NOT indexed — it stays mutable for
    // the human sign-off but binds back to the manifest's unsigned digest.
    let provenance_json = build_provenance_json(&sdk, &fixture.fixture_id, "S0", None);
    write_json_string_atomic(&temp.join("capture-provenance.json"), &provenance_json)?;
    files.push(bundle_file_text(
        &temp,
        "capture-provenance.json",
        "capture_provenance",
    )?);

    let mut manifest = BundleManifest {
        schema_version: BundleManifest::SCHEMA.into(),
        gate: "S0".into(),
        fixture_id: fixture.fixture_id.clone(),
        fixture_content_sha256: files
            .iter()
            .find(|f| f.name == "fixture.json")
            .map(|f| f.content_sha256.clone())
            .unwrap_or_default(),
        asset_id: asset.descriptor.asset_id.clone(),
        asset_descriptor_sha256: asset_hash,
        files,
        unsigned_manifest_sha256: None,
        manifest_content_sha256: None,
    };

    // Compute the canonical unsigned digest (stable, recomputable) BEFORE the
    // final bytes exist. Write the manifest exactly once with the unsigned
    // digest (and its legacy alias) populated, then record the detached
    // final-file digest in a separate sidecar so the on-disk manifest bytes are
    // stable. A JSON object cannot contain the SHA-256 of its own final bytes,
    // so the finalized digest is deliberately detached, not in-manifest.
    let unsigned_digest = manifest.recompute_unsigned_digest();
    manifest.unsigned_manifest_sha256 = Some(unsigned_digest.clone());
    manifest.manifest_content_sha256 = Some(unsigned_digest.clone());
    write_json_atomic(&temp.join("manifest.json"), &manifest)?;
    let manifest_bytes = std::fs::read(temp.join("manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read manifest for hashing: {e}")))?;
    let finalized_digest = fightbox_evidence::sha256_hex(&manifest_bytes);
    atomicio::write_bytes_plain(&temp.join("manifest.sha256"), finalized_digest.as_bytes())?;
    let manifest_hash = unsigned_digest.clone();

    dir.commit()?;
    eprintln!(
        "fightbox: S0 bundle written to {} (manifest sha256: {manifest_hash})",
        out.display()
    );
    Ok(())
}

/// The S0 inverse-distance assertion result.
struct InverseDistanceAssertion {
    delta_db: f32,
    tolerance_db: f32,
}

fn assert_monotonic_and_inverse_distance(
    trajectory: &[S0TrajectoryMetric],
) -> Result<InverseDistanceAssertion> {
    if trajectory.len() < 2 {
        return Err(CliError::new(
            "S0 trajectory must have at least two points for the inverse-distance assertion",
        ));
    }
    // Trajectory is ordered from far (100 m) to near (1 m). PCM RMS must be
    // nondecreasing as the listener approaches.
    let mut previous = f32::NEG_INFINITY;
    for metric in trajectory {
        let rms = metric
            .channel
            .rms_dbfs_per_channel
            .first()
            .copied()
            .flatten()
            .ok_or_else(|| {
                CliError::new("S0 trajectory PCM is silent; cannot assert monotonicity")
            })?;
        if rms + 1.0e-3 < previous {
            return Err(CliError::new(format!(
                "S0 PCM RMS is not monotonic nondecreasing: {rms} dBFS after {previous} dBFS at {distance} m",
                distance = metric.distance_m
            )));
        }
        previous = rms;
    }
    // Inverse-distance contribution from the first (100 m) to the last (1 m)
    // point. The backend distance attenuation ratio (near/far) should be about
    // +40 dB. Use the backend attenuation values directly.
    let far_attenuation = trajectory
        .first()
        .ok_or_else(|| CliError::new("empty S0 trajectory"))?
        .distance_attenuation;
    let near_attenuation = trajectory
        .last()
        .ok_or_else(|| CliError::new("empty S0 trajectory"))?
        .distance_attenuation;
    if far_attenuation <= 0.0 || near_attenuation <= 0.0 {
        return Err(CliError::new(
            "S0 backend distance attenuation is non-positive; cannot compute inverse-distance delta",
        ));
    }
    let delta_db = 20.0 * (near_attenuation / far_attenuation).log10();
    let tolerance_db = 6.0_f32;
    let expected = 40.0_f32;
    if (delta_db - expected).abs() > tolerance_db {
        return Err(CliError::new(format!(
            "S0 inverse-distance contribution {delta_db:.2} dB is not within {tolerance_db:.1} dB of the expected {expected:.1} dB (100 m -> 1 m)"
        )));
    }
    Ok(InverseDistanceAssertion {
        delta_db,
        tolerance_db,
    })
}

fn assert_high_band_does_not_increase(
    sample_rate_hz: u32,
    enabled: &[f32],
    disabled: &[f32],
) -> Result<crate::bundle::HighBandComparison> {
    let cutoff_hz = 4_000.0_f32;
    let enabled_high = high_band_rms(sample_rate_hz, enabled, cutoff_hz);
    let disabled_high = high_band_rms(sample_rate_hz, disabled, cutoff_hz);
    let does_not_exceed = enabled_high <= disabled_high + 1.0e-6;
    if !does_not_exceed {
        return Err(CliError::new(format!(
            "enabled air absorption increased high-band energy at 100 m: enabled={enabled_high:.6} > disabled={disabled_high:.6}"
        )));
    }
    Ok(crate::bundle::HighBandComparison {
        cutoff_hz,
        enabled_air_100m_high_band_rms: enabled_high,
        disabled_air_100m_high_band_rms: disabled_high,
        enabled_does_not_exceed_disabled: true,
    })
}

fn render_one(
    audio: &AudioConfig,
    fixture: &Fixture,
    listener: &ListenerPose,
    input_mono: &[f32],
    linear_gain: f32,
    apply_air_absorption: bool,
) -> Result<S0RenderOutput> {
    if fixture.gate() != Ok(Gate::S0) {
        return Err(CliError::new("s0 command requires an S0 fixture"));
    }
    let request = S0RenderRequest {
        audio: *audio,
        source_position_enu: to_enu(fixture.source.position_m),
        listener: *listener,
        input_mono: input_mono.to_vec(),
        calibration_gain: linear_gain,
        apply_air_absorption,
    };
    render_s0(&request).map_err(map_backend_error)
}

fn map_backend_error(error: BackendError) -> CliError {
    CliError::new(format!("Steam Audio S0 render failed: {error}"))
}

fn require_linked() -> Result<()> {
    if !fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        return Err(CliError::new(
            "S0 requires a linked Steam Audio SDK; rebuild with --features linked-sdk and STEAM_AUDIO_SDK_DIR set",
        ));
    }
    Ok(())
}

fn load_fixture(path: &Path) -> Result<Fixture> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::new(format!("cannot read fixture {}: {e}", path.display())))?;
    let fixture = Fixture::parse(&text).map_err(CliError::new)?;
    if fixture.gate() != Ok(Gate::S0) {
        return Err(CliError::new(format!(
            "expected an S0 fixture, got gate {}",
            fixture.gate
        )));
    }
    Ok(fixture)
}

fn load_asset(fixture: &Fixture) -> Result<ResolvedAsset> {
    let asset_path = resolve_asset_path(fixture, std::env::current_dir().ok().as_deref())?;
    let text = std::fs::read_to_string(&asset_path)
        .map_err(|e| CliError::new(format!("cannot read asset {}: {e}", asset_path.display())))?;
    let descriptor = AssetDescriptor::parse(&text).map_err(CliError::new)?;
    descriptor.resolve().map_err(CliError::new)
}

/// Resolve a fixture's `asset_id` to `fixtures/assets/<asset_id>.json` relative
/// to the fixture repository layout. The fixture path is
/// `<repo>/fixtures/<gate>/fixture.json`, so the assets directory is three
/// levels up then into `fixtures/assets`.
pub(crate) fn resolve_asset_path(
    fixture: &Fixture,
    cwd: Option<&std::path::Path>,
) -> Result<std::path::PathBuf> {
    let asset_id = &fixture.source.asset_id;
    // Prefer a `fixtures/assets/<id>.json` relative to the current working
    // directory, which matches the documented repository layout and lets the
    // command run from the repo root regardless of where the fixture file lives.
    if let Some(cwd) = cwd {
        let candidate = cwd
            .join("fixtures")
            .join("assets")
            .join(format!("{asset_id}.json"));
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(CliError::new(format!(
        "cannot resolve fixtures/assets/{asset_id}.json relative to the current directory; run from the repository root"
    )))
}

fn regenerate_input(asset: &ResolvedAsset) -> Result<Vec<f32>> {
    let (signal, _) = asset.regenerate_mono().map_err(CliError::new)?;
    Ok(signal.samples)
}

fn analyze(asset: &ResolvedAsset, _input_mono: &[f32]) -> Result<fightbox_evidence::AnalyzedAsset> {
    let (_, analysis) = asset.regenerate_mono().map_err(CliError::new)?;
    Ok(analysis)
}

fn serialize_asset(descriptor: &crate::asset::AssetDescriptor) -> Result<String> {
    serde_json::to_string_pretty(descriptor)
        .map_err(|e| CliError::new(format!("asset serialize: {e}")))
}

fn to_enu(vector: crate::fixture::Vec3) -> EnuVector3 {
    let [x, y, z] = vector.to_f32();
    EnuVector3::new(x, y, z)
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .map_err(|e| CliError::new(format!("cannot stat {}: {e}", path.display())))?
        .len())
}

fn bundle_file(dir: &Path, name: &str, kind: &str, written: &WrittenWav) -> Result<BundleFile> {
    Ok(BundleFile {
        name: name.into(),
        kind: kind.into(),
        content_sha256: written.content_sha256.clone(),
        size_bytes: file_size(&dir.join(name))?,
    })
}

fn bundle_file_text(dir: &Path, name: &str, kind: &str) -> Result<BundleFile> {
    let bytes = std::fs::read(dir.join(name))
        .map_err(|e| CliError::new(format!("cannot read {name}: {e}")))?;
    Ok(BundleFile {
        name: name.into(),
        kind: kind.into(),
        content_sha256: fightbox_evidence::sha256_hex(&bytes),
        size_bytes: bytes.len() as u64,
    })
}

fn s0_non_claims() -> Vec<String> {
    vec![
        NO_DELIVERED_EAR_SPL_NONCLAIM.into(),
        UNCOMMITTED_SOURCE_NONCLAIM.into(),
        REMAINING_PHASE_A_GATES_NONCLAIM.into(),
        "This S0 capture does not establish audible quality or pathing.".into(),
    ]
}

fn meters_label(distance_m: f32) -> String {
    if (distance_m - distance_m.round()).abs() < 1.0e-3 {
        format!("{}", distance_m.round() as i64)
    } else {
        format!("{distance_m:.2}")
    }
}

/// Build the authority-note §ν provenance sidecar. Shared by S0 and S3 bundles.
pub(crate) fn build_provenance_json(
    sdk: &SdkBinary,
    fixture_id: &str,
    gate: &str,
    world_dir: Option<&str>,
) -> String {
    let mut object = serde_json::json!({
        "engine_identity": ENGINE_IDENTITY,
        "platform": provenance::platform(),
        "cpu_class": provenance::cpu_class(),
        "hrtf_identity": provenance::HRTF_IDENTITY,
        "fixture_id": fixture_id,
        "gate": gate,
        "steam_audio_version": sdk.version,
        "steam_audio_upstream_commit": sdk.upstream_commit,
        "binary_checksum_sha256": sdk.dylib_checksum_sha256,
        "dylib_path": sdk.dylib_path.as_deref().map(|p| p.to_string_lossy().into_owned()),
        // Authority-note §ν build profile. Phase A offline capture fixes a
        // 48 kHz / 128-frame binaural profile at the "phase-a" quality tier;
        // streaming and real-time callbacks are genuinely not applicable.
        "build_profile": "phase-a-offline",
        "sample_rate_hz": 48_000,
        "block_size_frames": 128,
        "requested_quality": "phase-a",
        "delivered_quality": "phase-a",
        "streaming_cadence": "not_applicable",
        "callback_timing": "not_applicable",
        "non_claims": [
            UNCOMMITTED_SOURCE_NONCLAIM,
            NO_DELIVERED_EAR_SPL_NONCLAIM,
            REMAINING_PHASE_A_GATES_NONCLAIM,
        ],
    });
    if let Some(world) = world_dir {
        object["world_dir"] = serde_json::Value::String(world.into());
    }
    serde_json::to_string_pretty(&object).expect("provenance JSON must serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::test_fixtures;

    #[test]
    fn resolves_s0_asset_from_repo_layout() {
        let fixture = test_fixtures::s0();
        let cwd = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../");
        let path = resolve_asset_path(&fixture, Some(&cwd)).unwrap();
        assert!(path.ends_with("fixtures/assets/s0-calibrated-pink.json"));
    }

    #[test]
    fn meters_label_rounds_clean_distances() {
        assert_eq!(meters_label(100.0), "100");
        assert_eq!(meters_label(1.0), "1");
        assert_eq!(meters_label(1.5), "1.50");
    }
}

// Silence the unused `from_channel_metrics` import warning; it is used by the
// shared metrics module consumers (verify, s3).
#[allow(dead_code)]
fn _channel_metric_alias(
    metrics: &fightbox_evidence::ChannelMetrics,
) -> crate::bundle::ChannelMetricPayload {
    from_channel_metrics(metrics)
}
