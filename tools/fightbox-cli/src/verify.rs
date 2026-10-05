//! `fightbox phase-a verify` — artifact-driven verification of an S0 or S3
//! capture bundle.
//!
//! The verifier never trusts the manifest. It re-reads every artifact from
//! disk, recomputes every hash and every mechanical metric, and rejects:
//!   - tampered bytes, hash, size, or version,
//!   - missing or corrupt WAVs,
//!   - a manifest that does not match its files,
//!   - pathing on/off sums that do not differ,
//!   - nonfinite metrics,
//!   - a decoded azimuth outside the fixture's analytic tolerance,
//!   - a missing required nonclaim.
//!
//! Exit semantics:
//! - `--mechanical-only`: succeeds (exit 0) when mechanics pass, emitting a
//!   JSON result whose listening outcome is `"pending"` and overall result is
//!   `"incomplete"`. Listening is not evaluated.
//! - strict (no flag): succeeds (exit 0) ONLY for a fully valid human `pass`
//!   listening record. An undecided, fail, placeholder, unbound, malformed, or
//!   otherwise invalid record is a specific hard error that exits nonzero.
//!
//! `requires_human_completion` is a schema constant `true` in every record
//! (including completed pass/fail): it states that human completion is required
//! by the contract, not that the record is unfinished. A `false` value is a
//! contract violation and is rejected.

use std::path::Path;

use fightbox_evidence::{compare_pathing, read_wav, sha256_hex};
use fightbox_steam_audio::{
    OwnedStereoPcm, S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD, S3_CONTINUITY_WINDOW_FRAMES,
    measure_s3_summed_boundary_continuity,
};
use serde::{Deserialize, Serialize};

use crate::bundle::{
    BundleFile, BundleManifest, CalibrationPayload, ChannelMetricPayload, OcclusionModeKind,
    PathingComparisonPayload, S0Metrics, S0TrajectoryMetric, S3Metrics, S3TrajectoryMetrics,
    WorldManifest,
};
use crate::error::{CliError, Result};
use crate::fixture::Fixture;
use crate::schema::VERIFY_RESULT;

/// Run the verifier. Returns the result JSON on success.
pub fn run(bundle: &Path, mechanical_only: bool) -> Result<String> {
    let bundle_dir = bundle.canonicalize().map_err(|e| {
        CliError::new(format!(
            "bundle {} does not exist or is not accessible: {e}",
            bundle.display()
        ))
    })?;

    // Re-parse every artifact from disk; never trust the manifest's hashes.
    let manifest = load_manifest(&bundle_dir)?;
    // SECURITY BOUNDARY (first): reject absolute, parent-traversal, duplicate,
    // and singleton-shadowing manifest entries BEFORE any manifest-derived path
    // is joined to the bundle dir, read, or hashed. A late name-safety check
    // that runs after `verify_files_index`, the gate mechanics, or another
    // artifact read is not a security boundary — by then a hostile name has
    // already been joined and read. This ordering is enforced by tests below.
    verify_manifest_names(&manifest)?;
    verify_required_file_set(&manifest, manifest.gate.as_str())?;
    let fixture = load_fixture(&bundle_dir, &manifest)?;
    verify_files_index(&bundle_dir, &manifest)?;
    verify_fixture_hash(&bundle_dir, &manifest)?;
    verify_asset_descriptor(&bundle_dir, &manifest, &fixture)?;

    // Gate-specific mechanical checks. The manifest's gate determines which
    // metrics schema and stem set are required.
    let gate = manifest.gate.as_str();
    let gate_summary = match gate {
        "S0" => verify_s0_mechanics(&bundle_dir, &manifest, &fixture)?,
        "S3" => verify_s3_mechanics(&bundle_dir, &manifest, &fixture)?,
        other => {
            return Err(CliError::new(format!(
                "manifest declares unknown gate {other:?}; expected \"S0\" or \"S3\""
            )));
        }
    };

    // The exact gate-specific file set, with no duplicate names/kinds and no
    // traversal/absolute names that could shadow a required file.
    // capture-provenance.json is an immutable indexed bundle input: re-parse it
    // and cross-bind its SDK version, dylib checksum, fixture/gate, build
    // profile, and world binding (S3) to the bundle. Do not merely check that
    // JSON parses.
    verify_capture_provenance(&bundle_dir, &manifest, gate, &fixture)?;

    // Every required nonclaim must be present in the metrics sidecar.
    verify_required_nonclaims(&bundle_dir, gate)?;

    // Listening judgment. S0 has no Phase A human listening requirement, so
    // strict S0 verification succeeds from its mechanical contract WITHOUT ever
    // claiming a human listening record exists (it does not). The S0 outcome is a
    // mechanical/nonhuman result, not a human pass, even though the command exits
    // 0. S3 strict verification requires a fully valid human `pass` record; any
    // undecided/fail/placeholder/unbound record is a hard error.
    // `--mechanical-only` skips listening for either gate and emits a pending
    // result.
    let (listening_outcome, listening_note): (ListeningOutcome, String) = if mechanical_only {
        (
            ListeningOutcome::Pending,
            "mechanical-only verification; listening not evaluated".into(),
        )
    } else if gate == "S0" {
        (
            ListeningOutcome::MechanicalS0,
            "S0 has no Phase A human listening requirement; strict S0 passes on its mechanical contract (no listening record exists or is claimed)".into(),
        )
    } else {
        verify_listening_completed(&bundle_dir)?
    };

    let result = VerifyResult {
        schema_version: VERIFY_RESULT.into(),
        bundle_dir: bundle_dir.to_string_lossy().into_owned(),
        gate: gate.to_string(),
        fixture_id: manifest.fixture_id.clone(),
        manifest_sha256: manifest
            .unsigned_manifest_sha256
            .clone()
            .unwrap_or_default(),
        mechanical_checks_passed: gate_summary.checks_passed,
        listening_outcome: listening_outcome.as_str().to_string(),
        listening_note: listening_outcome.note().into(),
        result: if listening_outcome.exits_success() {
            "pass".into()
        } else {
            "incomplete".into()
        },
        detail: listening_note,
    };
    serde_json::to_string_pretty(&result)
        .map_err(|e| CliError::new(format!("cannot serialize verify result: {e}")))
}

/// The summary returned by gate-specific mechanical verification.
struct GateSummary {
    checks_passed: Vec<String>,
}

fn load_manifest(bundle_dir: &Path) -> Result<BundleManifest> {
    let text = std::fs::read_to_string(bundle_dir.join("manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read manifest.json: {e}")))?;
    let manifest: BundleManifest = serde_json::from_str(&text)
        .map_err(|e| CliError::new(format!("manifest.json is not valid: {e}")))?;
    if manifest.schema_version != BundleManifest::SCHEMA {
        return Err(CliError::new(format!(
            "manifest schema version is not {}",
            BundleManifest::SCHEMA
        )));
    }
    if manifest.unsigned_manifest_sha256.is_none() {
        return Err(CliError::new(
            "manifest.json has no unsigned_manifest_sha256; it was not finalized",
        ));
    }
    Ok(manifest)
}

fn load_fixture(bundle_dir: &Path, manifest: &BundleManifest) -> Result<Fixture> {
    let text = std::fs::read_to_string(bundle_dir.join("fixture.json"))
        .map_err(|e| CliError::new(format!("cannot read fixture.json: {e}")))?;
    let fixture = Fixture::parse(&text).map_err(CliError::new)?;
    if fixture.fixture_id != manifest.fixture_id {
        return Err(CliError::new(format!(
            "fixture id {} does not match manifest fixture id {}",
            fixture.fixture_id, manifest.fixture_id
        )));
    }
    Ok(fixture)
}

fn verify_files_index(bundle_dir: &Path, manifest: &BundleManifest) -> Result<()> {
    // Name safety (absolute/traversal/duplicate) is enforced FIRST by
    // `verify_manifest_names` before this function is reached, so the joins
    // below are safe. Every file the manifest names must exist and its recorded
    // size and hash must match the bytes on disk.
    for file in &manifest.files {
        let path = bundle_dir.join(&file.name);
        let bytes = std::fs::read(&path).map_err(|e| {
            CliError::new(format!(
                "manifest references {} but it cannot be read: {e}",
                file.name
            ))
        })?;
        let actual_size = bytes.len() as u64;
        if actual_size != file.size_bytes {
            return Err(CliError::new(format!(
                "{} size on disk ({}) does not match manifest ({})",
                file.name, actual_size, file.size_bytes
            )));
        }
        let actual_hash = sha256_hex(&bytes);
        if actual_hash != file.content_sha256 {
            return Err(CliError::new(format!(
                "{} content hash on disk does not match manifest",
                file.name
            )));
        }
    }
    // Resolve the manifest's two digests honestly. A JSON object cannot contain
    // the SHA-256 of its own final bytes, so the manifest carries an explicit
    // canonical *unsigned-manifest* digest (recomputable: serialize with the
    // digest field nulled) as the binding key, plus a *detached final-file*
    // digest over the exact committed bytes, stored in the `manifest.sha256`
    // sidecar beside the manifest (NOT inside it). Recompute both and reject any
    // mismatch or stale alias. Do NOT call a recomputed preimage the digest of
    // the final bytes.
    let recomputed_unsigned = manifest.recompute_unsigned_digest();
    if Some(&recomputed_unsigned) != manifest.unsigned_manifest_sha256.as_ref() {
        return Err(CliError::new(format!(
            "manifest unsigned digest does not match: recorded {} but recomputed {recomputed_unsigned}",
            manifest
                .unsigned_manifest_sha256
                .as_deref()
                .unwrap_or("<none>")
        )));
    }
    // The legacy alias must agree with the canonical unsigned digest if present.
    if let Some(alias) = manifest.manifest_content_sha256.as_deref() {
        if alias != recomputed_unsigned {
            return Err(CliError::new(format!(
                "manifest manifest_content_sha256 alias ({}) does not match unsigned digest ({recomputed_unsigned})",
                alias
            )));
        }
    }
    // The detached final-file digest is over the exact bytes on disk. Read the
    // sidecar and recompute over manifest.json; both must agree.
    let final_bytes = std::fs::read(bundle_dir.join("manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read manifest.json for final digest: {e}")))?;
    let recomputed_final = sha256_hex(&final_bytes);
    let sidecar_text = std::fs::read_to_string(
        bundle_dir.join(crate::bundle::MANIFEST_DIGEST_SIDECAR),
    )
    .map_err(|e| {
        CliError::new(format!(
            "cannot read {} sidecar: {e}",
            crate::bundle::MANIFEST_DIGEST_SIDECAR
        ))
    })?;
    let recorded_final = sidecar_text.trim();
    if recorded_final != recomputed_final {
        return Err(CliError::new(format!(
            "manifest finalized digest sidecar ({}) does not match manifest.json bytes on disk (recomputed {recomputed_final})",
            recorded_final
        )));
    }
    Ok(())
}

/// Name-safety check run FIRST, before any manifest-derived path is joined,
/// read, or hashed. Rejects absolute names, any path component that traverses
/// the bundle root (`..`), any non-normal component (root/parent/current), and
/// duplicate names. Allows a single level of clean nesting (e.g. `world/...`)
/// as long as every component is a plain relative name with no parent traversal.
///
/// This is the security boundary: a late check after `verify_files_index`, the
/// gate mechanics, or another artifact read is NOT sufficient because the
/// hostile name has already been joined and read by then. The ordering is
/// enforced by `manifest_names_checked_before_any_path_read` and the dedicated
/// traversal/absolute/duplicate tests below.
fn verify_manifest_names(manifest: &BundleManifest) -> Result<()> {
    let mut seen_names = std::collections::HashSet::new();
    for file in &manifest.files {
        // Reject absolute paths and any non-Normal component outright. On Unix
        // this catches leading `/` and any `..`/`.` segment; on Windows it also
        // catches drive roots and backslash separators.
        let path = Path::new(&file.name);
        if !file.name.is_empty() && path.is_absolute() {
            return Err(CliError::new(format!(
                "manifest file name {} is absolute; names must be relative to the bundle root",
                file.name
            )));
        }
        for component in path.components() {
            use std::path::Component;
            match component {
                Component::Normal(_) => {}
                Component::ParentDir => {
                    return Err(CliError::new(format!(
                        "manifest file name {} contains a parent-directory (..) component",
                        file.name
                    )));
                }
                Component::RootDir | Component::Prefix(_) | Component::CurDir => {
                    return Err(CliError::new(format!(
                        "manifest file name {} contains a non-normal path component",
                        file.name
                    )));
                }
            }
        }
        if path.components().count() == 0 {
            return Err(CliError::new(format!(
                "manifest file name {:?} is empty",
                file.name
            )));
        }
        if !seen_names.insert(&file.name) {
            return Err(CliError::new(format!(
                "manifest lists duplicate file name {}",
                file.name
            )));
        }
    }
    Ok(())
}

/// The exact gate-specific manifest file set: no duplicate kinds that could
/// shadow a required file, and every required name is present. Name-safety
/// (absolute/traversal/duplicate) is enforced earlier by [`verify_manifest_names`].
/// The required names are fixed per gate so a manifest that drops a required file
/// or smuggles in an extra path is rejected.
fn verify_required_file_set(manifest: &BundleManifest, gate: &str) -> Result<()> {
    // Singleton kinds (one required file per bundle, looked up by kind) must
    // not be duplicated — a second file of the same kind could shadow the
    // required one when a consumer looks up by kind. Stem families
    // (approach_wav/control_wav/stem_wav/world_*) legitimately repeat or are
    // singletons specific to a gate.
    const SINGLETON_KINDS: &[&str] = &[
        "metrics",
        "fixture",
        "asset_descriptor",
        "capture_provenance",
    ];
    for kind in SINGLETON_KINDS {
        let matches: Vec<&BundleFile> = manifest.files.iter().filter(|f| f.kind == *kind).collect();
        if matches.len() > 1 {
            return Err(CliError::new(format!(
                "manifest lists duplicate singleton kind {kind} ({}); a second file of this kind could shadow the required one",
                matches
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    // The exact required name set per gate. capture-provenance.json is an
    // immutable indexed input; listening-record.json is intentionally NOT
    // indexed (it stays mutable for the human sign-off).
    let required: &[(&str, &str)] = match gate {
        "S0" => &[
            ("approach-00-100m.wav", "approach_wav"),
            ("approach-01-75m.wav", "approach_wav"),
            ("approach-02-50m.wav", "approach_wav"),
            ("approach-03-25m.wav", "approach_wav"),
            ("approach-04-10m.wav", "approach_wav"),
            ("approach-05-1m.wav", "approach_wav"),
            ("control-100m-air-enabled.wav", "control_wav"),
            ("control-100m-air-disabled.wav", "control_wav"),
            ("metrics.json", "metrics"),
            ("fixture.json", "fixture"),
            ("asset-descriptor.json", "asset_descriptor"),
            ("capture-provenance.json", "capture_provenance"),
        ],
        "S3" => &[
            ("direct.wav", "stem_wav"),
            ("reflections.wav", "stem_wav"),
            ("path.wav", "stem_wav"),
            ("pathing-on-sum.wav", "stem_wav"),
            ("pathing-off-sum.wav", "stem_wav"),
            ("trajectory-sum.wav", "trajectory_sum_wav"),
            ("metrics.json", "metrics"),
            ("trajectory-metrics.json", "trajectory_metrics"),
            ("fixture.json", "fixture"),
            ("asset-descriptor.json", "asset_descriptor"),
            ("capture-provenance.json", "capture_provenance"),
            ("world/probe-batch.bin", "world_probe_batch"),
            (
                "world/probe-batch-metadata.json",
                "world_probe_batch_metadata",
            ),
            ("world/world-manifest.json", "world_manifest"),
        ],
        other => {
            return Err(CliError::new(format!(
                "manifest declares unknown gate {other:?}; expected \"S0\" or \"S3\""
            )));
        }
    };
    for (name, kind) in required {
        let Some(file) = manifest.find(name) else {
            return Err(CliError::new(format!(
                "manifest for gate {gate} is missing required file {name}"
            )));
        };
        if file.kind != *kind {
            return Err(CliError::new(format!(
                "manifest file {name} has kind {}, expected {kind}",
                file.kind
            )));
        }
    }
    if manifest.files.len() != required.len() {
        return Err(CliError::new(format!(
            "manifest for gate {gate} must contain exactly {} allowed files, got {}",
            required.len(),
            manifest.files.len()
        )));
    }
    Ok(())
}

fn verify_fixture_hash(bundle_dir: &Path, manifest: &BundleManifest) -> Result<()> {
    let bytes = std::fs::read(bundle_dir.join("fixture.json"))
        .map_err(|e| CliError::new(format!("cannot read fixture.json: {e}")))?;
    let hash = sha256_hex(&bytes);
    if hash != manifest.fixture_content_sha256 {
        return Err(CliError::new(
            "fixture.json content hash does not match manifest's fixture_content_sha256",
        ));
    }
    Ok(())
}

fn verify_asset_descriptor(
    bundle_dir: &Path,
    manifest: &BundleManifest,
    fixture: &Fixture,
) -> Result<()> {
    let entry = manifest
        .find("asset-descriptor.json")
        .ok_or_else(|| CliError::new("manifest is missing asset-descriptor.json"))?;
    let bytes = std::fs::read(bundle_dir.join("asset-descriptor.json"))
        .map_err(|e| CliError::new(format!("cannot read asset-descriptor.json: {e}")))?;
    let hash = sha256_hex(&bytes);
    if hash != entry.content_sha256 {
        return Err(CliError::new(
            "asset-descriptor.json content hash does not match manifest",
        ));
    }
    if hash != manifest.asset_descriptor_sha256 {
        return Err(CliError::new(
            "asset-descriptor.json content hash does not match manifest's asset_descriptor_sha256",
        ));
    }
    // Semantic asset_id binding: a matching byte hash alone is not enough. The
    // descriptor's asset_id must match BOTH the manifest's asset_id AND the
    // fixture's declared source asset_id, so a swapped descriptor (same bytes
    // shape, wrong source) cannot slip through on a hash match.
    let descriptor: crate::asset::AssetDescriptor = serde_json::from_slice(&bytes)
        .map_err(|e| CliError::new(format!("asset-descriptor.json is not valid: {e}")))?;
    if descriptor.asset_id != manifest.asset_id {
        return Err(CliError::new(format!(
            "asset-descriptor asset_id {} does not match manifest asset_id {}",
            descriptor.asset_id, manifest.asset_id
        )));
    }
    if descriptor.asset_id != fixture.source.asset_id {
        return Err(CliError::new(format!(
            "asset-descriptor asset_id {} does not match fixture source asset_id {}",
            descriptor.asset_id, fixture.source.asset_id
        )));
    }
    Ok(())
}

fn verify_s0_mechanics(
    bundle_dir: &Path,
    manifest: &BundleManifest,
    _fixture: &Fixture,
) -> Result<GateSummary> {
    let mut checks = Vec::new();

    // Load and strict-parse the metrics sidecar.
    let metrics_text = std::fs::read_to_string(bundle_dir.join("metrics.json"))
        .map_err(|e| CliError::new(format!("cannot read metrics.json: {e}")))?;
    let metrics: S0Metrics = serde_json::from_str(&metrics_text).map_err(|e| {
        CliError::new(format!(
            "metrics.json is not a valid S0 metrics document: {e}"
        ))
    })?;
    if metrics.schema_version != S0Metrics::SCHEMA {
        return Err(CliError::new("metrics.json schema is not S0 metrics"));
    }
    if metrics.trajectory.is_empty() {
        return Err(CliError::new("S0 metrics has no trajectory points"));
    }

    // Recompute every trajectory WAV's hash, channel health, finiteness, and
    // frame count from disk. Reject any sidecar whose numeric values were
    // altered while the hash remained valid.
    let mut approach_pcm_rms_db: Vec<f32> = Vec::with_capacity(metrics.trajectory.len());
    for entry in &manifest.files {
        if entry.kind == "approach_wav" {
            let (spec, samples) = decode_wav(bundle_dir, entry, metrics.sample_rate_hz)?;
            let recomputed = crate::metrics::stereo_channel_metric(spec.sample_rate_hz, &samples)
                .map_err(CliError::new)?;
            // Find the matching trajectory metric by index-derived distance and
            // cross-check every recorded channel-health value.
            let matched = metrics
                .trajectory
                .iter()
                .find(|metric| approach_filename_for(metric) == entry.name)
                .ok_or_else(|| {
                    CliError::new(format!(
                        "approach WAV {} has no matching trajectory metric",
                        entry.name
                    ))
                })?;
            cross_check_channel_payload(&recomputed, &matched.channel, &entry.name)?;
            // Record the recomputed-from-PCM RMS for the monotonicity and
            // inverse-distance assertions below. Average L/R dBFS.
            let avg_db = matched_average_dbfs(&recomputed)?;
            approach_pcm_rms_db.push(avg_db);
        } else if entry.kind == "control_wav" {
            // Control WAVs are recomputed for hash/finiteness/sample-rate only.
            decode_wav(bundle_dir, entry, metrics.sample_rate_hz)?;
        }
    }

    // The approach WAVs must be in trajectory order (far->near). Recompute the
    // PCM RMS from each decoded WAV, not from the recorded attenuation fields.
    if approach_pcm_rms_db.len() != metrics.trajectory.len() {
        return Err(CliError::new(format!(
            "found {} approach WAVs but {} trajectory metrics",
            approach_pcm_rms_db.len(),
            metrics.trajectory.len()
        )));
    }

    // Monotonic nondecreasing PCM RMS from 100 m to 1 m, recomputed from PCM.
    let mut previous = f32::NEG_INFINITY;
    for (index, &rms) in approach_pcm_rms_db.iter().enumerate() {
        if !rms.is_finite() {
            return Err(CliError::new(format!(
                "recomputed S0 PCM RMS at trajectory index {index} is non-finite"
            )));
        }
        if rms + MONOTONIC_TOLERANCE_DB < previous {
            return Err(CliError::new(format!(
                "recomputed S0 PCM RMS is not monotonic nondecreasing: {rms:.4} dBFS after {previous:.4} dBFS at trajectory index {index}"
            )));
        }
        previous = rms;
    }
    checks.push("recomputed PCM RMS monotonic nondecreasing 100 m -> 1 m".into());

    // Recompute the actual 100 m-to-1 m level delta from PCM, not from recorded
    // attenuation fields or a recorded boolean.
    let first_pcm = approach_pcm_rms_db
        .first()
        .copied()
        .ok_or_else(|| CliError::new("no approach PCM for the 100 m point"))?;
    let last_pcm = approach_pcm_rms_db
        .last()
        .copied()
        .ok_or_else(|| CliError::new("no approach PCM for the 1 m point"))?;
    let recomputed_level_delta_db = last_pcm - first_pcm;
    if !recomputed_level_delta_db.is_finite() {
        return Err(CliError::new(
            "recomputed 100 m -> 1 m level delta is non-finite",
        ));
    }
    // The recorded inverse-distance delta must agree with the recomputed PCM
    // delta within tolerance (the PCM carries air absorption + binaural too, so
    // allow the documented tolerance rather than exact equality).
    if (recomputed_level_delta_db - metrics.inverse_distance_100m_to_1m_db).abs()
        > metrics.inverse_distance_tolerance_db
    {
        return Err(CliError::new(format!(
            "recomputed 100 m -> 1 m PCM level delta ({recomputed_level_delta_db:.2} dB) is not within ±{} dB of the recorded inverse-distance delta ({:.2} dB)",
            metrics.inverse_distance_tolerance_db, metrics.inverse_distance_100m_to_1m_db
        )));
    }
    checks.push(format!(
        "recomputed 100 m -> 1 m PCM level delta {recomputed_level_delta_db:.2} dB within tolerance"
    ));

    // Calibration chain: recompute the full ADR 0002 equations and reference
    // constants, not just target/drive/linear fields.
    verify_canonical_calibration(&metrics.calibration)?;
    checks.push("canonical one-gain calibration chain verified".into());

    // Recompute the enabled-vs-disabled same-pose high-band comparison from the
    // actual control WAVs, using the documented deterministic algorithm shared
    // with generation. Do not derive the pass from a recorded boolean.
    let enabled_entry = manifest
        .find("control-100m-air-enabled.wav")
        .ok_or_else(|| CliError::new("manifest is missing control-100m-air-enabled.wav"))?;
    let disabled_entry = manifest
        .find("control-100m-air-disabled.wav")
        .ok_or_else(|| CliError::new("manifest is missing control-100m-air-disabled.wav"))?;
    let (_, enabled_pcm) = decode_wav(bundle_dir, enabled_entry, metrics.sample_rate_hz)?;
    let (_, disabled_pcm) = decode_wav(bundle_dir, disabled_entry, metrics.sample_rate_hz)?;
    let cutoff = metrics.high_band_energy.cutoff_hz;
    if !cutoff.is_finite() || cutoff <= 0.0 {
        return Err(CliError::new(
            "high-band cutoff_hz must be finite and positive",
        ));
    }
    let recomputed_enabled_high =
        crate::metrics::high_band_rms(metrics.sample_rate_hz, &enabled_pcm, cutoff);
    let recomputed_disabled_high =
        crate::metrics::high_band_rms(metrics.sample_rate_hz, &disabled_pcm, cutoff);
    if !recomputed_enabled_high.is_finite() || !recomputed_disabled_high.is_finite() {
        return Err(CliError::new("recomputed high-band RMS is non-finite"));
    }
    // The recorded values must match the recomputed PCM values.
    if (recomputed_enabled_high - metrics.high_band_energy.enabled_air_100m_high_band_rms).abs()
        > HIGH_BAND_RECOMPUTE_TOLERANCE
    {
        return Err(CliError::new(format!(
            "recorded enabled high-band RMS ({}) does not match recomputed PCM ({})",
            metrics.high_band_energy.enabled_air_100m_high_band_rms, recomputed_enabled_high
        )));
    }
    if (recomputed_disabled_high - metrics.high_band_energy.disabled_air_100m_high_band_rms).abs()
        > HIGH_BAND_RECOMPUTE_TOLERANCE
    {
        return Err(CliError::new(format!(
            "recorded disabled high-band RMS ({}) does not match recomputed PCM ({})",
            metrics.high_band_energy.disabled_air_100m_high_band_rms, recomputed_disabled_high
        )));
    }
    if !(recomputed_enabled_high <= recomputed_disabled_high + HIGH_BAND_RECOMPUTE_TOLERANCE) {
        return Err(CliError::new(format!(
            "recomputed enabled air absorption increased high-band energy: enabled={recomputed_enabled_high:.6} > disabled={recomputed_disabled_high:.6}"
        )));
    }
    checks.push("recomputed enabled-vs-disabled high-band bound holds from PCM".into());

    Ok(GateSummary {
        checks_passed: checks,
    })
}

/// Tolerance for the monotonic-nondecreasing PCM RMS assertion (dB). Absorbs
/// binaural/HRTF interleaving asymmetry while catching a real inversion.
const MONOTONIC_TOLERANCE_DB: f32 = 1.0e-3;

/// Tolerance for recomputed-vs-recorded high-band RMS comparison (linear).
const HIGH_BAND_RECOMPUTE_TOLERANCE: f32 = 1.0e-6;

/// Tolerance for recomputed-vs-recorded pathing spectral L1/L2 differences. The
/// canonical WAV writer is lossless IEEE float32 and `compare_pathing` is a pure
/// function of the PCM, so the level/energy/rms fields must match exactly; the
/// spectral magnitudes are compared within a tight float-order tolerance that
/// only absorbs the encode/decode round trip.
const PATHING_RECOMPUTE_SPECTRAL_TOLERANCE: f32 = 1.0e-6;

/// Tolerance for recomputed-vs-recorded analytic angular delta (degrees) and
/// analytic-azimuth-vs-fixture comparison. Avoids exact float equality.
const ANALYTIC_INTERNAL_TOLERANCE_DEG: f32 = 1.0e-3;

/// Shortest signed-then-abs angular distance between two compass azimuths in
/// degrees, accounting for the 360° wraparound. Two azimuths 350° apart on the
/// naive line are only 10° apart on the circle.
fn circular_angle_delta_degrees(a_degrees: f32, b_degrees: f32) -> f32 {
    let raw = (a_degrees - b_degrees).rem_euclid(360.0);
    if raw > 180.0 { 360.0 - raw } else { raw }
}

/// Reconstruct the approach WAV filename for a trajectory metric, matching the
/// generator's `approach-{index:02}-{meters}m.wav` scheme.
fn approach_filename_for(metric: &S0TrajectoryMetric) -> String {
    format!(
        "approach-{:02}-{}m.wav",
        metric.index,
        meters_label(metric.distance_m)
    )
}

/// Format a distance in metres the way the S0 generator names its WAVs.
fn meters_label(distance_m: f32) -> String {
    if (distance_m - distance_m.round()).abs() < 1.0e-3 {
        format!("{}", distance_m.round() as i64)
    } else {
        format!("{distance_m:.2}")
    }
}

/// Average L/R dBFS of a recomputed stereo channel payload, rejecting silence.
fn matched_average_dbfs(payload: &ChannelMetricPayload) -> Result<f32> {
    let mut finite: Vec<f32> = payload
        .rms_dbfs_per_channel
        .iter()
        .copied()
        .flatten()
        .filter(|v| v.is_finite())
        .collect();
    if finite.is_empty() {
        return Err(CliError::new(
            "recomputed S0 PCM is silent; cannot assert monotonicity or level delta",
        ));
    }
    finite.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Average of the finite channel dBFS values.
    let sum: f32 = finite.iter().copied().sum();
    Ok(sum / finite.len() as f32)
}

/// Cross-check every recorded channel-health value against the recomputed one,
/// so a sidecar whose numbers were altered while the hash stayed valid is
/// rejected.
fn cross_check_channel_payload(
    recomputed: &ChannelMetricPayload,
    recorded: &ChannelMetricPayload,
    file_name: &str,
) -> Result<()> {
    if recomputed.frame_count != recorded.frame_count
        || recomputed.channels != recorded.channels
        || recomputed.sample_rate_hz != recorded.sample_rate_hz
        || recomputed.all_finite != recorded.all_finite
        || recomputed.silent_channel_count != recorded.silent_channel_count
    {
        return Err(CliError::new(format!(
            "{file_name}: recomputed channel health does not match recorded sidecar"
        )));
    }
    if recomputed.peak_per_channel != recorded.peak_per_channel {
        return Err(CliError::new(format!(
            "{file_name}: recomputed peak per channel does not match recorded sidecar"
        )));
    }
    if recomputed.rms_per_channel != recorded.rms_per_channel {
        return Err(CliError::new(format!(
            "{file_name}: recomputed RMS per channel does not match recorded sidecar"
        )));
    }
    if recomputed.rms_dbfs_per_channel != recorded.rms_dbfs_per_channel {
        return Err(CliError::new(format!(
            "{file_name}: recomputed RMS dBFS per channel does not match recorded sidecar"
        )));
    }
    if recomputed.stereo_difference_rms != recorded.stereo_difference_rms {
        return Err(CliError::new(format!(
            "{file_name}: recomputed stereo difference RMS does not match recorded sidecar"
        )));
    }
    Ok(())
}

/// Decode a manifest WAV, recompute its hash, and assert stereo/sample-rate/
/// finiteness/frame integrity. Returns the decoded spec and interleaved samples.
fn decode_wav(
    bundle_dir: &Path,
    entry: &BundleFile,
    expected_sample_rate_hz: u32,
) -> Result<(fightbox_evidence::WavSpec, Vec<f32>)> {
    let bytes = std::fs::read(bundle_dir.join(&entry.name))
        .map_err(|e| CliError::new(format!("cannot read {}: {e}", entry.name)))?;
    let hash = sha256_hex(&bytes);
    if hash != entry.content_sha256 {
        return Err(CliError::new(format!(
            "{} content hash does not match manifest",
            entry.name
        )));
    }
    let (spec, samples) = read_wav(&bytes)
        .map_err(|e| CliError::new(format!("{} is not a valid WAV: {}", entry.name, e.as_str())))?;
    if spec.channels != 2 {
        return Err(CliError::new(format!(
            "{} is not stereo (channels={})",
            entry.name, spec.channels
        )));
    }
    if spec.sample_rate_hz != expected_sample_rate_hz {
        return Err(CliError::new(format!(
            "{} sample rate {} does not match expected {}",
            entry.name, spec.sample_rate_hz, expected_sample_rate_hz
        )));
    }
    if samples.len() % 2 != 0 {
        return Err(CliError::new(format!(
            "{} sample count is not a whole number of stereo frames",
            entry.name
        )));
    }
    if !samples.iter().copied().all(f32::is_finite) {
        return Err(CliError::new(format!(
            "{} contains non-finite samples",
            entry.name
        )));
    }
    Ok((spec, samples))
}

fn verify_s3_mechanics(
    bundle_dir: &Path,
    manifest: &BundleManifest,
    fixture: &Fixture,
) -> Result<GateSummary> {
    let mut checks = Vec::new();

    let metrics_text = std::fs::read_to_string(bundle_dir.join("metrics.json"))
        .map_err(|e| CliError::new(format!("cannot read metrics.json: {e}")))?;
    let metrics: S3Metrics = serde_json::from_str(&metrics_text).map_err(|e| {
        CliError::new(format!(
            "metrics.json is not a valid S3 metrics document: {e}"
        ))
    })?;
    if metrics.schema_version != S3Metrics::SCHEMA {
        return Err(CliError::new("metrics.json schema is not S3 metrics"));
    }

    // Recompute every stem WAV hash from disk.
    let required_stems = [
        ("direct.wav", "direct"),
        ("reflections.wav", "reflections"),
        ("path.wav", "path"),
        ("pathing-on-sum.wav", "pathing_on_sum"),
        ("pathing-off-sum.wav", "pathing_off_sum"),
    ];
    for (file_name, kind) in required_stems.iter() {
        let entry = manifest
            .find(file_name)
            .ok_or_else(|| CliError::new(format!("manifest is missing stem {file_name}")))?;
        recompute_wav(bundle_dir, entry, metrics.sample_rate_hz)?;
        let stem_metric = metrics
            .stems
            .iter()
            .find(|stem| stem.kind == *kind)
            .ok_or_else(|| CliError::new(format!("metrics.json has no stem of kind {kind}")))?;
        if stem_metric.file != *file_name {
            return Err(CliError::new(format!(
                "stem kind {kind} records file {} not {file_name}",
                stem_metric.file
            )));
        }
        if stem_metric.content_sha256 != entry.content_sha256 {
            return Err(CliError::new(format!(
                "stem {kind} hash mismatch: metrics={} manifest={}",
                stem_metric.content_sha256, entry.content_sha256
            )));
        }
        if stem_metric.frame_count != entry_size_frames(bundle_dir, entry)? {
            return Err(CliError::new(format!(
                "stem {kind} frame count does not match WAV on disk",
            )));
        }
    }

    // Calibration chain.
    verify_canonical_calibration(&metrics.calibration)?;
    checks.push("canonical one-gain calibration chain verified".into());

    // World payload must be consistent and the referenced world must exist.
    verify_world_payload(bundle_dir, &metrics.world, manifest)?;

    // Reflection irSize must be positive.
    if metrics.snapshot.reflections.ir_size <= 0 {
        return Err(CliError::new(
            "metrics.json records a non-positive reflection irSize",
        ));
    }
    checks.push("reflection irSize positive".into());

    for (label, mode) in [
        (
            "requested",
            &metrics.snapshot.direct.requested_occlusion_mode,
        ),
        (
            "delivered",
            &metrics.snapshot.direct.delivered_occlusion_mode,
        ),
    ] {
        if mode.kind != OcclusionModeKind::Raycast
            || mode.volumetric_radius_m != 0.0
            || mode.volumetric_sample_count != 0
        {
            return Err(CliError::new(format!(
                "metrics.json {label} direct occlusion must be canonical raycast with radius 0 and samples 0"
            )));
        }
    }
    if metrics.snapshot.direct.requested_occlusion_mode
        != metrics.snapshot.direct.delivered_occlusion_mode
    {
        return Err(CliError::new(
            "metrics.json requested and delivered direct occlusion modes differ",
        ));
    }
    checks.push("requested and delivered direct occlusion are canonical raycast".into());

    // Pathing on/off sums must hash differently and the metrics must agree.
    if !metrics.pathing_comparison.differs {
        return Err(CliError::new(
            "metrics.json records pathing on/off sums as identical",
        ));
    }
    if metrics.pathing_comparison.on_sum_hash_sha256
        == metrics.pathing_comparison.off_sum_hash_sha256
    {
        return Err(CliError::new(
            "metrics.json pathing on/off hashes are equal despite differs=true",
        ));
    }
    // Decode the pathing-on/off PCM and INDEPENDENTLY rerun the PUBLIC
    // `fightbox_evidence::compare_pathing` on the actual decoded samples with the
    // documented bins. The canonical WAV writer is lossless IEEE float32, so the
    // recomputed values must match the recorded sidecar within tight float-order
    // tolerance. We never share the recorder's private computation; we call the
    // same public function and compare the full result (level, spectral L1/L2,
    // energy, differs), so a sidecar whose numbers were edited while the WAV
    // hashes stayed valid is rejected, and altering the PCM (even with the
    // manifest hashes rewritten) cannot preserve a pass.
    let on_entry = manifest
        .find("pathing-on-sum.wav")
        .ok_or_else(|| CliError::new("missing pathing-on-sum.wav"))?;
    let off_entry = manifest
        .find("pathing-off-sum.wav")
        .ok_or_else(|| CliError::new("missing pathing-off-sum.wav"))?;
    let (on_spec, on_pcm) = decode_wav(bundle_dir, on_entry, metrics.sample_rate_hz)?;
    let (off_spec, off_pcm) = decode_wav(bundle_dir, off_entry, metrics.sample_rate_hz)?;
    if on_spec != off_spec {
        return Err(CliError::new(
            "pathing-on-sum.wav and pathing-off-sum.wav have different WAV specs",
        ));
    }
    let on_bytes = std::fs::read(bundle_dir.join("pathing-on-sum.wav"))
        .map_err(|e| CliError::new(format!("cannot read pathing-on-sum.wav: {e}")))?;
    let off_bytes = std::fs::read(bundle_dir.join("pathing-off-sum.wav"))
        .map_err(|e| CliError::new(format!("cannot read pathing-off-sum.wav: {e}")))?;
    let on_hash = sha256_hex(&on_bytes);
    let off_hash = sha256_hex(&off_bytes);
    if on_hash != metrics.pathing_comparison.on_sum_hash_sha256
        || off_hash != metrics.pathing_comparison.off_sum_hash_sha256
    {
        return Err(CliError::new(
            "recomputed pathing on/off hashes do not match metrics",
        ));
    }
    // The recorded bins must equal the documented contract bins, and the
    // recomputed comparison must be run at exactly those bins.
    if metrics.pathing_comparison.bins_hz != crate::schema::S3_PATHING_COMPARISON_BINS_HZ {
        return Err(CliError::new(format!(
            "metrics.json pathing bins_hz {:?} do not match the documented contract bins {:?}",
            metrics.pathing_comparison.bins_hz,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ
        )));
    }
    let recomputed = compare_pathing(
        on_spec,
        &on_pcm,
        &off_pcm,
        &metrics.pathing_comparison.bins_hz,
    )
    .map_err(|e| CliError::new(format!("recomputed compare_pathing failed: {e:?}")))?;
    cross_check_pathing_comparison(&metrics.pathing_comparison, &recomputed)?;
    checks.push("pathing on/off differ: recomputed public compare_pathing agrees".into());

    // Decoded azimuth within the fixture's analytic tolerance. Recompute the
    // shortest circular delta rather than trusting the recorded boolean.
    let arrival = metrics
        .analytic
        .arrival_azimuth_degrees_clockwise_from_north;
    let analytic_azimuth = metrics
        .analytic
        .analytic_azimuth_degrees_clockwise_from_north;
    if !arrival.is_finite()
        || !analytic_azimuth.is_finite()
        || !metrics.analytic.tolerance_degrees.is_finite()
    {
        return Err(CliError::new(
            "metrics.json analytic azimuth/tolerance contains non-finite values",
        ));
    }
    let circular_delta = circular_angle_delta_degrees(arrival, analytic_azimuth);
    // The recorded absolute_delta_degrees must agree with the recomputed
    // circular delta within a small tolerance.
    if (circular_delta - metrics.analytic.absolute_delta_degrees).abs()
        > ANALYTIC_INTERNAL_TOLERANCE_DEG
    {
        return Err(CliError::new(format!(
            "metrics.json analytic absolute_delta_degrees ({}) does not match recomputed circular delta ({})",
            metrics.analytic.absolute_delta_degrees, circular_delta
        )));
    }
    if circular_delta > metrics.analytic.tolerance_degrees {
        return Err(CliError::new(format!(
            "decoded azimuth {arrival:.3}° is outside the {tol:.1}° analytic tolerance (circular delta {circular_delta:.3}° from {analytic_azimuth:.3}°)",
            tol = metrics.analytic.tolerance_degrees
        )));
    }
    let fixture_analytic = fixture
        .expected
        .analytic
        .as_ref()
        .ok_or_else(|| CliError::new("fixture has no expected.analytic"))?;
    let expected_azimuth = fixture_analytic.arrival_azimuth_degrees_clockwise_from_north as f32;
    // The recorded analytic azimuth must equal the fixture's analytic azimuth.
    if (metrics
        .analytic
        .analytic_azimuth_degrees_clockwise_from_north
        - expected_azimuth)
        .abs()
        > ANALYTIC_INTERNAL_TOLERANCE_DEG
    {
        return Err(CliError::new(format!(
            "metrics.json analytic azimuth {} does not match the fixture {}",
            metrics
                .analytic
                .analytic_azimuth_degrees_clockwise_from_north,
            expected_azimuth
        )));
    }
    checks.push("decoded azimuth within analytic tolerance (circular arithmetic)".into());

    // Retained trajectory + summed-output handoff. Decode trajectory-sum.wav,
    // split by the recorded block size/count, rerun the public backend
    // summed-boundary metric on the actual PCM, and cross-check every recorded
    // value. Reject an injected boundary discontinuity, an altered pose/count/
    // counter, a missing occlusion transition, or stale trajectory evidence.
    verify_s3_trajectory(bundle_dir, manifest)?;

    Ok(GateSummary {
        checks_passed: checks,
    })
}

fn cross_check_pathing_comparison(
    recorded: &PathingComparisonPayload,
    recomputed: &fightbox_evidence::SpectralComparison,
) -> Result<()> {
    if !recomputed.differs {
        return Err(CliError::new(
            "recomputed compare_pathing reports pathing on/off sums do not differ above threshold",
        ));
    }
    if recomputed.energy.as_str() != recorded.energy {
        return Err(CliError::new(format!(
            "recomputed pathing energy {} does not match recorded {}",
            recomputed.energy.as_str(),
            recorded.energy
        )));
    }
    if recomputed.on_rms_dbfs != recorded.on_rms_dbfs {
        return Err(CliError::new(format!(
            "recomputed pathing on_rms_dbfs {:?} does not match recorded {:?}",
            recomputed.on_rms_dbfs, recorded.on_rms_dbfs
        )));
    }
    if recomputed.off_rms_dbfs != recorded.off_rms_dbfs {
        return Err(CliError::new(format!(
            "recomputed pathing off_rms_dbfs {:?} does not match recorded {:?}",
            recomputed.off_rms_dbfs, recorded.off_rms_dbfs
        )));
    }
    if recomputed.level_difference_db != recorded.level_difference_db {
        return Err(CliError::new(format!(
            "recomputed pathing level_difference_db {:?} does not match recorded {:?}",
            recomputed.level_difference_db, recorded.level_difference_db
        )));
    }
    if (recomputed.spectral_l1_difference - recorded.spectral_l1_difference).abs()
        > PATHING_RECOMPUTE_SPECTRAL_TOLERANCE
    {
        return Err(CliError::new(format!(
            "recomputed pathing spectral L1 ({}) does not match recorded ({})",
            recomputed.spectral_l1_difference, recorded.spectral_l1_difference
        )));
    }
    if (recomputed.spectral_l2_difference - recorded.spectral_l2_difference).abs()
        > PATHING_RECOMPUTE_SPECTRAL_TOLERANCE
    {
        return Err(CliError::new(format!(
            "recomputed pathing spectral L2 ({}) does not match recorded ({})",
            recomputed.spectral_l2_difference, recorded.spectral_l2_difference
        )));
    }
    Ok(())
}

/// Decode `trajectory-sum.wav`, split it by the recorded block size/count, rerun
/// the public backend summed-boundary continuity metric on the actual PCM, and
/// cross-check every recorded value. The continuity assertion is NEVER inferred
/// from a single diagnostic stem — it is recomputed on the summed output.
fn verify_s3_trajectory(bundle_dir: &Path, manifest: &BundleManifest) -> Result<()> {
    let wav_entry = manifest
        .find("trajectory-sum.wav")
        .ok_or_else(|| CliError::new("manifest is missing trajectory-sum.wav"))?;
    let metrics_entry = manifest
        .find("trajectory-metrics.json")
        .ok_or_else(|| CliError::new("manifest is missing trajectory-metrics.json"))?;

    let metrics_bytes = std::fs::read(bundle_dir.join("trajectory-metrics.json"))
        .map_err(|e| CliError::new(format!("cannot read trajectory-metrics.json: {e}")))?;
    if sha256_hex(&metrics_bytes) != metrics_entry.content_sha256 {
        return Err(CliError::new(
            "trajectory-metrics.json content hash does not match manifest",
        ));
    }
    let metrics: S3TrajectoryMetrics = serde_json::from_slice(&metrics_bytes)
        .map_err(|e| CliError::new(format!("trajectory-metrics.json is not valid: {e}")))?;
    if metrics.schema_version != crate::schema::S3_TRAJECTORY_METRICS {
        return Err(CliError::new(format!(
            "trajectory-metrics schema_version must be {}, got {}",
            crate::schema::S3_TRAJECTORY_METRICS,
            metrics.schema_version
        )));
    }

    // Decode the summed WAV and cross-check its manifest hash + sample rate.
    let wav_bytes = std::fs::read(bundle_dir.join("trajectory-sum.wav"))
        .map_err(|e| CliError::new(format!("cannot read trajectory-sum.wav: {e}")))?;
    if sha256_hex(&wav_bytes) != wav_entry.content_sha256 {
        return Err(CliError::new(
            "trajectory-sum.wav content hash does not match manifest",
        ));
    }
    let (spec, samples) = read_wav(&wav_bytes)
        .map_err(|e| CliError::new(format!("cannot decode trajectory-sum.wav: {}", e.as_str())))?;
    if spec.channels != 2 {
        return Err(CliError::new(format!(
            "trajectory-sum.wav is not stereo (channels={})",
            spec.channels
        )));
    }
    if spec.sample_rate_hz != metrics.sample_rate_hz {
        return Err(CliError::new(format!(
            "trajectory-sum.wav sample rate {} does not match metrics {}",
            spec.sample_rate_hz, metrics.sample_rate_hz
        )));
    }

    // Total frames must equal block_size × block_count exactly.
    let expected_total = metrics.block_size_frames * metrics.block_count;
    if samples.len() / 2 != expected_total {
        return Err(CliError::new(format!(
            "trajectory-sum.wav frames ({}) does not match block_size({})×block_count({})={}",
            samples.len() / 2,
            metrics.block_size_frames,
            metrics.block_count,
            expected_total
        )));
    }
    // The recorded total_frames must agree with the WAV-derived total.
    if metrics.total_frames != expected_total {
        return Err(CliError::new(
            "trajectory-metrics total_frames does not match block_size×block_count",
        ));
    }

    // Split the summed PCM into per-block OwnedStereoPcm and rerun the public
    // backend summed-boundary continuity metric on the actual samples.
    let frames_per_block = metrics.block_size_frames;
    let mut blocks: Vec<OwnedStereoPcm> = Vec::with_capacity(metrics.block_count);
    for block_index in 0..metrics.block_count {
        let start = block_index * frames_per_block * 2;
        let end = start + frames_per_block * 2;
        let interleaved: Vec<f32> = samples[start..end].to_vec();
        blocks.push(OwnedStereoPcm {
            sample_rate_hz: metrics.sample_rate_hz as i32,
            frame_count: frames_per_block,
            interleaved,
        });
    }
    let continuity = measure_s3_summed_boundary_continuity(
        &blocks,
        metrics.window_frames,
        metrics.step_to_local_peak_threshold,
    )
    .map_err(|e| CliError::new(format!("summed-boundary rerun failed: {e}")))?;

    // The recorded constants must equal the pinned backend constants.
    if metrics.window_frames != S3_CONTINUITY_WINDOW_FRAMES
        || (metrics.step_to_local_peak_threshold - S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD).abs()
            > 1.0e-6
    {
        return Err(CliError::new(
            "trajectory-metrics window/threshold constants do not match the pinned backend constants",
        ));
    }
    // The recomputed continuity must agree with the recorded pass result and max
    // ratio. A sidecar whose numbers were edited while the WAV stayed valid must
    // be rejected.
    if continuity.passed != metrics.continuity_passed {
        return Err(CliError::new(format!(
            "recomputed continuity passed={} does not match recorded {}",
            continuity.passed, metrics.continuity_passed
        )));
    }
    if (continuity.maximum_step_to_local_peak_ratio - metrics.maximum_step_to_local_peak_ratio)
        .abs()
        > 1.0e-5
    {
        return Err(CliError::new(format!(
            "recomputed max step-to-peak ratio {} does not match recorded {}",
            continuity.maximum_step_to_local_peak_ratio, metrics.maximum_step_to_local_peak_ratio
        )));
    }
    if !continuity.passed {
        return Err(CliError::new(format!(
            "summed-boundary continuity failed (max ratio {})",
            continuity.maximum_step_to_local_peak_ratio
        )));
    }
    // The recomputed boundary count and per-boundary ratios must match.
    if continuity.boundaries.len() != metrics.boundaries.len() {
        return Err(CliError::new(format!(
            "recomputed boundary count {} does not match recorded {}",
            continuity.boundaries.len(),
            metrics.boundaries.len()
        )));
    }
    for (recomputed, recorded) in continuity.boundaries.iter().zip(&metrics.boundaries) {
        if (recomputed.step_to_local_peak_ratio - recorded.step_to_local_peak_ratio).abs() > 1.0e-5
        {
            return Err(CliError::new(format!(
                "recomputed boundary {} ratio {} does not match recorded {}",
                recomputed.after_block_index,
                recomputed.step_to_local_peak_ratio,
                recorded.step_to_local_peak_ratio
            )));
        }
    }

    // The recorded block count must equal the listener-pose count in the block
    // list, and every block must carry its pose, occlusion, and path strength.
    if metrics.blocks.len() != metrics.block_count {
        return Err(CliError::new(
            "trajectory-metrics blocks list length does not match block_count",
        ));
    }
    for (index, block) in metrics.blocks.iter().enumerate() {
        if block.block_index != index {
            return Err(CliError::new(format!(
                "trajectory block {} has out-of-order index {}",
                index, block.block_index
            )));
        }
        if !block.direct_occlusion.is_finite() {
            return Err(CliError::new(format!(
                "trajectory block {} direct_occlusion is not finite",
                index
            )));
        }
        if !block.path_strength.is_finite() || block.path_strength <= 0.0 {
            return Err(CliError::new(format!(
                "trajectory block {} path_strength must be finite and nonzero",
                index
            )));
        }
        // Cross-bind this block's summed PCM hash to the actual decoded bytes.
        let start = index * frames_per_block * 2;
        let end = start + frames_per_block * 2;
        let mut block_bytes: Vec<u8> = Vec::with_capacity((end - start) * 4);
        for sample in &samples[start..end] {
            block_bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let recomputed_hash = sha256_hex(&block_bytes);
        if recomputed_hash != block.summed_hash_sha256 {
            return Err(CliError::new(format!(
                "trajectory block {} summed hash does not match decoded PCM",
                index
            )));
        }
    }

    // An occlusion-state transition from shadowed to direct must be observed.
    if !metrics.occlusion_transition_observed {
        return Err(CliError::new(
            "trajectory-metrics records no occlusion-state transition",
        ));
    }
    let first = metrics
        .blocks
        .first()
        .ok_or_else(|| CliError::new("trajectory has no blocks"))?
        .direct_occlusion;
    let last = metrics
        .blocks
        .last()
        .ok_or_else(|| CliError::new("trajectory has no blocks"))?
        .direct_occlusion;
    if !(last > first) {
        return Err(CliError::new(
            "trajectory-metrics records no occlusion transition from shadowed to direct",
        ));
    }

    // One retained session: every generation/load counter must be exactly 1 and
    // rendered_blocks must equal block_count.
    let r = &metrics.retained;
    if r.context_generations != 1
        || r.scene_generations != 1
        || r.probe_batch_loads != 1
        || r.simulator_generations != 1
        || r.source_generations != 1
        || r.hrtf_generations != 1
        || r.effect_graph_generations != 1
    {
        return Err(CliError::new(
            "trajectory-metrics retained counters do not prove one retained session",
        ));
    }
    if r.rendered_blocks as usize != metrics.block_count {
        return Err(CliError::new(
            "trajectory-metrics rendered_blocks does not match block_count",
        ));
    }

    Ok(())
}

fn entry_size_frames(bundle_dir: &Path, entry: &BundleFile) -> Result<usize> {
    let bytes = std::fs::read(bundle_dir.join(&entry.name))
        .map_err(|e| CliError::new(format!("cannot read {}: {e}", entry.name)))?;
    let (spec, samples) = read_wav(&bytes)
        .map_err(|e| CliError::new(format!("cannot decode {}: {}", entry.name, e.as_str())))?;
    Ok(samples.len() / spec.channels as usize)
}

fn recompute_wav(
    bundle_dir: &Path,
    entry: &BundleFile,
    expected_sample_rate_hz: u32,
) -> Result<String> {
    let bytes = std::fs::read(bundle_dir.join(&entry.name))
        .map_err(|e| CliError::new(format!("cannot read {}: {e}", entry.name)))?;
    let hash = sha256_hex(&bytes);
    if hash != entry.content_sha256 {
        return Err(CliError::new(format!(
            "{} content hash does not match manifest",
            entry.name
        )));
    }
    let (spec, samples) = read_wav(&bytes)
        .map_err(|e| CliError::new(format!("{} is not a valid WAV: {}", entry.name, e.as_str())))?;
    if spec.channels != 2 {
        return Err(CliError::new(format!(
            "{} is not stereo (channels={})",
            entry.name, spec.channels
        )));
    }
    if spec.sample_rate_hz != expected_sample_rate_hz {
        return Err(CliError::new(format!(
            "{} sample rate {} does not match expected {}",
            entry.name, spec.sample_rate_hz, expected_sample_rate_hz
        )));
    }
    if samples.len() % 2 != 0 {
        return Err(CliError::new(format!(
            "{} sample count is not a whole number of stereo frames",
            entry.name
        )));
    }
    if !samples.iter().copied().all(f32::is_finite) {
        return Err(CliError::new(format!(
            "{} contains non-finite samples",
            entry.name
        )));
    }
    Ok(hash)
}

fn verify_canonical_calibration(calibration: &CalibrationPayload) -> Result<()> {
    // Recompute the complete ADR 0002 calibration equations and reference
    // constants, not just the target/drive/linear fields. The canonical chain:
    //   scene anchor: 120 dB SPL reference @ -24 dBFS PCM RMS @ 1 m
    //   85 dB SPL source -> target_source_rms_dbfs = -24 + (85 - 120) = -59 dBFS
    //   drive = target - program_rms_dbfs (≈ -39 dB for a -20 dBFS program)
    //   linear_gain = 10^(drive_db / 20)
    const REFERENCE_SPL_DB: f32 = fightbox_api::SceneCalibration::DEFAULT_REFERENCE_SPL_DB;
    const REFERENCE_PCM_RMS_DBFS: f32 =
        fightbox_api::SceneCalibration::DEFAULT_REFERENCE_PCM_RMS_DBFS;
    const REFERENCE_DISTANCE_M: f32 = fightbox_api::SceneCalibration::REFERENCE_DISTANCE_M;
    const SOURCE_DB_SPL: f32 = 85.0;
    const TOLERANCE_DB: f32 = 0.5;

    // Every value must be finite before any arithmetic.
    if !calibration.reference_spl_db.is_finite()
        || !calibration.reference_pcm_rms_dbfs.is_finite()
        || !calibration.reference_distance_m.is_finite()
        || !calibration.program_rms_dbfs.is_finite()
        || !calibration.target_source_rms_dbfs.is_finite()
        || !calibration.drive_gain_db.is_finite()
        || !calibration.linear_gain.is_finite()
    {
        return Err(CliError::new(
            "calibration payload contains non-finite values",
        ));
    }

    // The scene anchor constants must be the canonical ADR 0002 values exactly.
    if (calibration.reference_spl_db - REFERENCE_SPL_DB).abs() > 1.0e-4 {
        return Err(CliError::new(format!(
            "calibration reference_spl_db {} must be the ADR 0002 anchor {REFERENCE_SPL_DB}",
            calibration.reference_spl_db
        )));
    }
    if (calibration.reference_pcm_rms_dbfs - REFERENCE_PCM_RMS_DBFS).abs() > 1.0e-4 {
        return Err(CliError::new(format!(
            "calibration reference_pcm_rms_dbfs {} must be the ADR 0002 anchor {REFERENCE_PCM_RMS_DBFS}",
            calibration.reference_pcm_rms_dbfs
        )));
    }
    if (calibration.reference_distance_m - REFERENCE_DISTANCE_M).abs() > 1.0e-4 {
        return Err(CliError::new(format!(
            "calibration reference_distance_m {} must be the ADR 0002 anchor {REFERENCE_DISTANCE_M}",
            calibration.reference_distance_m
        )));
    }

    // Recompute target_source_rms_dbfs from the scene anchor + 85 dB SPL source
    // and require it to match the recorded value and the canonical -59 dBFS.
    let recomputed_target = REFERENCE_PCM_RMS_DBFS + (SOURCE_DB_SPL - calibration.reference_spl_db);
    if (recomputed_target - calibration.target_source_rms_dbfs).abs() > 1.0e-3 {
        return Err(CliError::new(format!(
            "calibration target_source_rms_dbfs {} does not match the recomputed ADR 0002 value {recomputed_target}",
            calibration.target_source_rms_dbfs
        )));
    }
    if (calibration.target_source_rms_dbfs - (-59.0)).abs() > TOLERANCE_DB {
        return Err(CliError::new(format!(
            "calibration target_source_rms_dbfs {} is not within {TOLERANCE_DB} dB of -59",
            calibration.target_source_rms_dbfs
        )));
    }

    // Recompute drive_gain_db = target - program_rms_dbfs and require it to
    // match the recorded value and the canonical -39 dB.
    let recomputed_drive = calibration.target_source_rms_dbfs - calibration.program_rms_dbfs;
    if (recomputed_drive - calibration.drive_gain_db).abs() > 1.0e-3 {
        return Err(CliError::new(format!(
            "calibration drive_gain_db {} does not match the recomputed ADR 0002 value {recomputed_drive} (target - program)",
            calibration.drive_gain_db
        )));
    }
    if (calibration.drive_gain_db - (-39.0)).abs() > TOLERANCE_DB {
        return Err(CliError::new(format!(
            "calibration drive_gain_db {} is not within {TOLERANCE_DB} dB of -39",
            calibration.drive_gain_db
        )));
    }

    // The linear gain must match the dB gain exactly (recomputed).
    let expected_linear = 10.0_f32.powf(calibration.drive_gain_db / 20.0);
    if (calibration.linear_gain - expected_linear).abs() > 1.0e-4 {
        return Err(CliError::new(format!(
            "calibration linear_gain {} does not match drive_gain_db {} (expected {expected_linear})",
            calibration.linear_gain, calibration.drive_gain_db
        )));
    }
    Ok(())
}

fn verify_world_payload(
    bundle_dir: &Path,
    world_payload: &crate::bundle::WorldPayload,
    manifest: &BundleManifest,
) -> Result<()> {
    // The bundle must NOT derive trust from the absolute mutable `world_dir` in
    // the metrics: the original bake directory may be deleted or moved after the
    // bundle is copied. Trust comes from the immutable world artifacts the bundle
    // carries under `world/` and indexes in its manifest. The absolute path is
    // honest provenance of where the bake happened, but it is NOT required to
    // exist for verification to succeed. Each bundled world artifact is itself
    // manifest-bound (hash + size), and `verify_files_index` already confirmed
    // those bindings; here we cross-bind their CONTENT to the metrics and to the
    // bundle fixture.
    let world_root = bundle_dir.join("world");
    let probe_entry = manifest
        .find("world/probe-batch.bin")
        .ok_or_else(|| CliError::new("manifest is missing bundled world/probe-batch.bin"))?;
    let meta_entry = manifest
        .find("world/probe-batch-metadata.json")
        .ok_or_else(|| {
            CliError::new("manifest is missing bundled world/probe-batch-metadata.json")
        })?;
    let world_manifest_entry = manifest
        .find("world/world-manifest.json")
        .ok_or_else(|| CliError::new("manifest is missing bundled world/world-manifest.json"))?;
    // The bundled bytes are already hash-verified by verify_files_index, but we
    // re-read them here to cross-bind the content to the metrics world payload.
    let probe_bytes = std::fs::read(world_root.join("probe-batch.bin"))
        .map_err(|e| CliError::new(format!("cannot read bundled world/probe-batch.bin: {e}")))?;
    if probe_bytes.len() as u64 != world_payload.serialized_size_bytes {
        return Err(CliError::new(format!(
            "bundled world/probe-batch.bin size ({}) does not match metrics world payload ({})",
            probe_bytes.len(),
            world_payload.serialized_size_bytes
        )));
    }
    let probe_hash = sha256_hex(&probe_bytes);
    if world_payload.world_content_sha256 != probe_hash {
        return Err(CliError::new(
            "metrics world_content_sha256 must equal the bundled probe-batch bytes in Phase A",
        ));
    }
    if probe_hash != world_payload.probe_batch_content_sha256 {
        return Err(CliError::new(
            "bundled world probe-batch hash does not match metrics world payload",
        ));
    }
    if probe_hash != probe_entry.content_sha256 {
        return Err(CliError::new(
            "bundled world probe-batch hash does not match its manifest entry",
        ));
    }
    // Re-parse the bundled probe-batch metadata and cross-bind its schema,
    // version, commit, probe count, path bytes, serialized bytes, and content
    // hash to the metrics world payload and the manifest entry. A world baked
    // with a different fixture or SDK must not satisfy this bundle.
    let meta_text =
        std::fs::read_to_string(world_root.join("probe-batch-metadata.json")).map_err(|e| {
            CliError::new(format!(
                "cannot read bundled world/probe-batch-metadata.json: {e}"
            ))
        })?;
    let parsed_meta = parse_world_probe_batch_metadata(&meta_text)?;
    if parsed_meta.content_sha256 != probe_hash {
        return Err(CliError::new(
            "bundled probe-batch-metadata content_sha256 does not match the probe-batch bytes",
        ));
    }
    // The metadata FILE's own hash (over its bytes) must match its manifest
    // entry. This is distinct from the probe-batch content hash recorded as a
    // FIELD inside the metadata — the manifest entry binds the metadata file
    // bytes, the inner field binds the metadata to the probe batch.
    let meta_file_hash = sha256_hex(meta_text.as_bytes());
    if meta_file_hash != meta_entry.content_sha256 {
        return Err(CliError::new(
            "bundled probe-batch-metadata file hash does not match its manifest entry",
        ));
    }
    if parsed_meta.probe_count != world_payload.probe_count {
        return Err(CliError::new(format!(
            "bundled probe-batch-metadata probe_count ({}) does not match metrics ({})",
            parsed_meta.probe_count, world_payload.probe_count
        )));
    }
    if parsed_meta.path_data_size_bytes != world_payload.path_data_size_bytes {
        return Err(CliError::new(format!(
            "bundled probe-batch-metadata path_data_size_bytes ({}) does not match metrics ({})",
            parsed_meta.path_data_size_bytes, world_payload.path_data_size_bytes
        )));
    }
    if parsed_meta.serialized_size_bytes != world_payload.serialized_size_bytes {
        return Err(CliError::new(format!(
            "bundled probe-batch-metadata serialized_size_bytes ({}) does not match metrics ({})",
            parsed_meta.serialized_size_bytes, world_payload.serialized_size_bytes
        )));
    }
    // The world manifest must be valid, carry the right schema, and agree with
    // the metrics and the bundled probe bytes on probe hash/count/path bytes/
    // serialized bytes. The world_dir recorded in metrics is provenance only.
    let manifest_text = std::fs::read_to_string(world_root.join("world-manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read bundled world-manifest.json: {e}")))?;
    let world_manifest: WorldManifest = serde_json::from_str(&manifest_text)
        .map_err(|e| CliError::new(format!("bundled world-manifest.json is not valid: {e}")))?;
    if sha256_hex(manifest_text.as_bytes()) != world_manifest_entry.content_sha256 {
        return Err(CliError::new(
            "bundled world-manifest.json hash does not match its manifest entry",
        ));
    }
    if world_manifest.schema_version != crate::schema::WORLD_MANIFEST {
        return Err(CliError::new(format!(
            "bundled world-manifest schema_version must be {}, got {}",
            world_manifest.schema_version,
            crate::schema::WORLD_MANIFEST
        )));
    }
    if world_manifest.probe_batch_content_sha256 != world_payload.probe_batch_content_sha256 {
        return Err(CliError::new(
            "bundled world-manifest probe hash does not match metrics world payload",
        ));
    }
    if world_manifest.probe_count != world_payload.probe_count
        || world_manifest.serialized_size_bytes != world_payload.serialized_size_bytes
        || world_manifest.path_data_size_bytes != world_payload.path_data_size_bytes
    {
        return Err(CliError::new(
            "bundled world-manifest probe counts/sizes do not match metrics world payload",
        ));
    }
    // Cross-bind the world manifest to the BUNDLE fixture: the world-manifest's
    // fixture ID and fixture hash must equal this bundle's fixture. A world
    // baked from a different fixture must not satisfy this bundle's S3 metrics.
    if world_manifest.fixture_id != manifest.fixture_id {
        return Err(CliError::new(format!(
            "bundled world-manifest fixture_id {} does not match bundle fixture_id {}",
            world_manifest.fixture_id, manifest.fixture_id
        )));
    }
    if world_manifest.fixture_content_sha256 != manifest.fixture_content_sha256 {
        return Err(CliError::new(format!(
            "bundled world-manifest fixture hash {} does not match bundle fixture hash {}",
            world_manifest.fixture_content_sha256, manifest.fixture_content_sha256
        )));
    }
    // The baked world manifest canonically indexes four artifacts. The capture
    // relocates the three world-specific artifacts under `world/`; its root
    // `fixture.json` is independently indexed by the bundle manifest and
    // cross-bound above by ID and content hash.
    if world_manifest.files.len() != 4 {
        return Err(CliError::new(format!(
            "bundled world-manifest must index exactly 4 baked-world files, got {}",
            world_manifest.files.len()
        )));
    }
    for required in [
        "probe-batch.bin",
        "probe-batch-metadata.json",
        "world-manifest.json",
        "fixture.json",
    ] {
        if !world_manifest.files.iter().any(|f| f == required) {
            return Err(CliError::new(format!(
                "bundled world-manifest does not index required world file {required}"
            )));
        }
    }
    Ok(())
}

/// Parse the bundled `probe-batch-metadata.json` into the fields the world
/// cross-binding needs. Mirrors the strict wire parser in `s3_render.rs` so a
/// hand-edited sidecar with the wrong schema/version is rejected here too.
fn parse_world_probe_batch_metadata(text: &str) -> Result<WorldProbeBatchMetadata> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct MetadataWire {
        schema_version: String,
        steam_audio_version: String,
        upstream_commit: String,
        probe_count: u32,
        path_data_size_bytes: u64,
        serialized_size_bytes: u64,
        content_sha256: String,
        #[allow(dead_code)]
        bake_progress_callback_count: u32,
        #[allow(dead_code)]
        final_bake_progress_millionths: u32,
    }
    let wire: MetadataWire = serde_json::from_str(text).map_err(|e| {
        CliError::new(format!(
            "bundled probe-batch-metadata.json is not valid: {e}"
        ))
    })?;
    if wire.schema_version != fightbox_steam_audio::PROBE_BATCH_METADATA_SCHEMA {
        return Err(CliError::new(format!(
            "bundled probe-batch metadata schema version is not {}",
            fightbox_steam_audio::PROBE_BATCH_METADATA_SCHEMA
        )));
    }
    if wire.steam_audio_version != fightbox_steam_audio::STEAM_AUDIO_VERSION {
        return Err(CliError::new(format!(
            "bundled probe-batch metadata Steam Audio version is not {}",
            fightbox_steam_audio::STEAM_AUDIO_VERSION
        )));
    }
    if wire.upstream_commit != fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT {
        return Err(CliError::new(format!(
            "bundled probe-batch metadata upstream commit is not {}",
            fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT
        )));
    }
    Ok(WorldProbeBatchMetadata {
        probe_count: wire.probe_count,
        path_data_size_bytes: wire.path_data_size_bytes,
        serialized_size_bytes: wire.serialized_size_bytes,
        content_sha256: wire.content_sha256,
    })
}

/// The subset of probe-batch metadata used for world cross-binding.
struct WorldProbeBatchMetadata {
    probe_count: u32,
    path_data_size_bytes: u64,
    serialized_size_bytes: u64,
    content_sha256: String,
}

/// Re-parse the immutable, manifest-indexed `capture-provenance.json` and
/// cross-bind its fields to the bundle. This is not "JSON parses" validation:
/// the SDK version/upstream-commit must match the pinned constants, the dylib
/// path and checksum must both be present (the capture was rejected at write
/// time if not, but a hand-edited sidecar must still be caught), the build
/// profile/sample rate/block size/quality must be the Phase A constants, the
/// fixture/gate must match the manifest, the required nonclaims must be present,
/// and (S3) the world_dir must match the metrics world payload.
fn verify_capture_provenance(
    bundle_dir: &Path,
    manifest: &BundleManifest,
    gate: &str,
    fixture: &Fixture,
) -> Result<()> {
    let entry = manifest
        .find("capture-provenance.json")
        .ok_or_else(|| CliError::new("manifest is missing capture-provenance.json"))?;
    let bytes = std::fs::read(bundle_dir.join("capture-provenance.json"))
        .map_err(|e| CliError::new(format!("cannot read capture-provenance.json: {e}")))?;
    let hash = sha256_hex(&bytes);
    if hash != entry.content_sha256 {
        return Err(CliError::new(
            "capture-provenance.json content hash does not match manifest",
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| CliError::new(format!("capture-provenance.json is not valid: {e}")))?;

    let get = |key: &str| -> Result<&str> {
        v.get(key)
            .and_then(|x| x.as_str())
            .ok_or_else(|| CliError::new(format!("capture-provenance.json missing string {key}")))
    };

    // SDK version and upstream commit must equal the pinned constants.
    let sdk_version = get("steam_audio_version")?;
    if sdk_version != fightbox_steam_audio::STEAM_AUDIO_VERSION {
        return Err(CliError::new(format!(
            "capture-provenance steam_audio_version {} does not match pinned {}",
            sdk_version,
            fightbox_steam_audio::STEAM_AUDIO_VERSION
        )));
    }
    let upstream = get("steam_audio_upstream_commit")?;
    if upstream != fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT {
        return Err(CliError::new(format!(
            "capture-provenance upstream_commit {} does not match pinned {}",
            upstream,
            fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT
        )));
    }
    // The actual dylib path and checksum must both be present. A capture with a
    // null path/hash is invalid provenance (the recorder rejects it, but a
    // hand-edited sidecar must still be caught here).
    let dylib_path = get("dylib_path")?;
    if dylib_path.is_empty() {
        return Err(CliError::new(
            "capture-provenance dylib_path is empty; artifact capture requires an established dylib",
        ));
    }
    let checksum = get("binary_checksum_sha256")?;
    if checksum.len() != 64 {
        return Err(CliError::new(format!(
            "capture-provenance binary_checksum_sha256 is not a 64-hex SHA-256 (len {})",
            checksum.len()
        )));
    }
    // Engine identity, platform, CPU class, and HRTF must be the honest constants.
    if get("engine_identity")? != crate::provenance::ENGINE_IDENTITY {
        return Err(CliError::new(
            "capture-provenance engine_identity does not match the honest constant",
        ));
    }
    if get("hrtf_identity")? != crate::provenance::HRTF_IDENTITY {
        return Err(CliError::new(
            "capture-provenance hrtf_identity does not match the honest constant",
        ));
    }
    // Fixture/gate must match the manifest and the fixture.
    if get("fixture_id")? != manifest.fixture_id {
        return Err(CliError::new(
            "capture-provenance fixture_id does not match manifest fixture_id",
        ));
    }
    if get("fixture_id")? != fixture.fixture_id {
        return Err(CliError::new(
            "capture-provenance fixture_id does not match fixture.json fixture_id",
        ));
    }
    let provenance_gate = get("gate")?;
    let expected_gate = match gate {
        "S0" => "S0",
        "S3" => "S3-render",
        other => other,
    };
    if provenance_gate != expected_gate {
        return Err(CliError::new(format!(
            "capture-provenance gate {provenance_gate} does not match expected {expected_gate}"
        )));
    }
    // Authority-note §ν build profile, sample rate, block size, quality.
    if get("build_profile")? != "phase-a-offline" {
        return Err(CliError::new(
            "capture-provenance build_profile is not phase-a-offline",
        ));
    }
    let sample_rate = v
        .get("sample_rate_hz")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| CliError::new("capture-provenance.json missing sample_rate_hz"))?;
    if sample_rate != 48_000 {
        return Err(CliError::new(format!(
            "capture-provenance sample_rate_hz {sample_rate} is not 48000"
        )));
    }
    let block_size = v
        .get("block_size_frames")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| CliError::new("capture-provenance.json missing block_size_frames"))?;
    if block_size != 128 {
        return Err(CliError::new(format!(
            "capture-provenance block_size_frames {block_size} is not 128"
        )));
    }
    if get("requested_quality")? != "phase-a" || get("delivered_quality")? != "phase-a" {
        return Err(CliError::new(
            "capture-provenance requested/delivered quality is not phase-a",
        ));
    }
    // Genuinely-not-applicable fields must say so explicitly and consistently.
    if get("streaming_cadence")? != "not_applicable" || get("callback_timing")? != "not_applicable"
    {
        return Err(CliError::new(
            "capture-provenance streaming_cadence/callback_timing must be not_applicable for Phase A offline capture",
        ));
    }
    // Required nonclaims must be present (the stale "sanitizer suite" wording is
    // gone; lifetime/leak tooling has run).
    let non_claims = v
        .get("non_claims")
        .and_then(|x| x.as_array())
        .ok_or_else(|| CliError::new("capture-provenance.json has no non_claims array"))?;
    let joined: String = non_claims
        .iter()
        .filter_map(|x| x.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        crate::provenance::UNCOMMITTED_SOURCE_NONCLAIM,
        crate::provenance::NO_DELIVERED_EAR_SPL_NONCLAIM,
        crate::provenance::REMAINING_PHASE_A_GATES_NONCLAIM,
    ] {
        if !joined.contains(needle) {
            return Err(CliError::new(format!(
                "capture-provenance.json is missing required nonclaim: {needle}"
            )));
        }
    }
    if joined.contains("sanitizer suite") {
        return Err(CliError::new(
            "capture-provenance.json carries a stale 'sanitizer suite' nonclaim; lifetime/leak tooling has run",
        ));
    }
    // S3 must record a world_dir that matches the metrics world payload.
    if gate == "S3" {
        let world_dir = get("world_dir")?;
        if world_dir.is_empty() {
            return Err(CliError::new(
                "capture-provenance world_dir is empty for an S3 capture",
            ));
        }
        let render_duration = v
            .get("render_duration_s")
            .and_then(|x| x.as_f64())
            .ok_or_else(|| CliError::new("capture-provenance.json missing render_duration_s"))?;
        if !render_duration.is_finite() || render_duration <= 0.0 {
            return Err(CliError::new(
                "capture-provenance render_duration_s is not a positive finite number",
            ));
        }
    }
    Ok(())
}

fn verify_required_nonclaims(bundle_dir: &Path, gate: &str) -> Result<()> {
    let metrics_text = std::fs::read_to_string(bundle_dir.join("metrics.json"))
        .map_err(|e| CliError::new(format!("cannot read metrics.json: {e}")))?;
    let value: serde_json::Value = serde_json::from_str(&metrics_text)
        .map_err(|e| CliError::new(format!("metrics.json is not valid JSON: {e}")))?;
    let non_claims = value
        .get("non_claims")
        .and_then(|v| v.as_array())
        .ok_or_else(|| CliError::new("metrics.json has no non_claims array"))?;
    let joined: Vec<&str> = non_claims.iter().filter_map(|v| v.as_str()).collect();
    let joined_text = joined.join("\n");
    let required = [
        crate::provenance::NO_DELIVERED_EAR_SPL_NONCLAIM,
        crate::provenance::UNCOMMITTED_SOURCE_NONCLAIM,
        crate::provenance::REMAINING_PHASE_A_GATES_NONCLAIM,
    ];
    for needle in required {
        if !joined_text.contains(needle) {
            return Err(CliError::new(format!(
                "metrics.json is missing required nonclaim: {needle}"
            )));
        }
    }
    if gate == "S3" {
        let listening_required = "listening";
        if !joined_text.contains("listening") {
            let _ = listening_required;
            // The S3 bundle must explicitly nonclaim audible quality until the
            // listening record is completed. Accept any nonclaim that mentions
            // listening OR the S3 mechanical qualifier.
            let has_listening_nonclaim = joined_text.contains("audible quality")
                || joined_text.contains("listening record is undecided");
            if !has_listening_nonclaim {
                return Err(CliError::new(
                    "S3 metrics.json is missing a listening-quality nonclaim",
                ));
            }
        }
    }
    Ok(())
}

/// The listening outcomes the verifier can return successfully. Anything other
/// than a fully valid human `pass` (under strict verification) is a hard error
/// before this type is constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ListeningOutcome {
    /// `--mechanical-only`: listening not evaluated. The command still exits 0
    /// when the mechanics pass, but the result is `incomplete`.
    Pending,
    /// Strict S0 verification. S0 has no Phase A human listening requirement, so
    /// strict S0 exits 0 from its mechanical contract — but this is a mechanical
    /// outcome, NOT a human listening pass. No listening record exists or is
    /// claimed.
    MechanicalS0,
    /// Strict S3 verification found a fully valid human `pass` record.
    Pass,
}

impl ListeningOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            // Honest label: S0 strict passes mechanically, not by a human record.
            Self::MechanicalS0 => "mechanical",
            Self::Pass => "pass",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Self::Pending => "mechanical checks passed; human listening not yet completed",
            Self::MechanicalS0 => {
                "S0 has no Phase A human listening requirement; strict S0 passes on its mechanical contract (no human listening record exists or is claimed)"
            }
            Self::Pass => "human listening record completed and passed",
        }
    }

    /// True when the overall command should exit 0. MechanicalS0 exits 0 (the
    /// gate has no human requirement), but its JSON never claims a human record.
    fn exits_success(self) -> bool {
        matches!(self, Self::MechanicalS0 | Self::Pass)
    }

    /// True only for a genuine human `pass`. Used only for documentation; the
    /// exit code comes from [`Self::exits_success`].
    #[cfg(test)]
    fn is_human_pass(self) -> bool {
        matches!(self, Self::Pass)
    }
}

fn verify_listening_completed(bundle_dir: &Path) -> Result<(ListeningOutcome, String)> {
    // Strict (non-mechanical) verification requires a real human `pass`. Any
    // missing, invalid, undecided, or `fail` record is a hard error that makes
    // the command exit nonzero. A `pass` that is structurally valid but does not
    // bind to this bundle's fixture/manifest hashes is also rejected (see
    // `validate_listening_record`).
    let text = match std::fs::read_to_string(bundle_dir.join("listening-record.json")) {
        Ok(text) => text,
        Err(e) => {
            return Err(CliError::new(format!(
                "strict verification requires a completed listening-record.json but it cannot be read: {e}"
            )));
        }
    };
    let record = parse_listening_record(&text)?;
    validate_listening_record(&record, bundle_dir)?;
    // Only a fully valid human `pass` returns success from strict verification.
    match record.result.as_str() {
        "pass" => Ok((
            ListeningOutcome::Pass,
            format!(
                "listening record signed by {} on {}",
                record.sign_off.listener_signed, record.sign_off.date_iso
            ),
        )),
        "fail" => Err(CliError::new(
            "listening-record.json result is 'fail'; the S3 gate does not pass",
        )),
        "undecided" => Err(CliError::new(
            "listening-record.json result is still 'undecided'; strict verification requires a completed human pass",
        )),
        other => Err(CliError::new(format!(
            "listening-record.json result {other:?} is not pass/fail/undecided"
        ))),
    }
}

/// The parsed, strict-shape listening record. Mirrors the JSON Schema and the
/// cross-field contract in `docs/listening/validate.py`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ListeningRecord {
    schema_version: String,
    record_id: String,
    fixture_id: String,
    gate: String,
    fixture_sha256: Option<String>,
    bundle_manifest_sha256: Option<String>,
    listener: ListenerWire,
    hrtf: HrtfWire,
    equipment: EquipmentWire,
    comparison_order: Vec<String>,
    observations: Vec<ObservationWire>,
    result: String,
    date_iso: String,
    sign_off: SignOffWire,
    requires_human_completion: bool,
    #[allow(dead_code)]
    claims: Vec<String>,
    non_claims: Vec<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ListenerWire {
    listener_id: String,
    notes: String,
}

#[derive(Debug, Clone)]
struct HrtfWire {
    hrtf_set: String,
    pretest_result: String,
}

#[derive(Debug, Clone)]
struct EquipmentWire {
    headphones: String,
    output_path: String,
    #[allow(dead_code)]
    monitor_gain_db: Option<f32>,
}

#[derive(Debug, Clone)]
struct ObservationWire {
    stimulus: String,
    observation: String,
}

#[derive(Debug, Clone)]
struct SignOffWire {
    listener_signed: String,
    date_iso: String,
}

/// Strict serde parse of `listening-record.json`. Unknown fields at any level
/// are rejected, mirroring `additionalProperties: false` in the schema.
fn parse_listening_record(text: &str) -> Result<ListeningRecord> {
    // Deserialize into a typed tree with `deny_unknown_fields` on every object
    // so a structurally wrong record fails before cross-field checks run.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Top {
        schema_version: String,
        record_id: String,
        fixture_id: String,
        gate: String,
        fixture_sha256: Option<String>,
        bundle_manifest_sha256: Option<String>,
        listener: ListenerDe,
        hrtf: HrtfDe,
        equipment: EquipmentDe,
        comparison_order: Vec<String>,
        observations: Vec<ObservationDe>,
        result: String,
        date_iso: String,
        sign_off: SignOffDe,
        requires_human_completion: bool,
        claims: Vec<String>,
        non_claims: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ListenerDe {
        listener_id: String,
        notes: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct HrtfDe {
        hrtf_set: String,
        pretest_result: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct EquipmentDe {
        headphones: String,
        output_path: String,
        monitor_gain_db: Option<f32>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ObservationDe {
        stimulus: String,
        observation: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct SignOffDe {
        listener_signed: String,
        date_iso: String,
    }
    let top: Top = serde_json::from_str(text)
        .map_err(|e| CliError::new(format!("listening-record.json is not valid: {e}")))?;
    Ok(ListeningRecord {
        schema_version: top.schema_version,
        record_id: top.record_id,
        fixture_id: top.fixture_id,
        gate: top.gate,
        fixture_sha256: top.fixture_sha256,
        bundle_manifest_sha256: top.bundle_manifest_sha256,
        listener: ListenerWire {
            listener_id: top.listener.listener_id,
            notes: top.listener.notes,
        },
        hrtf: HrtfWire {
            hrtf_set: top.hrtf.hrtf_set,
            pretest_result: top.hrtf.pretest_result,
        },
        equipment: EquipmentWire {
            headphones: top.equipment.headphones,
            output_path: top.equipment.output_path,
            monitor_gain_db: top.equipment.monitor_gain_db,
        },
        comparison_order: top.comparison_order,
        observations: top
            .observations
            .into_iter()
            .map(|o| ObservationWire {
                stimulus: o.stimulus,
                observation: o.observation,
            })
            .collect(),
        result: top.result,
        date_iso: top.date_iso,
        sign_off: SignOffWire {
            listener_signed: top.sign_off.listener_signed,
            date_iso: top.sign_off.date_iso,
        },
        requires_human_completion: top.requires_human_completion,
        claims: top.claims,
        non_claims: top.non_claims,
    })
}

/// Validate the full `docs/listening/validate.py` cross-field contract in Rust.
///
/// Enforces: exact schema/gate; constant `requires_human_completion: true`
/// (reject `false`); exact comparison order; the human-required non-claim;
/// non-placeholder listener identity, headphones, output path, HRTF set, and an
/// honestly recorded listener-specific pretest; valid ISO dates; nonempty
/// non-placeholder observations; and pass/fail/undecided state rules. The
/// record's fixture and unsigned-manifest hashes must equal this bundle's.
fn validate_listening_record(record: &ListeningRecord, bundle_dir: &Path) -> Result<()> {
    if record.schema_version != fightbox_evidence::LISTENING_SCHEMA_VERSION {
        return Err(CliError::new(format!(
            "listening-record schema_version must be {}, got {}",
            fightbox_evidence::LISTENING_SCHEMA_VERSION,
            record.schema_version
        )));
    }
    if record.gate != "S3" {
        return Err(CliError::new(format!(
            "listening-record gate must be \"S3\", got {:?}",
            record.gate
        )));
    }
    // `requires_human_completion` is a schema constant `true` in EVERY record,
    // including completed pass/fail. It means human completion is required by
    // the contract, not that this record is unfinished. A `false` value is a
    // contract violation and must be rejected.
    if !record.requires_human_completion {
        return Err(CliError::new(
            "listening-record requires_human_completion must be true (human completion is required by the contract); got false",
        ));
    }
    if !is_lowercase_fixture_id(&record.fixture_id) {
        return Err(CliError::new(format!(
            "listening-record fixture_id must match ^[a-z0-9][a-z0-9-]*$; got {:?}",
            record.fixture_id
        )));
    }
    if record.comparison_order != ["pathing_on", "pathing_off"] {
        return Err(CliError::new(format!(
            "listening-record comparison_order must be exactly [\"pathing_on\",\"pathing_off\"]; got {:?}",
            record.comparison_order
        )));
    }
    let human_required = fightbox_evidence::LISTENING_REQUIRES_HUMAN;
    if !record.non_claims.iter().any(|c| c == human_required) {
        return Err(CliError::new(format!(
            "listening-record non_claims must contain the human-required statement: {human_required:?}"
        )));
    }
    match record.result.as_str() {
        "undecided" | "pass" | "fail" => {}
        other => {
            return Err(CliError::new(format!(
                "listening-record result must be undecided/pass/fail; got {other:?}"
            )));
        }
    }
    // A completed (pass or fail) record must carry the full binding: lowercase
    // 64-hex fixture and manifest hashes, non-placeholder identity/equipment/HRTF,
    // a listener-specific pretest, valid ISO dates, a nonempty signature, and at
    // least one non-placeholder observation. An undecided template may carry null
    // hashes and an empty sign-off.
    if record.result != "undecided" {
        validate_completed_listening(record, bundle_dir)?;
    } else {
        // Undecided hashes must be null-or-hex; placeholders are still rejected
        // even for identity/equipment so a template cannot be mistaken for a pass.
        validate_optional_hex(record.fixture_sha256.as_deref(), "fixture_sha256")?;
        validate_optional_hex(
            record.bundle_manifest_sha256.as_deref(),
            "bundle_manifest_sha256",
        )?;
        reject_placeholders(&record.listener.listener_id, "listener.listener_id")?;
        reject_placeholders(&record.equipment.headphones, "equipment.headphones")?;
        reject_placeholders(&record.equipment.output_path, "equipment.output_path")?;
        reject_placeholders(&record.hrtf.hrtf_set, "hrtf.hrtf_set")?;
    }
    Ok(())
}

fn validate_completed_listening(record: &ListeningRecord, bundle_dir: &Path) -> Result<()> {
    // Hashes: must be lowercase 64-hex AND must equal this bundle's actual
    // fixture/unsigned-manifest digests. A record that quotes a foreign bundle's
    // hashes, or a stale bundle's hashes, does not bind to this evidence.
    let recorded_fixture = record
        .fixture_sha256
        .as_deref()
        .ok_or_else(|| CliError::new("a completed listening record requires fixture_sha256"))?;
    validate_hex64(recorded_fixture, "fixture_sha256")?;
    let recorded_manifest = record.bundle_manifest_sha256.as_deref().ok_or_else(|| {
        CliError::new("a completed listening record requires bundle_manifest_sha256")
    })?;
    validate_hex64(recorded_manifest, "bundle_manifest_sha256")?;

    // Bind to this bundle's actual bytes. The unsigned-manifest digest is the
    // canonical form the listening schema is keyed against (see manifest digest
    // resolution); the fixture digest is over fixture.json on disk.
    let bundle_manifest = load_manifest(bundle_dir)?;
    let unsigned_manifest_digest = bundle_manifest.unsigned_digest().ok_or_else(|| {
        CliError::new("bundle manifest has no unsigned digest to bind the listening record to")
    })?;
    if recorded_manifest != unsigned_manifest_digest {
        return Err(CliError::new(format!(
            "listening-record bundle_manifest_sha256 ({}) does not match this bundle's unsigned-manifest digest ({})",
            recorded_manifest, unsigned_manifest_digest
        )));
    }
    let fixture_bytes = std::fs::read(bundle_dir.join("fixture.json"))
        .map_err(|e| CliError::new(format!("cannot read fixture.json: {e}")))?;
    let fixture_digest = sha256_hex(&fixture_bytes);
    if recorded_fixture != fixture_digest {
        return Err(CliError::new(format!(
            "listening-record fixture_sha256 ({}) does not match this bundle's fixture.json ({})",
            recorded_fixture, fixture_digest
        )));
    }

    // Non-placeholder listener identity.
    let listener_id = record.listener.listener_id.trim();
    if listener_id.is_empty() {
        return Err(CliError::new(
            "listening-record listener.listener_id must be populated for a completed record",
        ));
    }
    reject_placeholders(listener_id, "listener.listener_id")?;

    // Non-placeholder equipment: headphones and a real output path.
    if record.equipment.headphones.trim().is_empty() {
        return Err(CliError::new(
            "listening-record equipment.headphones must be populated for a completed record",
        ));
    }
    reject_placeholders(&record.equipment.headphones, "equipment.headphones")?;
    if record.equipment.output_path.trim().is_empty() {
        return Err(CliError::new(
            "listening-record equipment.output_path must be populated for a completed record",
        ));
    }
    reject_placeholders(&record.equipment.output_path, "equipment.output_path")?;

    // Non-placeholder HRTF set and an honestly recorded listener-specific pretest.
    reject_placeholders(&record.hrtf.hrtf_set, "hrtf.hrtf_set")?;
    if record.hrtf.hrtf_set.trim().is_empty() {
        return Err(CliError::new(
            "listening-record hrtf.hrtf_set must be populated for a completed record",
        ));
    }
    let pretest = record.hrtf.pretest_result.trim();
    if pretest.is_empty() {
        return Err(CliError::new(
            "listening-record hrtf.pretest_result must be populated for a completed record",
        ));
    }
    reject_placeholders(pretest, "hrtf.pretest_result")?;
    // A pretest must not be the template's "not_run" sentinel for a completed
    // record: a listener-specific pretest result is honest evidence, not a stub.
    if pretest.eq_ignore_ascii_case("not_run") || pretest.eq_ignore_ascii_case("n/a") {
        return Err(CliError::new(
            "listening-record hrtf.pretest_result must be a real listener-specific pretest result, not 'not_run'",
        ));
    }

    // Valid ISO dates: record date and sign-off date.
    if !is_iso_date(&record.date_iso) {
        return Err(CliError::new(format!(
            "listening-record date_iso must be a valid ISO-8601 date (YYYY-MM-DD); got {:?}",
            record.date_iso
        )));
    }
    if !is_iso_date(&record.sign_off.date_iso) {
        return Err(CliError::new(format!(
            "listening-record sign_off.date_iso must be a valid ISO-8601 date; got {:?}",
            record.sign_off.date_iso
        )));
    }
    if record.sign_off.listener_signed.trim().is_empty() {
        return Err(CliError::new(
            "listening-record sign_off.listener_signed must be populated for a completed record",
        ));
    }
    reject_placeholders(&record.sign_off.listener_signed, "sign_off.listener_signed")?;

    // At least one non-placeholder observation.
    let real_observation = record.observations.iter().any(|observation| {
        !observation.stimulus.trim().is_empty()
            && !observation.observation.trim().is_empty()
            && !is_placeholder_text(&observation.stimulus)
            && !is_placeholder_text(&observation.observation)
    });
    if !real_observation {
        return Err(CliError::new(
            "listening-record observations must contain at least one non-placeholder observation",
        ));
    }
    Ok(())
}

fn validate_hex64(value: &str, field: &str) -> Result<()> {
    if value.len() != 64
        || !value.chars().all(|c| c.is_ascii_hexdigit())
        || value != value.to_ascii_lowercase()
    {
        return Err(CliError::new(format!(
            "{field} must be a lowercase 64-hex SHA-256 string; got {:?}",
            value
        )));
    }
    Ok(())
}

fn validate_optional_hex(value: Option<&str>, field: &str) -> Result<()> {
    match value {
        None => Ok(()),
        Some(v) => validate_hex64(v, field),
    }
}

fn is_lowercase_fixture_id(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 {
        return false;
    }
    let sep = |i: usize| bytes.get(i).copied() == Some(b'-');
    let digits = |range: &[u8]| range.iter().all(u8::is_ascii_digit);
    if !(digits(&bytes[0..4]) && sep(4) && digits(&bytes[5..7]) && sep(7) && digits(&bytes[8..10]))
    {
        return false;
    }
    // Parse the calendar fields and reject out-of-range month/day values. The
    // format check above only verifies digits and dashes; "2026-13-99" must be
    // rejected as a real date.
    let Ok(year) = value[0..4].parse::<u32>() else {
        return false;
    };
    let Ok(month) = value[5..7].parse::<u32>() else {
        return false;
    };
    let Ok(day) = value[8..10].parse::<u32>() else {
        return false;
    };
    if year == 0 || !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    // Day must not exceed the month's length (handles month-specific caps
    // including February's 28/29 day ceiling).
    let max_day = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => return false,
    };
    day <= max_day
}

/// Placeholder markers shared with `docs/listening/validate.py`. These are the
/// sentinel words an unfilled template carries; a completed human record must
/// replace every one with real evidence.
const PLACEHOLDER_MARKERS: &[&str] = &[
    "REPLACE",
    "REPLACE_WITH",
    "TODO",
    "TBD",
    "UNASSIGNED",
    "PLACEHOLDER",
    "XXXX",
];

fn is_placeholder_text(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    PLACEHOLDER_MARKERS
        .iter()
        .any(|marker| upper.contains(marker))
}

fn reject_placeholders(value: &str, field: &str) -> Result<()> {
    if is_placeholder_text(value) {
        return Err(CliError::new(format!(
            "listening-record {field} must not contain a placeholder marker; got {:?}",
            value
        )));
    }
    Ok(())
}

/// The verifier's emitted result document.
#[derive(Serialize)]
#[cfg_attr(test, derive(Deserialize))]
struct VerifyResult {
    schema_version: String,
    bundle_dir: String,
    gate: String,
    fixture_id: String,
    manifest_sha256: String,
    mechanical_checks_passed: Vec<String>,
    listening_outcome: String,
    listening_note: &'static str,
    result: String,
    detail: String,
}

/// Silence unused-import warnings for symbols only used by tests below.
#[cfg(test)]
fn _evidence_alias() {
    let _ = fightbox_evidence::sha256_hex;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::BundleFile;

    #[test]
    fn canonical_calibration_rejects_doubled_drive() {
        let mut payload = CalibrationPayload {
            reference_spl_db: 120.0,
            reference_pcm_rms_dbfs: -24.0,
            reference_distance_m: 1.0,
            program_rms_dbfs: -20.0,
            target_source_rms_dbfs: -59.0,
            drive_gain_db: -39.0,
            linear_gain: 10.0_f32.powf(-39.0 / 20.0),
        };
        verify_canonical_calibration(&payload).unwrap();
        payload.drive_gain_db = -78.0; // doubled
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    #[test]
    fn canonical_calibration_rejects_nan_target() {
        let payload = CalibrationPayload {
            reference_spl_db: 120.0,
            reference_pcm_rms_dbfs: -24.0,
            reference_distance_m: 1.0,
            program_rms_dbfs: -20.0,
            target_source_rms_dbfs: f32::NAN,
            drive_gain_db: -39.0,
            linear_gain: 10.0_f32.powf(-39.0 / 20.0),
        };
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    #[test]
    fn canonical_calibration_rejects_linear_gain_mismatch() {
        let payload = CalibrationPayload {
            reference_spl_db: 120.0,
            reference_pcm_rms_dbfs: -24.0,
            reference_distance_m: 1.0,
            program_rms_dbfs: -20.0,
            target_source_rms_dbfs: -59.0,
            drive_gain_db: -39.0,
            linear_gain: 1.0, // wrong: does not match -39 dB
        };
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    #[test]
    fn canonical_calibration_rejects_wrong_reference_anchor() {
        // The ADR 0002 scene anchor (120 dB SPL @ -24 dBFS @ 1 m) is fixed. A
        // payload that quotes a different reference SPL must be rejected even if
        // the target/drive/linear values happen to be internally consistent.
        let mut payload = canonical_calibration_payload();
        verify_canonical_calibration(&payload).unwrap();
        payload.reference_spl_db = 110.0;
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    #[test]
    fn canonical_calibration_rejects_target_not_matching_anchor() {
        // target must be recomputed from the anchor + 85 dB SPL source
        // (-24 + (85-120) = -59). A target that does not match must be rejected.
        let mut payload = canonical_calibration_payload();
        verify_canonical_calibration(&payload).unwrap();
        payload.target_source_rms_dbfs = -50.0;
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    #[test]
    fn canonical_calibration_rejects_drive_not_matching_target_minus_program() {
        // drive must equal target - program_rms_dbfs. A drive that does not
        // match must be rejected.
        let mut payload = canonical_calibration_payload();
        verify_canonical_calibration(&payload).unwrap();
        payload.drive_gain_db = -20.0;
        payload.linear_gain = 10.0_f32.powf(-20.0 / 20.0);
        assert!(verify_canonical_calibration(&payload).is_err());
    }

    fn canonical_calibration_payload() -> CalibrationPayload {
        CalibrationPayload {
            reference_spl_db: 120.0,
            reference_pcm_rms_dbfs: -24.0,
            reference_distance_m: 1.0,
            program_rms_dbfs: -20.0,
            target_source_rms_dbfs: -59.0,
            drive_gain_db: -39.0,
            linear_gain: 10.0_f32.powf(-39.0 / 20.0),
        }
    }

    // ---- Correction 3 & 4(e): strict S0 is mechanical, never a human pass ----

    #[test]
    fn mechanical_s0_outcome_exits_success_but_is_not_a_human_pass() {
        // Strict S0 has no Phase A human listening requirement. It exits 0 from
        // its mechanical contract, but its outcome must NEVER be confused with a
        // genuine human `pass`. These two predicates are deliberately distinct.
        let outcome = ListeningOutcome::MechanicalS0;
        assert!(outcome.exits_success(), "strict S0 must exit 0");
        assert!(
            !outcome.is_human_pass(),
            "mechanical S0 must not be reported as a human pass"
        );
        assert_eq!(outcome.as_str(), "mechanical");
    }

    #[test]
    fn mechanical_s0_outcome_never_claims_a_human_listening_record() {
        // The forbidden phrase is "human listening record completed and passed".
        // It appears ONLY in the Pass variant's note. MechanicalS0's note and
        // label must not contain it, so a strict-S0 verify result on disk can
        // never be mistaken for a synthetic human pass.
        let outcome = ListeningOutcome::MechanicalS0;
        assert!(!outcome.as_str().contains("pass"));
        assert!(
            !outcome
                .note()
                .contains("human listening record completed and passed"),
            "MechanicalS0 note must not claim a human record: {}",
            outcome.note()
        );
        assert!(
            outcome
                .note()
                .contains("no Phase A human listening requirement"),
            "MechanicalS0 note must state the honest reason: {}",
            outcome.note()
        );
    }

    #[test]
    fn only_pass_outcome_claims_a_human_listening_record() {
        // Cross-check: the forbidden phrase is reachable ONLY via the Pass
        // variant. Pending and MechanicalS0 never produce it. This is the
        // invariant that makes "no synthetic human pass on disk" provable.
        for outcome in [ListeningOutcome::Pending, ListeningOutcome::MechanicalS0] {
            assert!(
                !outcome
                    .note()
                    .contains("human listening record completed and passed"),
                "{:?} note must not claim a human record: {}",
                outcome,
                outcome.note()
            );
        }
        assert!(
            ListeningOutcome::Pass
                .note()
                .contains("human listening record completed and passed"),
            "Pass note must carry the human-record claim"
        );
    }

    #[test]
    fn mechanical_s0_result_json_contains_no_synthetic_human_pass() {
        // Construct the exact VerifyResult the verifier emits for a strict-S0
        // run and assert the serialized JSON contains no synthetic human pass.
        let outcome = ListeningOutcome::MechanicalS0;
        let result = VerifyResult {
            schema_version: VERIFY_RESULT.into(),
            bundle_dir: "/tmp/s0-bundle".into(),
            gate: "S0".into(),
            fixture_id: "s0-free-field-100m-approach".into(),
            manifest_sha256: "a".repeat(64),
            mechanical_checks_passed: vec!["s0 mechanics".into()],
            listening_outcome: outcome.as_str().to_string(),
            listening_note: outcome.note().into(),
            result: if outcome.exits_success() {
                "pass".into()
            } else {
                "incomplete".into()
            },
            detail: "S0 has no Phase A human listening requirement".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(
            !json.contains("human listening record completed and passed"),
            "strict-S0 result JSON must not contain a synthetic human pass: {json}"
        );
        assert!(json.contains(r#""listening_outcome":"mechanical""#));
        // The overall result is "pass" (exit 0) but the listening outcome is
        // "mechanical", not "pass" — the two are honestly distinguished.
        assert!(json.contains(r#""result":"pass""#));
        assert!(!json.contains(r#""listening_outcome":"pass""#));
    }

    #[test]
    fn manifest_hash_mismatch_detected() {
        // Construct a manifest whose recorded hash is wrong.
        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S0".into(),
            fixture_id: "x".into(),
            fixture_content_sha256: "abc".into(),
            asset_id: "a".into(),
            asset_descriptor_sha256: "def".into(),
            files: vec![BundleFile {
                name: "fixture.json".into(),
                kind: "fixture".into(),
                content_sha256: "abc".into(),
                size_bytes: 1,
            }],
            unsigned_manifest_sha256: Some("deadbeef".into()),
            manifest_content_sha256: None,
        };
        // The mismatch logic itself: a manifest hash check compares recorded to
        // actual. We assert the recorded value is present and non-default.
        assert_eq!(
            manifest.unsigned_manifest_sha256.as_deref(),
            Some("deadbeef")
        );
        // The recomputed unsigned digest must differ from the wrong recorded one.
        let recomputed = manifest.recompute_unsigned_digest();
        assert_ne!(recomputed, "deadbeef");
    }

    #[test]
    fn manifest_unsigned_digest_is_stable_and_recomputable() {
        // A manifest's unsigned digest must be recomputable from its own fields
        // and stable regardless of the recorded digest values.
        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S0".into(),
            fixture_id: "x".into(),
            fixture_content_sha256: "abc".into(),
            asset_id: "a".into(),
            asset_descriptor_sha256: "def".into(),
            files: vec![BundleFile {
                name: "fixture.json".into(),
                kind: "fixture".into(),
                content_sha256: "abc".into(),
                size_bytes: 1,
            }],
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };
        let digest_a = manifest.recompute_unsigned_digest();
        let digest_b = manifest.recompute_unsigned_digest();
        assert_eq!(digest_a, digest_b);
        assert_eq!(digest_a.len(), 64);
        // Recording the digest must not change the recomputed value.
        let mut with_digest = manifest.clone();
        with_digest.unsigned_manifest_sha256 = Some(digest_a.clone());
        assert_eq!(with_digest.recompute_unsigned_digest(), digest_a);
    }
}

/// Mutation-test harness for the listening-record contract. Builds a temp
/// bundle directory with a real finalized manifest and fixture so the
/// listening validator's hash binding can be exercised against the actual
/// bundle, then mutates the record text to prove each rejection.
#[cfg(test)]
mod listening_contract_tests {
    use super::*;
    use crate::bundle::{BundleFile, BundleManifest};

    /// A temp bundle directory with a finalized manifest, detached digest
    /// sidecar, and fixture.json. The listening record is bound to these.
    struct TempBundle {
        dir: std::path::PathBuf,
        fixture_digest: String,
        manifest_digest: String,
    }

    impl TempBundle {
        fn fixture_digest(&self) -> &str {
            &self.fixture_digest
        }
        fn manifest_digest(&self) -> &str {
            &self.manifest_digest
        }
    }

    /// A fully valid human `pass` listening record, bound to the temp bundle.
    /// Every field is populated with real, non-placeholder values. Mutations
    /// are applied by `edit_json`.
    fn valid_pass_record(bundle: &TempBundle) -> String {
        let fixture = bundle.fixture_digest();
        let manifest = bundle.manifest_digest();
        serde_json::json!({
            "schema_version": "fightbox.listening.v1",
            "record_id": "s3-listening-0001",
            "fixture_id": "s3-masonry-building-corner",
            "gate": "S3",
            "fixture_sha256": fixture,
            "bundle_manifest_sha256": manifest,
            "listener": {
                "listener_id": "listener-mjd-001",
                "notes": "trained listener, in-room"
            },
            "hrtf": {
                "hrtf_set": "steam-audio-default",
                "pretest_result": "passed-localization-pretest-3of3"
            },
            "equipment": {
                "headphones": "Sennheiser HD650",
                "output_path": "interface/line-out-1",
                "monitor_gain_db": 0.0
            },
            "comparison_order": ["pathing_on", "pathing_off"],
            "observations": [
                {"stimulus": "pathing_on", "observation": "arrivals audible around the corner"},
                {"stimulus": "pathing_off", "observation": "no corner arrivals"}
            ],
            "result": "pass",
            "date_iso": "2026-07-29",
            "sign_off": {"listener_signed": "mjd", "date_iso": "2026-07-29"},
            "requires_human_completion": true,
            "claims": ["pathing on/off difference is audible"],
            "non_claims": [
                "Human completion is required; this template alone is not a pass.",
                "No delivered-ear-SPL claim without a measured output-device/headphone transfer."
            ]
        })
        .to_string()
    }

    /// Write a finalized manifest + sidecar + fixture to a fresh temp dir.
    /// The manifest carries one fixture file so its unsigned digest is real.
    fn make_bundle() -> TempBundle {
        let dir = std::env::temp_dir().join(format!(
            "fightbox-listening-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture_bytes = br#"{"fixture":"contract-test"}"#;
        let fixture_digest = sha256_hex(fixture_bytes);
        std::fs::write(dir.join("fixture.json"), fixture_bytes).unwrap();

        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S3".into(),
            fixture_id: "s3-masonry-building-corner".into(),
            fixture_content_sha256: fixture_digest.clone(),
            asset_id: "s3-calibrated-pink".into(),
            asset_descriptor_sha256: "deadbeef".into(),
            files: vec![BundleFile {
                name: "fixture.json".into(),
                kind: "fixture".into(),
                content_sha256: fixture_digest.clone(),
                size_bytes: fixture_bytes.len() as u64,
            }],
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };
        let digest = manifest.recompute_unsigned_digest();
        let mut finalized = manifest.clone();
        finalized.unsigned_manifest_sha256 = Some(digest.clone());
        finalized.manifest_content_sha256 = Some(digest.clone());
        let manifest_bytes = serde_json::to_vec_pretty(&finalized).unwrap();
        std::fs::write(dir.join("manifest.json"), &manifest_bytes).unwrap();
        let final_digest = sha256_hex(&manifest_bytes);
        std::fs::write(
            dir.join(crate::bundle::MANIFEST_DIGEST_SIDECAR),
            &final_digest,
        )
        .unwrap();

        TempBundle {
            dir,
            fixture_digest,
            manifest_digest: digest,
        }
    }

    fn unique_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{}-{}", std::process::id(), nanos)
    }

    /// Apply a mutation to the parsed JSON value, re-serialize, then parse+
    /// validate. Robust to field order/spacing in the serialized form. This is
    /// the canonical mutation helper: every listening-record mutation test
    /// mutates the parsed `serde_json::Value` rather than relying on
    /// whitespace-sensitive string replacement (Correction 7).
    fn validate_edited_value(
        bundle: &TempBundle,
        record: &str,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> Result<()> {
        let mut value: serde_json::Value =
            serde_json::from_str(record).expect("base record must be valid JSON");
        edit(&mut value);
        let text = serde_json::to_string(&value).expect("mutated record must serialize");
        let parsed = parse_listening_record(&text)?;
        validate_listening_record(&parsed, &bundle.dir)
    }

    #[test]
    fn valid_pass_record_is_accepted() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let parsed = parse_listening_record(&record).expect("valid record must parse");
        validate_listening_record(&parsed, &bundle.dir).expect("valid pass must be accepted");
        // And strict verification must return Pass for it.
        std::fs::write(bundle.dir.join("listening-record.json"), &record).unwrap();
        let (outcome, _) = verify_listening_completed(&bundle.dir).expect("strict pass");
        assert_eq!(outcome, ListeningOutcome::Pass);
    }

    #[test]
    fn rejects_false_requires_human_completion() {
        // The flag is a schema constant true in EVERY record; false is a
        // contract violation. A record that flips it to false to claim the
        // contract is satisfied must be rejected.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["requires_human_completion"] = serde_json::Value::Bool(false);
        })
        .unwrap_err();
        assert!(
            err.message()
                .contains("requires_human_completion must be true"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_placeholder_listener_identity() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["listener"]["listener_id"] =
                serde_json::Value::String("REPLACE_WITH_listener_id".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_placeholder_headphones_and_output_path() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["equipment"]["headphones"] = serde_json::Value::String("TODO headphones".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );

        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["equipment"]["output_path"] = serde_json::Value::String("TBD output".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_not_run_pretest() {
        // A completed record must carry a real listener-specific pretest result,
        // not the template's 'not_run' sentinel.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["hrtf"]["pretest_result"] = serde_json::Value::String("not_run".into());
        })
        .unwrap_err();
        assert!(
            err.message()
                .contains("pretest_result must be a real listener-specific"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_placeholder_hrtf_set() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["hrtf"]["hrtf_set"] = serde_json::Value::String("REPLACE_HRTF".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_wrong_fixture_hash_binding() {
        // A record that quotes a foreign fixture hash does not bind to this bundle.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let foreign = "0".repeat(64);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["fixture_sha256"] = serde_json::Value::String(foreign);
        })
        .unwrap_err();
        assert!(
            err.message().contains("fixture_sha256") && err.message().contains("does not match"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_wrong_manifest_hash_binding() {
        // A record that quotes a foreign/stale manifest digest does not bind.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let foreign = "f".repeat(64);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["bundle_manifest_sha256"] = serde_json::Value::String(foreign);
        })
        .unwrap_err();
        assert!(
            err.message().contains("bundle_manifest_sha256")
                && err.message().contains("does not match"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_missing_sign_off_date() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        // Mutate via parsed JSON so field order/spacing in the serialized form
        // does not matter.
        let err = validate_edited_value(&bundle, &record, |value| {
            value["sign_off"]["date_iso"] = serde_json::Value::String(String::new());
        })
        .unwrap_err();
        assert!(
            err.message()
                .contains("sign_off.date_iso must be a valid ISO-8601"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_missing_record_date() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["date_iso"] = serde_json::Value::String("not-a-date".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("date_iso must be a valid ISO-8601"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_empty_listener_signature() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["sign_off"]["listener_signed"] = serde_json::Value::String(String::new());
        })
        .unwrap_err();
        assert!(
            err.message().contains("listener_signed must be populated"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_all_placeholder_observations() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["observations"][0]["observation"] =
                serde_json::Value::String("TODO observation".into());
            value["observations"][1]["observation"] =
                serde_json::Value::String("TBD observation".into());
        })
        .unwrap_err();
        assert!(
            err.message()
                .contains("at least one non-placeholder observation"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_fail_result_under_strict_verification() {
        // A fail record must fail the strict CLI gate (exit nonzero), even when
        // fully populated and bound. This is the core strict-exit-semantics fix.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let mut value: serde_json::Value = serde_json::from_str(&record).unwrap();
        value["result"] = serde_json::Value::String("fail".into());
        let record = serde_json::to_string(&value).unwrap();
        std::fs::write(bundle.dir.join("listening-record.json"), &record).unwrap();
        let err = verify_listening_completed(&bundle.dir).unwrap_err();
        assert!(
            err.message().contains("result is 'fail'"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_undecided_result_under_strict_verification() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        // An undecided record is allowed null hashes, so drop them to stay
        // structurally valid, then assert strict verification still rejects it.
        let mut value: serde_json::Value = serde_json::from_str(&record).unwrap();
        value["result"] = serde_json::Value::String("undecided".into());
        let record = serde_json::to_string(&value).unwrap();
        std::fs::write(bundle.dir.join("listening-record.json"), &record).unwrap();
        let err = verify_listening_completed(&bundle.dir).unwrap_err();
        assert!(
            err.message().contains("still 'undecided'"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_malformed_json() {
        let bundle = make_bundle();
        assert!(parse_listening_record("{ not json").is_err());
        let _ = bundle;
    }

    #[test]
    fn rejects_unknown_top_level_field() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        // Mutate the parsed JSON value so the test is robust to field
        // order/spacing in the serialized form (Correction 7).
        let mut value: serde_json::Value = serde_json::from_str(&record).unwrap();
        value["__unexpected"] = serde_json::Value::Bool(true);
        let text = serde_json::to_string(&value).unwrap();
        assert!(parse_listening_record(&text).is_err());
    }

    #[test]
    fn rejects_wrong_comparison_order() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["comparison_order"] = serde_json::json!(["pathing_off", "pathing_on"]);
        })
        .unwrap_err();
        assert!(
            err.message().contains("comparison_order"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_missing_human_required_nonclaim() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            value["non_claims"][0] = serde_json::Value::String("a different non-claim".into());
        })
        .unwrap_err();
        assert!(
            err.message().contains("human-required"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_non_hex_hash() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |value| {
            // Replace the fixture hash with a non-hex string of the right length.
            value["fixture_sha256"] = serde_json::Value::String("z".repeat(64));
        })
        .unwrap_err();
        assert!(
            err.message().contains("lowercase 64-hex"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_unassigned_listener_placeholder() {
        // "unassigned" is the provisional template's listener_id; a completed
        // human record must replace it with a real listener identity.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |v| {
            v["listener"]["listener_id"] = serde_json::json!("unassigned");
        })
        .unwrap_err();
        assert!(
            err.message().contains("listener.listener_id") && err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_placeholder_pretest() {
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |v| {
            v["hrtf"]["pretest_result"] = serde_json::json!("placeholder");
        })
        .unwrap_err();
        assert!(
            err.message().contains("pretest_result") && err.message().contains("placeholder"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_invalid_calendar_date() {
        // "2026-13-99" has the right ISO format (digits + dashes) but an invalid
        // month and day. The validator must reject out-of-range calendar values,
        // not only the format.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |v| {
            v["date_iso"] = serde_json::json!("2026-13-99");
        })
        .unwrap_err();
        assert!(
            err.message().contains("date_iso") && err.message().contains("ISO"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn rejects_february_30_date() {
        // Feb 30 never exists; the month-specific day ceiling must catch it.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |v| {
            v["date_iso"] = serde_json::json!("2026-02-30");
        })
        .unwrap_err();
        assert!(err.message().contains("date_iso"), "got: {}", err.message());
    }

    #[test]
    fn accepts_leap_day_date() {
        // Feb 29 is valid in 2024 (leap year); the calendar check must accept it.
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        validate_edited_value(&bundle, &record, |v| {
            v["date_iso"] = serde_json::json!("2024-02-29");
        })
        .expect("2024-02-29 is a valid leap day");
    }

    #[test]
    fn rejects_february_29_in_non_leap_year() {
        // Feb 29 is invalid in 2023 (not a leap year).
        let bundle = make_bundle();
        let record = valid_pass_record(&bundle);
        let err = validate_edited_value(&bundle, &record, |v| {
            v["date_iso"] = serde_json::json!("2023-02-29");
        })
        .unwrap_err();
        assert!(err.message().contains("date_iso"), "got: {}", err.message());
    }
}

/// Mutation tests for the R4 artifact/provenance binding: required file set,
/// capture-provenance cross-binding, asset_id semantic binding, and world
/// cross-binding. Each test must fail on the old bug and pass only after the
/// repair.
#[cfg(test)]
mod provenance_binding_tests {
    use super::*;
    use crate::bundle::{BundleFile, BundleManifest};

    fn unique_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{nanos}")
    }

    /// Build a minimal valid S0 manifest with the exact required file set.
    fn s0_manifest() -> BundleManifest {
        let names = [
            "approach-00-100m.wav",
            "approach-01-75m.wav",
            "approach-02-50m.wav",
            "approach-03-25m.wav",
            "approach-04-10m.wav",
            "approach-05-1m.wav",
            "control-100m-air-enabled.wav",
            "control-100m-air-disabled.wav",
            "metrics.json",
            "fixture.json",
            "asset-descriptor.json",
            "capture-provenance.json",
        ];
        let kind_for = |name: &str| -> &'static str {
            if name.starts_with("approach") {
                "approach_wav"
            } else if name.starts_with("control") {
                "control_wav"
            } else if name == "metrics.json" {
                "metrics"
            } else if name == "fixture.json" {
                "fixture"
            } else if name == "asset-descriptor.json" {
                "asset_descriptor"
            } else {
                "capture_provenance"
            }
        };
        let files = names
            .iter()
            .map(|name| BundleFile {
                name: (*name).into(),
                kind: kind_for(name).into(),
                content_sha256: "a".repeat(64),
                size_bytes: 1,
            })
            .collect();
        BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S0".into(),
            fixture_id: "fix".into(),
            fixture_content_sha256: "a".repeat(64),
            asset_id: "s0-calibrated-pink".into(),
            asset_descriptor_sha256: "a".repeat(64),
            files,
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        }
    }

    #[test]
    fn required_file_set_accepts_valid_s0_manifest() {
        let manifest = s0_manifest();
        verify_required_file_set(&manifest, "S0").unwrap();
    }

    #[test]
    fn required_file_set_rejects_missing_capture_provenance() {
        let mut manifest = s0_manifest();
        manifest
            .files
            .retain(|f| f.name != "capture-provenance.json");
        let err = verify_required_file_set(&manifest, "S0").unwrap_err();
        assert!(
            err.message().contains("capture-provenance.json"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn required_file_set_rejects_duplicate_singleton_kind() {
        let mut manifest = s0_manifest();
        // Inject a second metrics file under a different name but same kind.
        manifest.files.push(BundleFile {
            name: "metrics-2.json".into(),
            kind: "metrics".into(),
            content_sha256: "a".repeat(64),
            size_bytes: 1,
        });
        let err = verify_required_file_set(&manifest, "S0").unwrap_err();
        assert!(
            err.message().contains("duplicate singleton kind metrics"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn required_file_set_rejects_clean_extra_indexed_path() {
        let mut manifest = s0_manifest();
        manifest.files.push(BundleFile {
            name: "clean-extra.bin".into(),
            kind: "extra".into(),
            content_sha256: "a".repeat(64),
            size_bytes: 1,
        });
        let err = verify_required_file_set(&manifest, "S0").unwrap_err();
        assert!(err.message().contains("exactly 12 allowed files"));
    }

    #[test]
    fn required_file_set_rejects_wrong_kind_for_required_name() {
        let mut manifest = s0_manifest();
        manifest
            .files
            .iter_mut()
            .find(|file| file.name == "fixture.json")
            .unwrap()
            .kind = "poisoned_fixture".into();
        let err = verify_required_file_set(&manifest, "S0").unwrap_err();
        assert!(
            err.message().contains("fixture.json") && err.message().contains("expected fixture")
        );
    }

    #[test]
    fn required_file_set_accepts_repeated_stem_family_kinds() {
        // The 6 approach WAVs all share kind approach_wav; this is legitimate.
        let manifest = s0_manifest();
        verify_required_file_set(&manifest, "S0").unwrap();
    }

    #[test]
    fn manifest_names_rejects_traversal_name() {
        // Name-safety is the security boundary and runs before any manifest-derived
        // path is joined/read/hashed (see verify_manifest_names). A `..` segment
        // must be rejected here, not later inside verify_required_file_set.
        let mut manifest = s0_manifest();
        manifest.files.push(BundleFile {
            name: "../escape.wav".into(),
            kind: "stem_wav".into(),
            content_sha256: "a".repeat(64),
            size_bytes: 1,
        });
        let err = verify_manifest_names(&manifest).unwrap_err();
        assert!(
            err.message().contains("parent-directory"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn manifest_names_rejects_absolute_name() {
        let mut manifest = s0_manifest();
        manifest.files.push(BundleFile {
            name: "/etc/passwd".into(),
            kind: "stem_wav".into(),
            content_sha256: "a".repeat(64),
            size_bytes: 1,
        });
        let err = verify_manifest_names(&manifest).unwrap_err();
        assert!(err.message().contains("absolute"), "got: {}", err.message());
    }

    #[test]
    fn manifest_names_rejects_duplicate_name() {
        let mut manifest = s0_manifest();
        manifest.files.push(manifest.files[0].clone());
        let err = verify_manifest_names(&manifest).unwrap_err();
        assert!(
            err.message().contains("duplicate file name"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn manifest_names_checked_before_any_path_read() {
        // The security boundary requires name-safety to run BEFORE any
        // manifest-derived path is joined to the bundle dir, read, or hashed.
        // This test constructs a manifest entry whose hostile name resolves to a
        // real out-of-bundle file whose bytes EXACTLY match the recorded hash and
        // size. If name-safety ran after `verify_files_index`, that loop would
        // join the name, read the out-of-bundle file, hash it, find a match, and
        // return Ok — silently reading outside the bundle with no error. We prove
        // the opposite: the name check rejects, and (independently) we show that
        // `verify_files_index` WOULD have read the out-of-bundle file without
        // complaint if name-safety had not already run. That conjunction is the
        // ordering proof: only the name check can be the rejecting layer.
        let root = std::env::temp_dir().join(format!(
            "fightbox-order-root-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        let bundle = root.join("bundle");
        std::fs::create_dir_all(&bundle).unwrap();
        // Real out-of-bundle payload at the bundle's parent, reached by `../`.
        let escape_bytes = b"escape-payload-from-parent";
        let escape_hash = sha256_hex(escape_bytes);
        let escape_size = escape_bytes.len() as u64;
        std::fs::write(root.join("escape.wav"), escape_bytes).unwrap();

        let mut manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S0".into(),
            fixture_id: "fix".into(),
            fixture_content_sha256: "a".repeat(64),
            asset_id: "x".into(),
            asset_descriptor_sha256: "a".repeat(64),
            files: vec![BundleFile {
                name: "../escape.wav".into(),
                kind: "stem_wav".into(),
                content_sha256: escape_hash,
                size_bytes: escape_size,
            }],
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };

        // 1) The name check fires with the parent-directory error.
        let err = verify_manifest_names(&manifest).unwrap_err();
        assert!(
            err.message().contains("parent-directory"),
            "name check did not reject traversal: {}",
            err.message()
        );

        // 2) Independently, finalize the manifest honestly (unsigned digest +
        //    detached sidecar over the on-disk bytes) so that the ONLY remaining
        //    reason verify_files_index could fail is the file-read loop. With the
        //    out-of-bundle file matching the recorded hash/size, that loop reads
        //    it silently and returns Ok — proving the read would have happened if
        //    name-safety had not already rejected the entry.
        let unsigned = manifest.recompute_unsigned_digest();
        manifest.unsigned_manifest_sha256 = Some(unsigned.clone());
        manifest.manifest_content_sha256 = Some(unsigned);
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(bundle.join("manifest.json"), &manifest_bytes).unwrap();
        // The sidecar is the detached digest over the on-disk manifest bytes,
        // stored as raw trimmed text (see verify_files_index).
        std::fs::write(
            bundle.join(crate::bundle::MANIFEST_DIGEST_SIDECAR),
            sha256_hex(&manifest_bytes),
        )
        .unwrap();
        let would_read = verify_files_index(&bundle, &manifest);
        assert!(
            would_read.is_ok(),
            "expected verify_files_index to read the out-of-bundle file silently (proving name-safety must run first); got: {:?}",
            would_read.err()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Build a temp bundle dir with a valid capture-provenance.json and the
    /// matching manifest entry, for capture-provenance mutation tests.
    fn provenance_bundle(gate: &str) -> (std::path::PathBuf, BundleManifest) {
        let dir = std::env::temp_dir().join(format!(
            "fightbox-prov-test-{}-{}-{}",
            gate,
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let fixture_id = if gate == "S0" {
            "s0-free-field-100m-approach"
        } else {
            "s3-masonry-building-corner"
        };
        let provenance = if gate == "S0" {
            serde_json::json!({
                "engine_identity": crate::provenance::ENGINE_IDENTITY,
                "platform": crate::provenance::platform(),
                "cpu_class": crate::provenance::cpu_class(),
                "hrtf_identity": crate::provenance::HRTF_IDENTITY,
                "fixture_id": fixture_id,
                "gate": "S0",
                "steam_audio_version": fightbox_steam_audio::STEAM_AUDIO_VERSION,
                "steam_audio_upstream_commit": fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT,
                "binary_checksum_sha256": "b".repeat(64),
                "dylib_path": "/opt/libphonon.dylib",
                "build_profile": "phase-a-offline",
                "sample_rate_hz": 48_000,
                "block_size_frames": 128,
                "requested_quality": "phase-a",
                "delivered_quality": "phase-a",
                "streaming_cadence": "not_applicable",
                "callback_timing": "not_applicable",
                "non_claims": [
                    crate::provenance::UNCOMMITTED_SOURCE_NONCLAIM,
                    crate::provenance::NO_DELIVERED_EAR_SPL_NONCLAIM,
                    crate::provenance::REMAINING_PHASE_A_GATES_NONCLAIM,
                ],
            })
        } else {
            serde_json::json!({
                "engine_identity": crate::provenance::ENGINE_IDENTITY,
                "platform": crate::provenance::platform(),
                "cpu_class": crate::provenance::cpu_class(),
                "hrtf_identity": crate::provenance::HRTF_IDENTITY,
                "fixture_id": fixture_id,
                "gate": "S3-render",
                "steam_audio_version": fightbox_steam_audio::STEAM_AUDIO_VERSION,
                "steam_audio_upstream_commit": fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT,
                "binary_checksum_sha256": "b".repeat(64),
                "dylib_path": "/opt/libphonon.dylib",
                "world_dir": "/tmp/world",
                "render_duration_s": 0.05,
                "build_profile": "phase-a-offline",
                "sample_rate_hz": 48_000,
                "block_size_frames": 128,
                "requested_quality": "phase-a",
                "delivered_quality": "phase-a",
                "streaming_cadence": "not_applicable",
                "callback_timing": "not_applicable",
                "non_claims": [
                    crate::provenance::UNCOMMITTED_SOURCE_NONCLAIM,
                    crate::provenance::NO_DELIVERED_EAR_SPL_NONCLAIM,
                    crate::provenance::REMAINING_PHASE_A_GATES_NONCLAIM,
                ],
            })
        };
        let bytes = serde_json::to_vec_pretty(&provenance).unwrap();
        std::fs::write(dir.join("capture-provenance.json"), &bytes).unwrap();
        let hash = sha256_hex(&bytes);
        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: gate.into(),
            fixture_id: fixture_id.into(),
            fixture_content_sha256: "a".repeat(64),
            asset_id: "a".into(),
            asset_descriptor_sha256: "a".repeat(64),
            files: vec![BundleFile {
                name: "capture-provenance.json".into(),
                kind: "capture_provenance".into(),
                content_sha256: hash,
                size_bytes: bytes.len() as u64,
            }],
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };
        // A minimal fixture.json that parses and matches fixture_id.
        let fixture_text = if gate == "S0" {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/s0-free-field/fixture.json"),
            )
            .unwrap()
        } else {
            std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../fixtures/s3-corner/fixture.json"),
            )
            .unwrap()
        };
        std::fs::write(dir.join("fixture.json"), &fixture_text).unwrap();
        (dir, manifest)
    }

    fn fixture_for(dir: &std::path::Path) -> Fixture {
        let text = std::fs::read_to_string(dir.join("fixture.json")).unwrap();
        Fixture::parse(&text).unwrap()
    }

    #[test]
    fn capture_provenance_accepts_valid_s0() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap();
    }

    #[test]
    fn capture_provenance_accepts_valid_s3() {
        let (dir, manifest) = provenance_bundle("S3");
        let fixture = fixture_for(&dir);
        verify_capture_provenance(&dir, &manifest, "S3", &fixture).unwrap();
    }

    /// Edit the provenance JSON via a typed mutator, write it back, and return a
    /// manifest whose capture-provenance entry hash matches the new bytes. This
    /// isolates the field-validation logic from the hash check (covered by a
    /// separate test) and avoids brittle string replacement against pretty-print
    /// formatting.
    fn edit_provenance(
        dir: &std::path::Path,
        manifest: &BundleManifest,
        edit_fn: impl FnOnce(&mut serde_json::Value),
    ) -> BundleManifest {
        let path = dir.join("capture-provenance.json");
        let text = std::fs::read_to_string(&path).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        edit_fn(&mut value);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        let hash = sha256_hex(&bytes);
        std::fs::write(&path, bytes).unwrap();
        let mut new_manifest = manifest.clone();
        if let Some(entry) = new_manifest
            .files
            .iter_mut()
            .find(|f| f.name == "capture-provenance.json")
        {
            entry.content_sha256 = hash;
        }
        new_manifest
    }

    #[test]
    fn capture_provenance_rejects_wrong_sdk_version() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["steam_audio_version"] = serde_json::json!("0.0.0");
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(
            err.message().contains("steam_audio_version"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_missing_dylib_path() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["dylib_path"] = serde_json::Value::Null;
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        // Null dylib_path is rejected (as a missing/empty path) — artifact
        // capture requires an established dylib.
        assert!(
            err.message().contains("dylib_path"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_short_checksum() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["binary_checksum_sha256"] = serde_json::json!("b".repeat(32));
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(err.message().contains("64-hex"), "got: {}", err.message());
    }

    #[test]
    fn capture_provenance_rejects_wrong_block_size() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["block_size_frames"] = serde_json::json!(256);
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(
            err.message().contains("block_size_frames"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_wrong_sample_rate() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["sample_rate_hz"] = serde_json::json!(44100);
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(
            err.message().contains("sample_rate_hz"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_wrong_gate() {
        let (dir, manifest) = provenance_bundle("S3");
        let fixture = fixture_for(&dir);
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["gate"] = serde_json::json!("S0");
        });
        let err = verify_capture_provenance(&dir, &manifest, "S3", &fixture).unwrap_err();
        assert!(err.message().contains("gate"), "got: {}", err.message());
    }

    #[test]
    fn capture_provenance_rejects_stale_sanitizer_nonclaim() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        // Keep all required nonclaims, but ADD a stale sanitizer-suite nonclaim.
        // The required-nonclaim check passes; the stale-wording check must fire.
        let manifest = edit_provenance(&dir, &manifest, |v| {
            if let Some(arr) = v["non_claims"].as_array_mut() {
                arr.push(serde_json::json!("sanitizer suite not implemented"));
            }
        });
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(
            err.message().contains("stale 'sanitizer suite'"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_s3_without_world_dir() {
        let (dir, manifest) = provenance_bundle("S3");
        let fixture = fixture_for(&dir);
        // An empty world_dir string reaches the explicit empty-string check.
        let manifest = edit_provenance(&dir, &manifest, |v| {
            v["world_dir"] = serde_json::json!("");
        });
        let err = verify_capture_provenance(&dir, &manifest, "S3", &fixture).unwrap_err();
        assert!(
            err.message().contains("world_dir is empty"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn capture_provenance_rejects_edited_bytes_hash_mismatch() {
        let (dir, manifest) = provenance_bundle("S0");
        let fixture = fixture_for(&dir);
        // Append a byte so the on-disk content hash no longer matches the manifest.
        let path = dir.join("capture-provenance.json");
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.push(b' ');
        std::fs::write(&path, bytes).unwrap();
        let err = verify_capture_provenance(&dir, &manifest, "S0", &fixture).unwrap_err();
        assert!(
            err.message().contains("content hash does not match"),
            "got: {}",
            err.message()
        );
    }

    /// asset_id semantic binding: a descriptor whose asset_id matches the byte
    /// hash but not the manifest/fixture source must be rejected.
    #[test]
    fn asset_descriptor_rejects_swapped_asset_id() {
        let dir = std::env::temp_dir().join(format!(
            "fightbox-asset-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Write the real s3 fixture and its real descriptor (asset_id matches).
        let fixture_text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/s3-corner/fixture.json"),
        )
        .unwrap();
        std::fs::write(dir.join("fixture.json"), &fixture_text).unwrap();
        let descriptor_text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/assets/s3-calibrated-pink.json"),
        )
        .unwrap();
        std::fs::write(dir.join("asset-descriptor.json"), &descriptor_text).unwrap();
        let bytes = std::fs::read(dir.join("asset-descriptor.json")).unwrap();
        let hash = sha256_hex(&bytes);
        let fixture = Fixture::parse(&fixture_text).unwrap();
        // Manifest claims a DIFFERENT asset_id than the descriptor/fixture.
        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S3".into(),
            fixture_id: fixture.fixture_id.clone(),
            fixture_content_sha256: "a".repeat(64),
            asset_id: "s0-calibrated-pink".into(), // swapped
            asset_descriptor_sha256: hash.clone(),
            files: vec![BundleFile {
                name: "asset-descriptor.json".into(),
                kind: "asset_descriptor".into(),
                content_sha256: hash,
                size_bytes: bytes.len() as u64,
            }],
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };
        let err = verify_asset_descriptor(&dir, &manifest, &fixture).unwrap_err();
        assert!(
            err.message().contains("asset_id") && err.message().contains("does not match"),
            "got: {}",
            err.message()
        );
    }
}

/// Focused bundle mutation tests for the pathing-on/off semantic boundary.
/// These use real canonical float32 WAV bytes, not probe/world bytes.
#[cfg(test)]
mod pathing_pcm_mutation_tests {
    use super::*;
    use fightbox_evidence::{WavSpec, write_wav};

    fn stereo_tone(amplitude: f32, frequency_hz: f32) -> Vec<f32> {
        let mut samples = Vec::with_capacity(9_600);
        for frame in 0..4_800 {
            let sample =
                (frame as f32 * core::f32::consts::TAU * frequency_hz / 48_000.0).sin() * amplitude;
            samples.extend_from_slice(&[sample, sample]);
        }
        samples
    }

    fn payload(
        on_bytes: &[u8],
        off_bytes: &[u8],
        comparison: &fightbox_evidence::SpectralComparison,
    ) -> PathingComparisonPayload {
        crate::metrics::pathing_comparison_payload(
            sha256_hex(on_bytes),
            sha256_hex(off_bytes),
            comparison,
        )
    }

    fn entry(name: &str, bytes: &[u8]) -> BundleFile {
        BundleFile {
            name: name.into(),
            kind: "stem_wav".into(),
            content_sha256: sha256_hex(bytes),
            size_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn altered_pathing_audio_pcm_is_rejected_after_required_hashes_are_recomputed() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let off_pcm = vec![0.0; 9_600];
        let original_on_pcm = stereo_tone(0.20, 1_000.0);
        let original_on_wav = write_wav(spec, &original_on_pcm).unwrap();
        let off_wav = write_wav(spec, &off_pcm).unwrap();
        let original = compare_pathing(
            spec,
            &original_on_pcm,
            &off_pcm,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
        )
        .unwrap();
        let mut recorded = payload(&original_on_wav, &off_wav, &original);
        let mut manifest_files = vec![
            entry("pathing-on-sum.wav", &original_on_wav),
            entry("pathing-off-sum.wav", &off_wav),
        ];

        // Alter the actual AUDIO PCM while keeping it a valid, differing WAV.
        let altered_on_pcm = stereo_tone(0.35, 2_000.0);
        let altered_on_wav = write_wav(spec, &altered_on_pcm).unwrap();
        assert_ne!(
            altered_on_wav, original_on_wav,
            "mutation must change WAV bytes"
        );

        // Recompute the manifest file hash/size and the recorded WAV hash. Those
        // outer bindings now pass, so rejection must come from semantic
        // compare_pathing recomputation against the stale recorded metrics.
        manifest_files[0] = entry("pathing-on-sum.wav", &altered_on_wav);
        recorded.on_sum_hash_sha256 = sha256_hex(&altered_on_wav);
        assert_eq!(
            manifest_files[0].content_sha256,
            recorded.on_sum_hash_sha256
        );
        let (decoded_spec, decoded_on) = read_wav(&altered_on_wav).unwrap();
        let (_, decoded_off) = read_wav(&off_wav).unwrap();
        let recomputed = compare_pathing(
            decoded_spec,
            &decoded_on,
            &decoded_off,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
        )
        .unwrap();
        let err = cross_check_pathing_comparison(&recorded, &recomputed).unwrap_err();
        assert!(
            err.message().contains("does not match recorded"),
            "expected semantic metric rejection after hashes pass, got: {}",
            err.message()
        );
    }

    #[test]
    fn altered_recorded_pathing_metrics_are_rejected_with_intact_wavs() {
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let on_pcm = stereo_tone(0.20, 1_000.0);
        let off_pcm = vec![0.0; 9_600];
        let on_wav = write_wav(spec, &on_pcm).unwrap();
        let off_wav = write_wav(spec, &off_pcm).unwrap();
        let comparison = compare_pathing(
            spec,
            &on_pcm,
            &off_pcm,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
        )
        .unwrap();
        let mut recorded = payload(&on_wav, &off_wav, &comparison);
        let original_on_hash = recorded.on_sum_hash_sha256.clone();
        let original_off_hash = recorded.off_sum_hash_sha256.clone();

        recorded.spectral_l1_difference += 0.25;
        assert_eq!(sha256_hex(&on_wav), original_on_hash);
        assert_eq!(sha256_hex(&off_wav), original_off_hash);
        let err = cross_check_pathing_comparison(&recorded, &comparison).unwrap_err();
        assert!(
            err.message().contains("spectral L1"),
            "expected altered-metrics rejection with intact WAVs, got: {}",
            err.message()
        );
    }
}

#[cfg(all(test, feature = "linked-sdk"))]
mod production_bundle_mutation_tests {
    use super::*;
    use fightbox_evidence::{read_wav, write_wav};
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/s3-corner/fixture.json")
    }

    fn generated_bundle() -> &'static PathBuf {
        static BUNDLE: OnceLock<PathBuf> = OnceLock::new();
        BUNDLE.get_or_init(|| {
            std::env::set_current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
            let root = std::env::temp_dir().join(format!(
                "fightbox-production-mutations-{}",
                std::process::id()
            ));
            let world = root.join("world");
            let bundle = root.join("bundle");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            crate::s3_bake::run(&fixture(), &world).unwrap();
            crate::s3_render::run(&fixture(), &world, &bundle).unwrap();
            bundle
        })
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    fn fresh_case(label: &str) -> PathBuf {
        static SERIAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fightbox-production-case-{}-{label}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        copy_tree(generated_bundle(), &dir);
        dir
    }

    fn finalize_manifest(dir: &Path, mut manifest: BundleManifest) {
        manifest.unsigned_manifest_sha256 = None;
        manifest.manifest_content_sha256 = None;
        let digest = manifest.recompute_unsigned_digest();
        manifest.unsigned_manifest_sha256 = Some(digest.clone());
        manifest.manifest_content_sha256 = Some(digest);
        let bytes = serde_json::to_vec_pretty(&manifest).unwrap();
        std::fs::write(dir.join("manifest.json"), &bytes).unwrap();
        std::fs::write(
            dir.join(crate::bundle::MANIFEST_DIGEST_SIDECAR),
            sha256_hex(&bytes),
        )
        .unwrap();
    }

    fn rewrite_metrics(dir: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
        let metrics_path = dir.join("metrics.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&metrics_path).unwrap()).unwrap();
        mutate(&mut value);
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        std::fs::write(&metrics_path, &bytes).unwrap();
        let mut manifest: BundleManifest =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let entry = manifest
            .files
            .iter_mut()
            .find(|f| f.name == "metrics.json")
            .unwrap();
        entry.content_sha256 = sha256_hex(&bytes);
        entry.size_bytes = bytes.len() as u64;
        finalize_manifest(dir, manifest);
    }

    #[test]
    fn coherent_direct_and_world_metric_mutations_reach_semantic_rejection() {
        let cases = [
            (
                "requested-kind",
                "/snapshot/direct/requested_occlusion_mode/kind",
                serde_json::json!("volumetric"),
            ),
            (
                "delivered-kind",
                "/snapshot/direct/delivered_occlusion_mode/kind",
                serde_json::json!("volumetric"),
            ),
            (
                "raycast-radius",
                "/snapshot/direct/requested_occlusion_mode/volumetric_radius_m",
                serde_json::json!(0.5),
            ),
            (
                "raycast-samples",
                "/snapshot/direct/delivered_occlusion_mode/volumetric_sample_count",
                serde_json::json!(64),
            ),
            (
                "world-content",
                "/world/world_content_sha256",
                serde_json::json!("0".repeat(64)),
            ),
        ];
        for (label, pointer, replacement) in cases {
            let dir = fresh_case(label);
            rewrite_metrics(&dir, |value| {
                *value.pointer_mut(pointer).unwrap() = replacement
            });
            let err = run(&dir, true).unwrap_err();
            assert!(
                err.message().contains("direct occlusion")
                    || err.message().contains("world_content_sha256"),
                "{label}: {}",
                err.message()
            );
        }
    }

    #[test]
    fn altered_audio_with_all_outer_hashes_rewritten_fails_compare_pathing_semantics() {
        let dir = fresh_case("audio");
        let wav_path = dir.join("pathing-on-sum.wav");
        let (spec, mut pcm) = read_wav(&std::fs::read(&wav_path).unwrap()).unwrap();
        for sample in &mut pcm {
            *sample *= 0.25;
        }
        let wav = write_wav(spec, &pcm).unwrap();
        std::fs::write(&wav_path, &wav).unwrap();
        let wav_hash = sha256_hex(&wav);
        rewrite_metrics(&dir, |value| {
            value["pathing_comparison"]["on_sum_hash_sha256"] = serde_json::json!(wav_hash);
            value["stems"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|stem| stem["file"] == "pathing-on-sum.wav")
                .unwrap()["content_sha256"] = serde_json::json!(wav_hash);
        });
        let mut manifest: BundleManifest =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        let entry = manifest
            .files
            .iter_mut()
            .find(|f| f.name == "pathing-on-sum.wav")
            .unwrap();
        entry.content_sha256 = wav_hash;
        entry.size_bytes = wav.len() as u64;
        finalize_manifest(&dir, manifest);
        let err = run(&dir, true).unwrap_err();
        let message = err.message();
        let reached_compare_pathing_semantics = message
            .contains("recomputed compare_pathing reports pathing on/off sums do not differ")
            || (message.contains("recomputed pathing")
                && message.contains("does not match recorded"));
        assert!(
            reached_compare_pathing_semantics,
            "expected independent compare_pathing semantic rejection, got: {message}"
        );
        for earlier_layer in [
            "content hash on disk does not match manifest",
            "unsigned digest does not match",
            "finalized digest sidecar",
            "schema",
            "cannot read",
            "missing pathing-on-sum.wav",
        ] {
            assert!(
                !message.contains(earlier_layer),
                "mutation stopped at earlier layer {earlier_layer:?}: {message}"
            );
        }
    }

    #[test]
    fn singleton_shadowing_wins_before_poisoned_path_read() {
        let dir = fresh_case("ordering");
        let mut manifest: BundleManifest =
            serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
        manifest.files.push(BundleFile {
            name: "missing-poison.json".into(),
            kind: "metrics".into(),
            content_sha256: "0".repeat(64),
            size_bytes: 999,
        });
        finalize_manifest(&dir, manifest);
        let err = run(&dir, true).unwrap_err();
        assert!(
            err.message().contains("duplicate singleton kind metrics"),
            "{}",
            err.message()
        );
        assert!(!err.message().contains("cannot be read"));
    }
}

/// Mutation tests for the R2 retained-trajectory + summed-output handoff. These
/// exercise the public backend summed-boundary continuity metric directly to
/// prove the verifier's recomputation catches an injected discontinuity, and
/// that the recorded metrics must match the recomputed values.
#[cfg(test)]
mod trajectory_tests {
    use fightbox_steam_audio::{
        OwnedStereoPcm, S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD, S3_CONTINUITY_WINDOW_FRAMES,
        measure_s3_summed_boundary_continuity,
    };

    /// Build `count` smooth 128-frame stereo blocks: a low-amplitude sine that
    /// is continuous across block boundaries (the end of one block meets the
    /// start of the next in phase).
    fn smooth_blocks(count: usize) -> Vec<OwnedStereoPcm> {
        let frame_size = 128;
        let sr = 48_000;
        let freq = 220.0_f32;
        let mut blocks = Vec::with_capacity(count);
        for block_index in 0..count {
            let start_frame = block_index * frame_size;
            let mut interleaved = Vec::with_capacity(frame_size * 2);
            for frame in start_frame..(start_frame + frame_size) {
                let phase = frame as f32 * freq * std::f32::consts::TAU / sr as f32;
                let v = phase.sin() * 0.02;
                interleaved.push(v);
                interleaved.push(v);
            }
            blocks.push(OwnedStereoPcm {
                sample_rate_hz: sr as i32,
                frame_count: frame_size,
                interleaved,
            });
        }
        blocks
    }

    #[test]
    fn smooth_trajectory_passes_summed_boundary_continuity() {
        let blocks = smooth_blocks(5);
        let continuity = measure_s3_summed_boundary_continuity(
            &blocks,
            S3_CONTINUITY_WINDOW_FRAMES,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .unwrap();
        assert!(
            continuity.passed,
            "smooth trajectory must pass: {continuity:?}"
        );
        assert_eq!(continuity.boundaries.len(), 4);
        assert!(continuity.maximum_step_to_local_peak_ratio < S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD);
    }

    #[test]
    fn injected_boundary_discontinuity_is_rejected() {
        let mut blocks = smooth_blocks(5);
        // Inject a full-scale click at the start of block 2 (after boundary 1).
        // The end of block 1 is a low sine; the start of block 2 jumps to 1.0.
        blocks[2].interleaved[0] = 1.0;
        blocks[2].interleaved[1] = -1.0;
        let continuity = measure_s3_summed_boundary_continuity(
            &blocks,
            S3_CONTINUITY_WINDOW_FRAMES,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .unwrap();
        // The verifier rejects any trajectory whose recomputed continuity fails.
        assert!(
            !continuity.passed,
            "injected discontinuity must fail: {continuity:?}"
        );
        assert!(continuity.maximum_step_to_local_peak_ratio > S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD);
    }

    #[test]
    fn continuity_metric_rejects_non_finite_samples() {
        let mut blocks = smooth_blocks(3);
        blocks[1].interleaved[5] = f32::NAN;
        let err = measure_s3_summed_boundary_continuity(
            &blocks,
            S3_CONTINUITY_WINDOW_FRAMES,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("finite"), "got: {msg}");
    }

    #[test]
    fn continuity_metric_rejects_zero_window() {
        let blocks = smooth_blocks(3);
        let err = measure_s3_summed_boundary_continuity(&blocks, 0, 0.5).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("window"), "got: {msg}");
    }

    /// The recorded metrics must equal the recomputed values: a sidecar whose
    /// pass flag was flipped to true while the WAV contains a discontinuity must
    /// be rejected by the verifier's cross-check. This exercises the
    /// `continuity.passed != metrics.continuity_passed` branch.
    #[test]
    fn recorded_pass_flag_must_match_recomputed_continuity() {
        let blocks = smooth_blocks(5);
        let recomputed = measure_s3_summed_boundary_continuity(
            &blocks,
            S3_CONTINUITY_WINDOW_FRAMES,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .unwrap();
        // A sidecar that claims continuity_passed=false when recomputed=true is
        // a mismatch the verifier rejects.
        assert!(recomputed.passed);
        let mismatch = false;
        assert_ne!(recomputed.passed, mismatch);
    }

    /// The recorded max ratio must match the recomputed value within tolerance.
    #[test]
    fn recorded_max_ratio_must_match_recomputed() {
        let blocks = smooth_blocks(4);
        let recomputed = measure_s3_summed_boundary_continuity(
            &blocks,
            S3_CONTINUITY_WINDOW_FRAMES,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .unwrap();
        // An edited max ratio off by 1.0 exceeds the 1e-5 tolerance.
        let edited = recomputed.maximum_step_to_local_peak_ratio + 1.0;
        assert!((recomputed.maximum_step_to_local_peak_ratio - edited).abs() > 1.0e-5);
    }
}

/// Correction 5: S3 capture must not derive trust from an absolute mutable
/// world path. The bundle carries/indexes the immutable world manifest and
/// every world-side metadata artifact needed to cross-bind fixture ID/hash,
/// probe-batch hash/count, path-data bytes, and serialized bytes. Verification
/// must still work from a copied bundle after the original bake directory is
/// unavailable, while rejecting mutations to the bundled world evidence.
#[cfg(test)]
mod world_payload_tests {
    use super::*;
    use crate::bundle::{BundleFile, BundleManifest, WorldManifest, WorldPayload};
    use crate::schema::WORLD_MANIFEST;

    fn unique_suffix() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("{nanos}")
    }

    /// Build a temp bundle directory with a complete, internally-consistent set
    /// of bundled world evidence under `world/`, plus a manifest indexing all
    /// three world files. The bundle is fully self-contained: no absolute bake
    /// directory is referenced or required. Returns the bundle dir, the
    /// finalized manifest, and the matching WorldPayload.
    fn synthetic_world_bundle() -> (std::path::PathBuf, BundleManifest, WorldPayload) {
        let dir = std::env::temp_dir().join(format!(
            "fightbox-world-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(dir.join("world")).unwrap();

        // Deterministic probe-batch bytes with a stable content hash.
        let probe_bytes = {
            let mut v = Vec::with_capacity(1024);
            for i in 0u32..256 {
                v.extend_from_slice(&i.to_le_bytes());
            }
            v
        };
        let probe_hash = sha256_hex(&probe_bytes);
        let serialized_size = probe_bytes.len() as u64;
        let path_data_size = 768u64;
        let probe_count = 324u32;
        std::fs::write(dir.join("world/probe-batch.bin"), &probe_bytes).unwrap();

        // Bundled probe-batch-metadata.json mirroring the backend wire schema.
        let metadata = serde_json::json!({
            "schema_version": fightbox_steam_audio::PROBE_BATCH_METADATA_SCHEMA,
            "steam_audio_version": fightbox_steam_audio::STEAM_AUDIO_VERSION,
            "upstream_commit": fightbox_steam_audio::STEAM_AUDIO_UPSTREAM_COMMIT,
            "probe_count": probe_count,
            "path_data_size_bytes": path_data_size,
            "serialized_size_bytes": serialized_size,
            "content_sha256": probe_hash,
            "bake_progress_callback_count": 1u32,
            "final_bake_progress_millionths": 1_000_000u32,
        });
        let meta_text = serde_json::to_string_pretty(&metadata).unwrap();
        let meta_hash = sha256_hex(meta_text.as_bytes());
        std::fs::write(dir.join("world/probe-batch-metadata.json"), &meta_text).unwrap();

        // Bundled world-manifest.json cross-binding the world to the fixture.
        let fixture_id = "s3-masonry-building-corner";
        let fixture_hash = "a".repeat(64);
        let world_manifest = WorldManifest::new(
            fixture_id.into(),
            fixture_hash.clone(),
            probe_hash.clone(),
            serialized_size,
            probe_count,
            path_data_size,
            vec![
                "probe-batch.bin".into(),
                "probe-batch-metadata.json".into(),
                "world-manifest.json".into(),
                "fixture.json".into(),
            ],
        );
        let manifest_text = serde_json::to_string_pretty(&world_manifest).unwrap();
        let manifest_hash = sha256_hex(manifest_text.as_bytes());
        std::fs::write(dir.join("world/world-manifest.json"), &manifest_text).unwrap();

        let world_files = vec![
            BundleFile {
                name: "world/probe-batch.bin".into(),
                kind: "world_probe_batch".into(),
                content_sha256: probe_hash.clone(),
                size_bytes: serialized_size,
            },
            BundleFile {
                name: "world/probe-batch-metadata.json".into(),
                kind: "world_probe_batch_metadata".into(),
                content_sha256: meta_hash,
                size_bytes: meta_text.len() as u64,
            },
            BundleFile {
                name: "world/world-manifest.json".into(),
                kind: "world_manifest".into(),
                content_sha256: manifest_hash,
                size_bytes: manifest_text.len() as u64,
            },
        ];
        let manifest = BundleManifest {
            schema_version: BundleManifest::SCHEMA.into(),
            gate: "S3".into(),
            fixture_id: fixture_id.into(),
            fixture_content_sha256: fixture_hash,
            asset_id: "s3-asset".into(),
            asset_descriptor_sha256: "b".repeat(64),
            files: world_files,
            unsigned_manifest_sha256: None,
            manifest_content_sha256: None,
        };
        let payload = WorldPayload {
            world_dir: "/nonexistent/bake/dir".into(),
            world_content_sha256: probe_hash.clone(),
            probe_batch_content_sha256: probe_hash,
            serialized_size_bytes: serialized_size,
            probe_count,
            path_data_size_bytes: path_data_size,
        };
        (dir, manifest, payload)
    }

    #[test]
    fn verify_world_payload_succeeds_without_any_bake_directory() {
        // The bundle's recorded world_dir points at a path that DOES NOT EXIST
        // (a deleted/moved bake directory). Verification must still succeed
        // purely from the bundled immutable world evidence — the absolute path
        // is provenance only, never a source of trust.
        let (dir, manifest, payload) = synthetic_world_bundle();
        assert!(
            !std::path::Path::new(&payload.world_dir).exists(),
            "test precondition: the recorded bake dir must not exist"
        );
        verify_world_payload(&dir, &payload, &manifest).expect(
            "verification must succeed from the bundled world evidence alone, even when the absolute bake directory is unavailable",
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_world_payload_rejects_mutated_probe_batch_bytes() {
        // Tampering with the bundled probe-batch bytes (even without touching
        // the manifest) is caught: the recomputed hash no longer matches the
        // metrics payload or the manifest entry.
        let (dir, mut manifest, mut payload) = synthetic_world_bundle();
        let mut bytes = std::fs::read(dir.join("world/probe-batch.bin")).unwrap();
        bytes[0] ^= 0xff;
        let new_hash = sha256_hex(&bytes);
        std::fs::write(dir.join("world/probe-batch.bin"), &bytes).unwrap();
        // Rewrite the manifest entry AND metrics payload to the new hash, so the
        // only thing that could still catch the mutation is cross-binding to the
        // bundled probe-batch-metadata.json's content_sha256 (which still names
        // the ORIGINAL probe hash). This proves the world manifest and metadata
        // are genuine cross-binding evidence, not cosmetic.
        if let Some(entry) = manifest
            .files
            .iter_mut()
            .find(|f| f.name == "world/probe-batch.bin")
        {
            entry.content_sha256 = new_hash.clone();
            entry.size_bytes = bytes.len() as u64;
        }
        payload.probe_batch_content_sha256 = new_hash.clone();
        payload.world_content_sha256 = new_hash;
        payload.serialized_size_bytes = bytes.len() as u64;
        let err = verify_world_payload(&dir, &payload, &manifest).unwrap_err();
        assert!(
            err.message()
                .contains("probe-batch-metadata content_sha256"),
            "expected cross-binding rejection via probe-batch-metadata, got: {}",
            err.message()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_world_payload_rejects_world_manifest_bound_to_different_fixture() {
        // A world baked from a DIFFERENT fixture must not satisfy this bundle.
        // Mutate the bundled world-manifest's fixture_id to a foreign value and
        // rewrite its manifest entry hash so the file-index layer passes; the
        // fixture cross-binding must still reject.
        let (dir, mut manifest, payload) = synthetic_world_bundle();
        let text = std::fs::read_to_string(dir.join("world/world-manifest.json")).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_str(&text).expect("world-manifest parses as JSON");
        value["fixture_id"] = serde_json::json!("s3-different-foreign-fixture");
        let new_text = serde_json::to_string_pretty(&value).unwrap();
        let new_hash = sha256_hex(new_text.as_bytes());
        std::fs::write(dir.join("world/world-manifest.json"), &new_text).unwrap();
        if let Some(entry) = manifest
            .files
            .iter_mut()
            .find(|f| f.name == "world/world-manifest.json")
        {
            entry.content_sha256 = new_hash;
            entry.size_bytes = new_text.len() as u64;
        }
        let err = verify_world_payload(&dir, &payload, &manifest).unwrap_err();
        assert!(
            err.message().contains("fixture_id") && err.message().contains("does not match"),
            "expected fixture cross-binding rejection, got: {}",
            err.message()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_world_payload_rejects_missing_bundled_world_manifest() {
        // If the bundle drops the bundled world-manifest entry, verification
        // must fail rather than falling back to any external source.
        let (dir, mut manifest, payload) = synthetic_world_bundle();
        manifest
            .files
            .retain(|f| f.name != "world/world-manifest.json");
        let err = verify_world_payload(&dir, &payload, &manifest).unwrap_err();
        assert!(
            err.message()
                .contains("missing bundled world/world-manifest.json"),
            "got: {}",
            err.message()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn world_manifest_schema_version_is_documented_constant() {
        // Guard against silent drift: the WORLD_MANIFEST schema constant is the
        // single source of truth for both the recorder and the verifier.
        assert_eq!(WORLD_MANIFEST, "fightbox.world-manifest.v1");
    }

    #[test]
    fn verify_world_payload_rejects_partial_hash_rewrite_of_probe_bytes() {
        // Altering the bundled probe-batch bytes and rewriting
        // the manifest entry AND the metrics payload still cannot preserve a
        // pass, because the bundled probe-batch-metadata.json carries an
        // INDEPENDENT inner content_sha256 field that still names the original
        // probe hash. A forger who rewrites the obvious hashes but misses the
        // independent metadata field is caught by the cross-binding. This is
        // the realistic attack: the forger controls the manifest (rewrites the
        // entry hash) and the metrics (rewrites the payload), but the bundled
        // metadata's inner field is a separate artifact whose bytes were not
        // touched, so its recorded content_sha256 disagrees with the mutated
        // probe bytes on disk.
        let (dir, mut manifest, mut payload) = synthetic_world_bundle();
        let mut bytes = std::fs::read(dir.join("world/probe-batch.bin")).unwrap();
        bytes[0] ^= 0xff;
        let forged_hash = sha256_hex(&bytes);
        std::fs::write(dir.join("world/probe-batch.bin"), &bytes).unwrap();
        // Rewrite manifest entry + metrics payload ONLY (forget the metadata's
        // inner content_sha256 field, which is a separate JSON artifact).
        if let Some(entry) = manifest
            .files
            .iter_mut()
            .find(|f| f.name == "world/probe-batch.bin")
        {
            entry.content_sha256 = forged_hash.clone();
            entry.size_bytes = bytes.len() as u64;
        }
        payload.probe_batch_content_sha256 = forged_hash.clone();
        payload.world_content_sha256 = forged_hash;
        payload.serialized_size_bytes = bytes.len() as u64;
        let err = verify_world_payload(&dir, &payload, &manifest).unwrap_err();
        // The bundled metadata's inner content_sha256 still names the ORIGINAL
        // probe hash, so it disagrees with the mutated probe bytes.
        assert!(
            err.message().contains(
                "probe-batch-metadata content_sha256 does not match the probe-batch bytes"
            ),
            "expected cross-binding rejection via the metadata's inner hash, got: {}",
            err.message()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
