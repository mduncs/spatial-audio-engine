//! `fightbox phase-a s3-render` — reload a baked world, run the real S3
//! simulator on the initial occluded pose, and write the five stems and
//! evidence sidecars.
//!
//! This is the second of two process invocations. It re-reads the world
//! directory written by `s3-bake`, validates every artifact before handing it
//! back to Steam Audio, reconstructs the baked probe batch from on-disk
//! metadata, runs direct/reflections/pathing once each, and writes the bundle:
//!   `direct.wav`, `reflections.wav`, `path.wav`, `pathing-on-sum.wav`,
//!   `pathing-off-sum.wav`, plus `metrics.json`, `manifest.json`,
//!   `capture-provenance.json`, `listening-record.json`, `fixture.json`, and
//!   `asset-descriptor.json`.
//!
//! The pathing-on and pathing-off sums must hash differently; the rendered
//! azimuth must match the analytic corner geometry; every validation segment
//! must be unoccluded.

use std::path::{Path, PathBuf};

use fightbox_evidence::{
    EquipmentRecord, HrtfRecord, ListenerIdentity, ListeningRecord, ListeningResult, SignOff,
    sha256_hex,
};
use fightbox_steam_audio::{
    BackendError, BakedProbeBatch, OwnedStereoPcm, PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata,
    S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD, S3_CONTINUITY_WINDOW_FRAMES, S3RenderOutput,
    S3RenderRequest, S3TrajectoryRenderOutput, S3TrajectoryRenderRequest,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, render_s3, render_s3_trajectory,
};

use crate::asset::{AssetDescriptor, ResolvedAsset};
use crate::atomicio::{
    self, AtomicDir, WrittenWav, validate_output_path, write_json_atomic, write_json_string_atomic,
};
use crate::bundle::{
    AnalyticPayload, BoundaryMeasurementPayload, BundleFile, BundleManifest, DirectSnapshotPayload,
    PathSnapshotPayload, PathingComparisonPayload, ReflectionSnapshotPayload,
    RetainedSessionStatsPayload, S3Metrics, S3SnapshotPayload, S3TrajectoryMetrics,
    TrajectoryBlockPayload, WorldPayload,
};
use crate::error::{CliError, Result};
use crate::fixture::{Fixture, Gate};
use crate::metrics::{
    calibration_payload, direct_snapshot_payload, path_snapshot_payload,
    pathing_comparison_payload, reflection_snapshot_payload, stem_hash_payload,
};
use crate::provenance::{
    self, ENGINE_IDENTITY, NO_DELIVERED_EAR_SPL_NONCLAIM, REMAINING_PHASE_A_GATES_NONCLAIM,
    SdkBinary, UNCOMMITTED_SOURCE_NONCLAIM,
};
use crate::scene::FixtureScene;

/// Run the S3 render. `out` is the bundle directory to create atomically;
/// `world` is the directory written by `s3-bake`.
pub fn run(fixture_path: &Path, world: &Path, out: &Path) -> Result<()> {
    require_linked()?;
    let fixture = load_fixture(fixture_path)?;
    let asset = load_asset(&fixture)?;
    let out = validate_output_path(out)?;
    let sdk = SdkBinary::detect();
    // An artifact-producing capture is invalid unless the actual dylib path and
    // checksum were established from STEAM_AUDIO_SDK_DIR at runtime.
    sdk.require_established()?;
    let world_canonical = validate_world_dir(world)?;

    // Reload the world directory artifacts and reconstruct the baked probe batch
    // from on-disk metadata only. No in-memory bake state is reused.
    let probe_batch = reload_probe_batch(&world_canonical)?;
    let world_manifest = reload_world_manifest(&world_canonical)?;
    // Cross-validate: the world manifest's recorded hashes/counts must match the
    // reloaded probe-batch bytes and metadata.
    verify_world_manifest_matches_batch(&world_manifest, &probe_batch)?;

    let scene = FixtureScene::build(fixture.clone(), &asset)?;
    let request = scene.s3_render_request().map_err(CliError::new)?;

    let started = std::time::Instant::now();
    let output = render_s3_with_baked(&request, &probe_batch).map_err(map_backend_error)?;
    let render_duration_s = started.elapsed().as_secs_f32() as f32;

    // Every authority-note §ν property is evidenced explicitly from the actual
    // backend output before any artifact is written.
    assert_loaded_sizes_match_metadata(&output, &probe_batch)?;
    assert_direct_occlusion_samples(&output, &request)?;
    assert_path_snapshot_finite_and_first_order(&output)?;
    assert_path_validation_unoccluded(&output)?;
    assert_reflection_ir_size_positive(&output)?;
    let analytic = assert_azimuth_within_tolerance(&output, &fixture)?;

    let pathing = assert_pathing_sums_differ(&output)?;

    // Retained-session trajectory: the fixture's exact ordered listener
    // trajectory rendered through ONE retained context/scene/probe/simulator/
    // source/HRTF/effect graph. This is the summed-output handoff evidence —
    // not a per-pose independent render and not continuity faked from a single
    // diagnostic stem.
    let trajectory_request = scene
        .s3_trajectory_render_request()
        .map_err(CliError::new)?;
    let trajectory_output = render_s3_trajectory_with_baked(&trajectory_request, &probe_batch)
        .map_err(map_backend_error)?;
    let trajectory_metrics =
        build_trajectory_metrics(&fixture.fixture_id, &trajectory_output, &trajectory_request)?;

    // Write the bundle atomically.
    let dir = AtomicDir::create(out.clone())?;
    let temp = dir.temp_path().to_path_buf();

    // Five stems. The hash of each is taken over the deterministic WAV bytes.
    let direct_wav = write_stem(&temp, "direct.wav", &output.stems.direct)?;
    let reflections_wav = write_stem(&temp, "reflections.wav", &output.stems.reflections)?;
    let path_wav = write_stem(&temp, "path.wav", &output.stems.path)?;
    let pathing_on_wav = write_stem(&temp, "pathing-on-sum.wav", &output.stems.pathing_on_sum)?;
    let pathing_off_wav = write_stem(&temp, "pathing-off-sum.wav", &output.stems.pathing_off_sum)?;
    // The retained trajectory's summed output — the handoff evidence.
    let trajectory_wav = write_stem(&temp, "trajectory-sum.wav", &trajectory_output.summed)?;
    write_json_atomic(&temp.join("trajectory-metrics.json"), &trajectory_metrics)?;

    // Record exact input bytes and asset descriptor.
    let fixture_bytes = std::fs::read(fixture_path).map_err(|e| {
        CliError::new(format!(
            "cannot read fixture {}: {e}",
            fixture_path.display()
        ))
    })?;
    let fixture_hash = sha256_hex(&fixture_bytes);
    atomicio::write_bytes_plain(&temp.join("fixture.json"), &fixture_bytes)?;
    let asset_text = serde_json::to_string_pretty(&asset.descriptor)
        .map_err(|e| CliError::new(format!("asset serialize: {e}")))?;
    let asset_hash = sha256_hex(asset_text.as_bytes());
    atomicio::write_bytes_plain(&temp.join("asset-descriptor.json"), asset_text.as_bytes())?;

    // Record the world directory as referenced by this bundle AND copy the
    // immutable world-side artifacts into the bundle under `world/`. The bundle
    // must not derive trust from an absolute mutable world path: the bake
    // directory can be deleted or moved after capture. The bundle therefore
    // carries and indexes the immutable world manifest and every world-side
    // metadata artifact needed to cross-bind fixture ID/hash, probe-batch hash/
    // count, path-data bytes, and serialized bytes, so verification still works
    // from a copied bundle even when the original bake dir is unavailable.
    let probe_batch_hash = probe_batch.metadata.content_sha256.clone();
    let world_bytes = std::fs::read(world_canonical.join("probe-batch.bin"))
        .map_err(|e| CliError::new(format!("cannot read probe-batch.bin: {e}")))?;
    let world_content_sha256 = sha256_hex(&world_bytes);
    let world_meta_bytes = std::fs::read(world_canonical.join("probe-batch-metadata.json"))
        .map_err(|e| CliError::new(format!("cannot read probe-batch-metadata.json: {e}")))?;
    let world_manifest_bytes = std::fs::read(world_canonical.join("world-manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read world-manifest.json: {e}")))?;
    std::fs::create_dir_all(temp.join("world"))
        .map_err(|e| CliError::new(format!("cannot create bundle world dir: {e}")))?;
    atomicio::write_bytes_plain(&temp.join("world/probe-batch.bin"), &world_bytes)?;
    atomicio::write_bytes_plain(
        &temp.join("world/probe-batch-metadata.json"),
        &world_meta_bytes,
    )?;
    atomicio::write_bytes_plain(
        &temp.join("world/world-manifest.json"),
        &world_manifest_bytes,
    )?;

    let cal_payload = calibration_payload(
        fightbox_api::SceneCalibration::DEFAULT_REFERENCE_SPL_DB,
        fightbox_api::SceneCalibration::DEFAULT_REFERENCE_PCM_RMS_DBFS,
        fightbox_api::SceneCalibration::REFERENCE_DISTANCE_M,
        scene.calibrated.program_rms_dbfs,
        scene.calibrated.drive.target_source_rms_dbfs(),
        scene.calibrated.drive.gain_db(),
        scene.calibrated.drive.linear_gain(),
    );

    let snapshot_payload = build_snapshot_payload(&output);
    let s3_claims_v = s3_claims(&pathing);
    let metrics = S3Metrics {
        schema_version: S3Metrics::SCHEMA.into(),
        fixture_id: fixture.fixture_id.clone(),
        sample_rate_hz: scene.audio.sample_rate_hz as u32,
        frame_count: output.stems.direct.frame_count,
        calibration: cal_payload,
        world: WorldPayload {
            world_dir: world_canonical.to_string_lossy().into_owned(),
            world_content_sha256: world_content_sha256.clone(),
            probe_batch_content_sha256: probe_batch_hash.clone(),
            serialized_size_bytes: probe_batch.metadata.serialized_size_bytes,
            probe_count: probe_batch.metadata.probe_count,
            path_data_size_bytes: probe_batch.metadata.path_data_size_bytes,
        },
        snapshot: snapshot_payload,
        pathing_comparison: pathing,
        analytic,
        stems: vec![
            stem_hash_payload(
                "direct",
                "direct.wav",
                direct_wav.content_sha256.clone(),
                direct_wav.frame_count,
            ),
            stem_hash_payload(
                "reflections",
                "reflections.wav",
                reflections_wav.content_sha256.clone(),
                reflections_wav.frame_count,
            ),
            stem_hash_payload(
                "path",
                "path.wav",
                path_wav.content_sha256.clone(),
                path_wav.frame_count,
            ),
            stem_hash_payload(
                "pathing_on_sum",
                "pathing-on-sum.wav",
                pathing_on_wav.content_sha256.clone(),
                pathing_on_wav.frame_count,
            ),
            stem_hash_payload(
                "pathing_off_sum",
                "pathing-off-sum.wav",
                pathing_off_wav.content_sha256.clone(),
                pathing_off_wav.frame_count,
            ),
        ],
        claims: s3_claims_v,
        non_claims: s3_non_claims(),
    };
    write_json_atomic(&temp.join("metrics.json"), &metrics)?;

    // Manifest with file hashes/sizes, then rehash and rewrite to record its own hash.
    let mut files = vec![
        stem_bundle_file(&temp, "direct.wav", "stem_wav", &direct_wav)?,
        stem_bundle_file(&temp, "reflections.wav", "stem_wav", &reflections_wav)?,
        stem_bundle_file(&temp, "path.wav", "stem_wav", &path_wav)?,
        stem_bundle_file(&temp, "pathing-on-sum.wav", "stem_wav", &pathing_on_wav)?,
        stem_bundle_file(&temp, "pathing-off-sum.wav", "stem_wav", &pathing_off_wav)?,
        stem_bundle_file(
            &temp,
            "trajectory-sum.wav",
            "trajectory_sum_wav",
            &trajectory_wav,
        )?,
        text_bundle_file(&temp, "metrics.json", "metrics")?,
        text_bundle_file(&temp, "trajectory-metrics.json", "trajectory_metrics")?,
        BundleFile {
            name: "fixture.json".into(),
            kind: "fixture".into(),
            content_sha256: fixture_hash.clone(),
            size_bytes: file_size(&temp.join("fixture.json"))?,
        },
        BundleFile {
            name: "asset-descriptor.json".into(),
            kind: "asset_descriptor".into(),
            content_sha256: asset_hash.clone(),
            size_bytes: file_size(&temp.join("asset-descriptor.json"))?,
        },
    ];
    // capture-provenance.json is an IMMUTABLE bundle input: write it before the
    // manifest and index it so the manifest binds the SDK version, dylib
    // checksum, and host facts. listening-record.json stays mutable for the
    // human sign-off and is intentionally NOT indexed here.
    let provenance_json = build_provenance_json(
        &sdk,
        &fixture.fixture_id,
        "S3-render",
        &world_canonical,
        render_duration_s,
    );
    write_json_string_atomic(&temp.join("capture-provenance.json"), &provenance_json)?;
    files.push(text_bundle_file(
        &temp,
        "capture-provenance.json",
        "capture_provenance",
    )?);
    // Index the immutable world artifacts copied into the bundle under `world/`.
    // These are the cross-binding evidence the verifier uses instead of trusting
    // an absolute mutable world path: probe-batch bytes, probe-batch metadata,
    // and the immutable world manifest. The names use a relative `world/` prefix
    // (a single ParentDir-free path component nesting) so the verifier can join
    // them to the bundle root without escaping it.
    files.push(BundleFile {
        name: "world/probe-batch.bin".into(),
        kind: "world_probe_batch".into(),
        content_sha256: world_content_sha256.clone(),
        size_bytes: world_bytes.len() as u64,
    });
    let world_meta_hash = sha256_hex(&world_meta_bytes);
    files.push(BundleFile {
        name: "world/probe-batch-metadata.json".into(),
        kind: "world_probe_batch_metadata".into(),
        content_sha256: world_meta_hash,
        size_bytes: world_meta_bytes.len() as u64,
    });
    let world_manifest_hash = sha256_hex(&world_manifest_bytes);
    files.push(BundleFile {
        name: "world/world-manifest.json".into(),
        kind: "world_manifest".into(),
        content_sha256: world_manifest_hash,
        size_bytes: world_manifest_bytes.len() as u64,
    });

    let mut manifest = BundleManifest {
        schema_version: BundleManifest::SCHEMA.into(),
        gate: "S3".into(),
        fixture_id: fixture.fixture_id.clone(),
        fixture_content_sha256: fixture_hash.clone(),
        asset_id: asset.descriptor.asset_id.clone(),
        asset_descriptor_sha256: asset_hash.clone(),
        files,
        unsigned_manifest_sha256: None,
        manifest_content_sha256: None,
    };
    // Canonical unsigned digest first (the listening/provenance binding key).
    // Write the manifest exactly once with the unsigned digest populated, then
    // record the detached final-file digest in a separate sidecar so the
    // on-disk manifest bytes are stable (a JSON object cannot contain the
    // SHA-256 of its own final bytes).
    let unsigned_digest = manifest.recompute_unsigned_digest();
    manifest.unsigned_manifest_sha256 = Some(unsigned_digest.clone());
    manifest.manifest_content_sha256 = Some(unsigned_digest.clone());
    write_json_atomic(&temp.join("manifest.json"), &manifest)?;
    let manifest_bytes = std::fs::read(temp.join("manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read manifest for hashing: {e}")))?;
    let finalized_digest = sha256_hex(&manifest_bytes);
    atomicio::write_bytes_plain(&temp.join("manifest.sha256"), finalized_digest.as_bytes())?;
    let manifest_hash = unsigned_digest.clone();

    // Provisional listening record (always undecided; never a pass by itself).
    let listening = build_listening_record(&fixture.fixture_id, &fixture_hash, &manifest_hash);
    write_json_string_atomic(&temp.join("listening-record.json"), &listening.to_json())?;

    dir.commit()?;
    eprintln!(
        "fightbox: S3 bundle written to {} (manifest sha256: {manifest_hash})",
        out.display()
    );
    Ok(())
}

fn require_linked() -> Result<()> {
    if !fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        return Err(CliError::new(
            "s3-render requires a linked Steam Audio SDK; rebuild with --features linked-sdk and STEAM_AUDIO_SDK_DIR set",
        ));
    }
    Ok(())
}

fn load_fixture(path: &Path) -> Result<Fixture> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::new(format!("cannot read fixture {}: {e}", path.display())))?;
    let fixture = Fixture::parse(&text).map_err(CliError::new)?;
    if fixture.gate() != Ok(Gate::S3) {
        return Err(CliError::new(format!(
            "expected an S3 fixture, got gate {}",
            fixture.gate
        )));
    }
    Ok(fixture)
}

fn load_asset(fixture: &Fixture) -> Result<ResolvedAsset> {
    let asset_path =
        crate::s0::resolve_asset_path(fixture, std::env::current_dir().ok().as_deref())?;
    let text = std::fs::read_to_string(&asset_path)
        .map_err(|e| CliError::new(format!("cannot read asset {}: {e}", asset_path.display())))?;
    let descriptor = AssetDescriptor::parse(&text).map_err(CliError::new)?;
    descriptor.resolve().map_err(CliError::new)
}

fn validate_world_dir(path: &Path) -> Result<PathBuf> {
    let canonical = path.canonicalize().map_err(|e| {
        CliError::new(format!(
            "world directory {} does not exist or is not accessible: {e}",
            path.display()
        ))
    })?;
    for required in [
        "probe-batch.bin",
        "probe-batch-metadata.json",
        "world-manifest.json",
        "fixture.json",
    ] {
        if !canonical.join(required).is_file() {
            return Err(CliError::new(format!(
                "world directory {} is missing required file {required}",
                canonical.display()
            )));
        }
    }
    Ok(canonical)
}

fn reload_probe_batch(world: &Path) -> Result<BakedProbeBatch> {
    let bytes = atomicio::read_bytes(&world.join("probe-batch.bin"))?;
    let metadata_text = std::fs::read_to_string(world.join("probe-batch-metadata.json"))
        .map_err(|e| CliError::new(format!("cannot read probe-batch-metadata.json: {e}")))?;
    let metadata = parse_metadata(&metadata_text)?;
    let baked = BakedProbeBatch {
        metadata: metadata.clone(),
        bytes,
    };
    // The backend's own validation re-checks every field against the bytes and
    // the pinned version. Run it before any artifact is handed to Steam Audio.
    baked
        .validate()
        .map_err(|e| CliError::new(format!("reloaded probe batch failed validation: {e}")))?;
    Ok(baked)
}

fn parse_metadata(text: &str) -> Result<ProbeBatchMetadata> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct MetadataWire {
        schema_version: String,
        steam_audio_version: String,
        upstream_commit: String,
        probe_count: u32,
        path_data_size_bytes: u64,
        serialized_size_bytes: u64,
        content_sha256: String,
        bake_progress_callback_count: u32,
        final_bake_progress_millionths: u32,
    }
    let wire: MetadataWire = serde_json::from_str(text)
        .map_err(|e| CliError::new(format!("probe-batch-metadata.json is not valid: {e}")))?;
    if wire.schema_version != PROBE_BATCH_METADATA_SCHEMA {
        return Err(CliError::new(format!(
            "probe-batch metadata schema version is not {PROBE_BATCH_METADATA_SCHEMA}"
        )));
    }
    if wire.steam_audio_version != STEAM_AUDIO_VERSION {
        return Err(CliError::new(format!(
            "probe-batch metadata Steam Audio version is not {STEAM_AUDIO_VERSION}"
        )));
    }
    if wire.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT {
        return Err(CliError::new(format!(
            "probe-batch metadata upstream commit is not {STEAM_AUDIO_UPSTREAM_COMMIT}"
        )));
    }
    Ok(ProbeBatchMetadata {
        schema_version: PROBE_BATCH_METADATA_SCHEMA,
        steam_audio_version: STEAM_AUDIO_VERSION,
        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
        probe_count: wire.probe_count,
        path_data_size_bytes: wire.path_data_size_bytes,
        serialized_size_bytes: wire.serialized_size_bytes,
        content_sha256: wire.content_sha256,
        bake_progress_callback_count: wire.bake_progress_callback_count,
        final_bake_progress_millionths: wire.final_bake_progress_millionths,
    })
}

fn reload_world_manifest(world: &Path) -> Result<crate::bundle::WorldManifest> {
    let text = std::fs::read_to_string(world.join("world-manifest.json"))
        .map_err(|e| CliError::new(format!("cannot read world-manifest.json: {e}")))?;
    let manifest: crate::bundle::WorldManifest = serde_json::from_str(&text)
        .map_err(|e| CliError::new(format!("world-manifest.json is not valid: {e}")))?;
    if manifest.schema_version != crate::schema::WORLD_MANIFEST {
        return Err(CliError::new(format!(
            "world-manifest schema version is not {}",
            crate::schema::WORLD_MANIFEST
        )));
    }
    Ok(manifest)
}

/// Cross-validate the world manifest against the reloaded probe batch: the
/// recorded hash, size, probe count, and path-data size must all match.
fn verify_world_manifest_matches_batch(
    manifest: &crate::bundle::WorldManifest,
    baked: &BakedProbeBatch,
) -> Result<()> {
    if manifest.probe_batch_content_sha256 != baked.metadata.content_sha256 {
        return Err(CliError::new(
            "world-manifest probe hash does not match reloaded probe-batch metadata",
        ));
    }
    if manifest.serialized_size_bytes != baked.metadata.serialized_size_bytes {
        return Err(CliError::new(
            "world-manifest serialized size does not match reloaded probe-batch metadata",
        ));
    }
    if manifest.probe_count != baked.metadata.probe_count {
        return Err(CliError::new(
            "world-manifest probe count does not match reloaded probe-batch metadata",
        ));
    }
    if manifest.path_data_size_bytes != baked.metadata.path_data_size_bytes {
        return Err(CliError::new(
            "world-manifest path-data size does not match reloaded probe-batch metadata",
        ));
    }
    Ok(())
}

fn render_s3_with_baked(
    request: &S3RenderRequest,
    baked: &BakedProbeBatch,
) -> std::result::Result<S3RenderOutput, BackendError> {
    render_s3(request, baked)
}

/// Render the retained trajectory through one Steam Audio session. Asserts the
/// session was actually retained (every generation/load counter is exactly 1),
/// the block count equals the listener-pose count, the summed output covers the
/// exact total frame budget, an occlusion-state transition was observed, and the
/// summed-boundary continuity passed — before returning the evidence.
fn render_s3_trajectory_with_baked(
    request: &S3TrajectoryRenderRequest,
    baked: &BakedProbeBatch,
) -> std::result::Result<S3TrajectoryRenderOutput, BackendError> {
    let pose_count = request.listener_trajectory.len();
    let block_size = request.base.audio.frame_size as usize;
    let output = render_s3_trajectory(request, baked)?;
    // One retained session: every construction counter must be exactly 1 and the
    // rendered block count must equal the listener-pose count.
    assert_eq!(
        output.retained.context_generations, 1,
        "trajectory must retain one context"
    );
    assert_eq!(output.retained.scene_generations, 1);
    assert_eq!(output.retained.probe_batch_loads, 1);
    assert_eq!(output.retained.simulator_generations, 1);
    assert_eq!(output.retained.source_generations, 1);
    assert_eq!(output.retained.hrtf_generations, 1);
    assert_eq!(output.retained.effect_graph_generations, 1);
    assert_eq!(
        output.retained.rendered_blocks as usize, pose_count,
        "rendered block count must equal listener-pose count"
    );
    assert_eq!(output.blocks.len(), pose_count);
    let total_frames = block_size * pose_count;
    assert_eq!(
        output.summed.frame_count, total_frames,
        "summed output must cover the exact total frame budget"
    );
    assert!(
        output.summed.is_finite(),
        "trajectory summed output must be finite"
    );
    // An occlusion-state transition from the initial shadowed region to direct
    // line of sight must be observed in the retained blocks.
    let first_occlusion = output
        .blocks
        .first()
        .map(|b| b.direct_occlusion)
        .unwrap_or(1.0);
    let last_occlusion = output
        .blocks
        .last()
        .map(|b| b.direct_occlusion)
        .unwrap_or(0.0);
    assert!(
        last_occlusion > first_occlusion,
        "trajectory must transition from shadowed to direct line of sight"
    );
    // Finite nonzero path support across the trajectory.
    for block in &output.blocks {
        assert!(
            block.path_strength.is_finite() && block.path_strength > 0.0,
            "trajectory block path strength must be finite and nonzero"
        );
    }
    // Summed-boundary continuity must pass — the handoff is evidenced on the
    // SUMMED output, never inferred from a single diagnostic stem.
    assert!(
        output.continuity.passed,
        "summed boundary continuity failed: max ratio {}",
        output.continuity.maximum_step_to_local_peak_ratio
    );
    Ok(output)
}

/// Build the strict trajectory-metrics evidence sidecar from the retained
/// trajectory output. Every field is evidenced from the actual backend output.
fn build_trajectory_metrics(
    fixture_id: &str,
    output: &S3TrajectoryRenderOutput,
    request: &S3TrajectoryRenderRequest,
) -> Result<S3TrajectoryMetrics> {
    let block_size = request.base.audio.frame_size as usize;
    let sample_rate_hz = request.base.audio.sample_rate_hz as u32;
    let total_frames = block_size * request.listener_trajectory.len();

    // Hash the whole summed output bytes for the top-level binding; the verifier
    // recomputes the summed-boundary metric on the decoded WAV and cross-checks.
    let mut summed_bytes: Vec<u8> = Vec::with_capacity(output.summed.interleaved.len() * 4);
    for sample in &output.summed.interleaved {
        summed_bytes.extend_from_slice(&sample.to_le_bytes());
    }
    let trajectory_sum_hash = sha256_hex(&summed_bytes);

    let blocks = output
        .blocks
        .iter()
        .map(|block| {
            // Hash this block's summed PCM for per-pose cross-binding.
            let mut block_bytes: Vec<u8> = Vec::with_capacity(block.summed.interleaved.len() * 4);
            for sample in &block.summed.interleaved {
                block_bytes.extend_from_slice(&sample.to_le_bytes());
            }
            TrajectoryBlockPayload {
                block_index: block.block_index,
                listener_position_enu: [
                    block.listener.position_enu.x,
                    block.listener.position_enu.y,
                    block.listener.position_enu.z,
                ],
                direct_occlusion: block.direct_occlusion,
                path_strength: block.path_strength,
                summed_hash_sha256: sha256_hex(&block_bytes),
            }
        })
        .collect();

    let boundaries = output
        .continuity
        .boundaries
        .iter()
        .map(|b| BoundaryMeasurementPayload {
            after_block_index: b.after_block_index,
            max_step_full_scale: b.max_step_full_scale,
            local_peak_full_scale: b.local_peak_full_scale,
            step_to_local_peak_ratio: b.step_to_local_peak_ratio,
        })
        .collect();

    // The occlusion transition is evidenced by the first/last block occlusion.
    let first_occlusion = output
        .blocks
        .first()
        .map(|b| b.direct_occlusion)
        .unwrap_or(1.0);
    let last_occlusion = output
        .blocks
        .last()
        .map(|b| b.direct_occlusion)
        .unwrap_or(0.0);
    let occlusion_transition_observed = last_occlusion > first_occlusion;

    let _ = total_frames;
    Ok(S3TrajectoryMetrics {
        schema_version: S3TrajectoryMetrics::SCHEMA.into(),
        fixture_id: fixture_id.into(),
        trajectory_sum_hash_sha256: trajectory_sum_hash,
        sample_rate_hz,
        block_size_frames: block_size,
        block_count: request.listener_trajectory.len(),
        total_frames,
        occlusion_transition_observed,
        blocks,
        boundaries,
        maximum_step_to_local_peak_ratio: output.continuity.maximum_step_to_local_peak_ratio,
        step_to_local_peak_threshold: S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        window_frames: S3_CONTINUITY_WINDOW_FRAMES,
        continuity_passed: output.continuity.passed,
        retained: RetainedSessionStatsPayload {
            context_generations: output.retained.context_generations,
            scene_generations: output.retained.scene_generations,
            probe_batch_loads: output.retained.probe_batch_loads,
            simulator_generations: output.retained.simulator_generations,
            source_generations: output.retained.source_generations,
            hrtf_generations: output.retained.hrtf_generations,
            effect_graph_generations: output.retained.effect_graph_generations,
            rendered_blocks: output.retained.rendered_blocks,
        },
        non_claims: vec![
            "Trajectory continuity is evidenced on the summed output; a single diagnostic stem is insufficient."
                .into(),
            crate::provenance::NO_DELIVERED_EAR_SPL_NONCLAIM.into(),
        ],
    })
}

fn map_backend_error(error: BackendError) -> CliError {
    CliError::new(format!("Steam Audio S3 render failed: {error}"))
}

fn write_stem(dir: &Path, file_name: &str, pcm: &OwnedStereoPcm) -> Result<WrittenWav> {
    if !pcm.is_finite() {
        return Err(CliError::new(format!("{file_name} PCM is not finite")));
    }
    atomicio::write_stereo_wav(dir, file_name, pcm.sample_rate_hz, &pcm.interleaved)
}

fn assert_loaded_sizes_match_metadata(
    output: &S3RenderOutput,
    baked: &BakedProbeBatch,
) -> Result<()> {
    if output.loaded_probe_count != baked.metadata.probe_count {
        return Err(CliError::new(format!(
            "loaded probe count ({}) does not match metadata ({})",
            output.loaded_probe_count, baked.metadata.probe_count
        )));
    }
    if output.loaded_path_data_size_bytes != baked.metadata.path_data_size_bytes {
        return Err(CliError::new(format!(
            "loaded path-data size ({}) does not match metadata ({})",
            output.loaded_path_data_size_bytes, baked.metadata.path_data_size_bytes
        )));
    }
    Ok(())
}

fn assert_direct_occlusion_samples(
    output: &S3RenderOutput,
    request: &S3RenderRequest,
) -> Result<()> {
    // Canonical S3 direct occlusion is raycast. The requested and delivered
    // algorithms must BOTH be `Raycast`: no silent fallback to a different mode,
    // and no volumetric substitution (volumetric would additionally require a
    // positive source radius the fixture does not carry, and the bare
    // `occlusion_samples: 64` capacity must not be reinterpreted as a volumetric
    // request). The 64-sample budget is reserved as simulator capacity only.
    let requested = request.simulation.direct_occlusion;
    let delivered = output.snapshot.direct.delivered_occlusion_mode;
    if requested != fightbox_steam_audio::DirectOcclusionMode::Raycast {
        return Err(CliError::new(format!(
            "S3 contract requests raycast direct occlusion, but the request is {requested:?}"
        )));
    }
    if delivered != requested {
        return Err(CliError::new(format!(
            "direct snapshot delivered occlusion mode {delivered:?} does not match requested raycast"
        )));
    }
    if request.simulation.max_occlusion_samples != 64 {
        return Err(CliError::new(format!(
            "S3 contract reserves max_occlusion_samples=64 as simulator capacity, got {}",
            request.simulation.max_occlusion_samples
        )));
    }
    Ok(())
}

fn assert_path_snapshot_finite_and_first_order(output: &S3RenderOutput) -> Result<()> {
    let path = &output.snapshot.path;
    if !path.eq_coeffs.iter().copied().all(f32::is_finite)
        || !path.sh_coeffs.iter().copied().all(f32::is_finite)
    {
        return Err(CliError::new(
            "path snapshot EQ or SH coefficients are not finite",
        ));
    }
    match &path.direction {
        Some(direction) => {
            if direction.first_order_magnitude <= 0.0 {
                return Err(CliError::new(
                    "path first-order magnitude is non-positive; no directional energy",
                ));
            }
            Ok(())
        }
        None => Err(CliError::new(
            "path first-order directional moment is missing for the S3 corner fixture",
        )),
    }
}

fn assert_path_validation_unoccluded(output: &S3RenderOutput) -> Result<()> {
    let path = &output.snapshot.path;
    if path.validation_segments.is_empty() {
        return Err(CliError::new(
            "pathing validation callback reported no segments for the S3 corner fixture",
        ));
    }
    for segment in &path.validation_segments {
        if segment.occluded {
            return Err(CliError::new(format!(
                "a baked path segment was occluded at validation: from={:?} to={:?}",
                segment.from_enu_m, segment.to_enu_m
            )));
        }
    }
    Ok(())
}

fn assert_reflection_ir_size_positive(output: &S3RenderOutput) -> Result<()> {
    if output.snapshot.reflections.ir_size <= 0 {
        return Err(CliError::new("reflection snapshot irSize is non-positive"));
    }
    Ok(())
}

fn assert_azimuth_within_tolerance(
    output: &S3RenderOutput,
    fixture: &Fixture,
) -> Result<AnalyticPayload> {
    let expected = fixture
        .expected
        .analytic
        .as_ref()
        .ok_or_else(|| CliError::new("S3 fixture missing expected.analytic"))?;
    let analytic_azimuth = expected.arrival_azimuth_degrees_clockwise_from_north as f32;
    let tolerance = expected.tolerance_degrees as f32;
    let decoded = output
        .snapshot
        .path
        .direction
        .ok_or_else(|| CliError::new("decoded path direction is missing"))?;
    let arrival = decoded.azimuth_degrees_clockwise_from_north;
    let delta = (arrival - analytic_azimuth).abs();
    let within = delta <= tolerance;
    if !within {
        return Err(CliError::new(format!(
            "decoded path azimuth {arrival:.3}° is not within {tolerance:.1}° of analytic {analytic_azimuth:.3}°"
        )));
    }
    Ok(AnalyticPayload {
        arrival_azimuth_degrees_clockwise_from_north: arrival,
        analytic_azimuth_degrees_clockwise_from_north: analytic_azimuth,
        tolerance_degrees: tolerance,
        absolute_delta_degrees: delta,
        within_tolerance: true,
    })
}

fn assert_pathing_sums_differ(output: &S3RenderOutput) -> Result<PathingComparisonPayload> {
    let on = &output.stems.pathing_on_sum;
    let off = &output.stems.pathing_off_sum;
    let on_bytes = wav_bytes_for_hash(scene_audio_sample_rate(on.sample_rate_hz), &on.interleaved)?;
    let off_bytes = wav_bytes_for_hash(
        scene_audio_sample_rate(off.sample_rate_hz),
        &off.interleaved,
    )?;
    let on_hash = sha256_hex(&on_bytes);
    let off_hash = sha256_hex(&off_bytes);
    if on_hash == off_hash {
        return Err(CliError::new(
            "pathing-on and pathing-off sums are identical; path stem had no effect",
        ));
    }
    // Run the PUBLIC `fightbox_evidence::compare_pathing` on the exact delivered
    // pathing-on/off PCM at the documented bins. The canonical WAV writer is
    // lossless IEEE float32, so the verifier can rerun this same function on the
    // decoded bytes and compare within tight float-order tolerance — no
    // quantization excuse and no private recorder helper.
    let spec = fightbox_evidence::WavSpec {
        sample_rate_hz: scene_audio_sample_rate(on.sample_rate_hz),
        channels: 2,
    };
    let comparison = fightbox_evidence::compare_pathing(
        spec,
        &on.interleaved,
        &off.interleaved,
        crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
    )
    .map_err(|e| CliError::new(format!("pathing comparison failed: {e:?}")))?;
    if !comparison.differs {
        return Err(CliError::new(
            "compare_pathing reports pathing on/off sums do not differ above threshold",
        ));
    }
    Ok(pathing_comparison_payload(on_hash, off_hash, &comparison))
}

fn scene_audio_sample_rate(reported: i32) -> u32 {
    if reported > 0 {
        reported as u32
    } else {
        48_000
    }
}

fn wav_bytes_for_hash(sample_rate_hz: u32, interleaved: &[f32]) -> Result<Vec<u8>> {
    use fightbox_evidence::{WavSpec, write_wav};
    let spec = WavSpec {
        sample_rate_hz,
        channels: 2,
    };
    write_wav(spec, interleaved)
        .map_err(|e| CliError::new(format!("wav encode failed: {}", e.as_str())))
}

fn build_snapshot_payload(output: &S3RenderOutput) -> S3SnapshotPayload {
    let direct: DirectSnapshotPayload = direct_snapshot_payload(&output.snapshot.direct);
    let path: PathSnapshotPayload = path_snapshot_payload(&output.snapshot.path);
    let reflections: ReflectionSnapshotPayload =
        reflection_snapshot_payload(&output.snapshot.reflections);
    let validation_total = output.snapshot.path.validation_segments.len();
    let validation_occluded = output
        .snapshot
        .path
        .validation_segments
        .iter()
        .filter(|segment| segment.occluded)
        .count();
    S3SnapshotPayload {
        direct,
        path,
        reflections,
        loaded_probe_count: output.loaded_probe_count,
        loaded_path_data_size_bytes: output.loaded_path_data_size_bytes,
        validation_segments_total: validation_total,
        validation_segments_occluded: validation_occluded,
    }
}

fn file_size(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)
        .map_err(|e| CliError::new(format!("cannot stat {}: {e}", path.display())))?
        .len())
}

fn stem_bundle_file(
    dir: &Path,
    name: &str,
    kind: &str,
    written: &WrittenWav,
) -> Result<BundleFile> {
    Ok(BundleFile {
        name: name.into(),
        kind: kind.into(),
        content_sha256: written.content_sha256.clone(),
        size_bytes: file_size(&dir.join(name))?,
    })
}

fn text_bundle_file(dir: &Path, name: &str, kind: &str) -> Result<BundleFile> {
    let bytes = std::fs::read(dir.join(name))
        .map_err(|e| CliError::new(format!("cannot read {name}: {e}")))?;
    Ok(BundleFile {
        name: name.into(),
        kind: kind.into(),
        content_sha256: sha256_hex(&bytes),
        size_bytes: bytes.len() as u64,
    })
}

fn s3_claims(comparison: &PathingComparisonPayload) -> Vec<String> {
    vec![
        format!(
            "S3 pathing on/off sums differ (on_sha256={}, off_sha256={}).",
            comparison.on_sum_hash_sha256, comparison.off_sum_hash_sha256
        ),
        "S3 canonical direct occlusion is raycast (requested and delivered); max_occlusion_samples=64 is simulator capacity, not a volumetric request.".into(),
    ]
}

fn s3_non_claims() -> Vec<String> {
    vec![
        NO_DELIVERED_EAR_SPL_NONCLAIM.into(),
        UNCOMMITTED_SOURCE_NONCLAIM.into(),
        REMAINING_PHASE_A_GATES_NONCLAIM.into(),
        "S3 mechanical artifacts do not establish audible quality; the listening record is undecided."
            .into(),
    ]
}

fn build_listening_record(
    fixture_id: &str,
    fixture_hash: &str,
    manifest_hash: &str,
) -> ListeningRecord {
    let record = ListeningRecord::new(
        format!("s3-provisional-{manifest_hash}"),
        fixture_id,
        ListenerIdentity {
            listener_id: "unassigned".into(),
            notes: "provisional template emitted by s3-render; no human has completed it yet."
                .into(),
        },
        HrtfRecord {
            hrtf_set: "steam-audio-default".into(),
            pretest_result: "not_run".into(),
        },
        EquipmentRecord {
            headphones: "unassigned".into(),
            output_path: "unassigned".into(),
            monitor_gain_db: None,
        },
        SignOff {
            listener_signed: "unassigned".into(),
            date_iso: today_iso(),
        },
        today_iso(),
    );
    // Bind the record to its artifacts. The record remains undecided; only a
    // human signing and flipping result can complete it.
    let mut bound = record;
    bound.fixture_sha256 = Some(fixture_hash.into());
    bound.bundle_manifest_sha256 = Some(manifest_hash.into());
    bound.result = ListeningResult::Undecided;
    bound
}

fn today_iso() -> String {
    // Best-effort ISO date from SystemTime. No claim of wall-clock accuracy;
    // this only records when the template was emitted.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert days-since-epoch to (year, month, day) via the well-known
/// Gregorian algorithm. Bounded by u64 range; correct for the testable era.
fn civil_from_days(days: u64) -> (i32, u32, u32) {
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year as i32, m as u32, d as u32)
}

fn build_provenance_json(
    sdk: &SdkBinary,
    fixture_id: &str,
    gate: &str,
    world_dir: &Path,
    render_duration_s: f32,
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
        "steam_audio_expected_version": STEAM_AUDIO_VERSION,
        "steam_audio_expected_commit": STEAM_AUDIO_UPSTREAM_COMMIT,
        "binary_checksum_sha256": sdk.dylib_checksum_sha256,
        "dylib_path": sdk.dylib_path.as_deref().map(|p| p.to_string_lossy().into_owned()),
        "world_dir": world_dir.to_string_lossy(),
        "render_duration_s": render_duration_s,
        // Authority-note §ν build profile. Phase A offline capture fixes a
        // 48 kHz / 128-frame binaural profile at the "phase-a" quality tier;
        // streaming and real-time callbacks are genuinely not applicable and
        // say so explicitly and consistently.
        "build_profile": "phase-a-offline",
        "sample_rate_hz": 48_000,
        "block_size_frames": 128,
        "requested_quality": "phase-a",
        "delivered_quality": "phase-a",
        "streaming_cadence": "not_applicable",
        "callback_timing": "not_applicable",
        "probe_batch_metadata_schema": PROBE_BATCH_METADATA_SCHEMA,
        "non_claims": [
            UNCOMMITTED_SOURCE_NONCLAIM,
            NO_DELIVERED_EAR_SPL_NONCLAIM,
            REMAINING_PHASE_A_GATES_NONCLAIM,
            "S3 render invocation reuses no in-memory state from s3-bake.",
        ],
    });
    let _ = &mut object;
    serde_json::to_string_pretty(&object).expect("provenance JSON must serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_roundtrip_preserves_every_field() {
        let bytes = vec![1_u8, 2, 3, 4];
        let original = ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: 324,
            path_data_size_bytes: 4_096,
            serialized_size_bytes: bytes.len() as u64,
            content_sha256: sha256_hex(&bytes),
            bake_progress_callback_count: 2,
            final_bake_progress_millionths: 1_000_000,
        };
        // Round-trip through the backend's own JSON sidecar form.
        let text = original.to_json();
        let parsed = parse_metadata(&text).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn metadata_rejects_wrong_schema_version() {
        let text = r#"{
            "schema_version": "fightbox.bogus.v1",
            "steam_audio_version": "4.8.1",
            "upstream_commit": "0da1825",
            "probe_count": 1,
            "path_data_size_bytes": 1,
            "serialized_size_bytes": 1,
            "content_sha256": "x",
            "bake_progress_callback_count": 1,
            "final_bake_progress_millionths": 1000000
        }"#;
        assert!(parse_metadata(text).is_err());
    }

    #[test]
    fn metadata_rejects_unknown_field() {
        let text = r#"{
            "schema_version": "fightbox.steam-audio.probe-batch.v1",
            "steam_audio_version": "4.8.1",
            "upstream_commit": "0da1825",
            "probe_count": 1,
            "path_data_size_bytes": 1,
            "serialized_size_bytes": 1,
            "content_sha256": "x",
            "bake_progress_callback_count": 1,
            "final_bake_progress_millionths": 1000000,
            "unexpected": true
        }"#;
        assert!(parse_metadata(text).is_err());
    }

    #[test]
    fn metadata_rejects_wrong_steam_audio_version() {
        let bytes = vec![0_u8; 4];
        let mut metadata = test_metadata(&bytes);
        metadata.steam_audio_version = "4.8.0";
        // The wire form carries the wrong version; parse must reject it.
        let text = metadata_json(&metadata);
        assert!(parse_metadata(&text).is_err());
    }

    #[test]
    fn civil_from_days_matches_known_epoch() {
        // 1970-01-01 is day 0.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2026-01-01 is 20_454 days after epoch.
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    }

    #[test]
    fn compare_pathing_distinguishes_real_pathing_difference() {
        // The recorder runs the PUBLIC compare_pathing at the documented bins on
        // the exact delivered PCM. A real pathing-on/off difference must be
        // detected (differs=true) at those bins.
        use fightbox_evidence::{WavSpec, compare_pathing, sine};
        let spec = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let off = sine(spec, 1_000.0, 4_800, -20.0).unwrap().samples;
        let mut on = off.clone();
        for (o, a) in on
            .iter_mut()
            .zip(sine(spec, 1_000.0, 4_800, -12.0).unwrap().samples.iter())
        {
            *o += a;
        }
        let comparison = compare_pathing(
            spec,
            &on,
            &off,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
        )
        .unwrap();
        assert!(comparison.differs);
        assert_eq!(
            comparison.bins_hz,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ
        );
        assert!(comparison.level_difference_db.unwrap() > 0.0);
    }

    fn test_metadata(bytes: &[u8]) -> ProbeBatchMetadata {
        ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: 4,
            path_data_size_bytes: 64,
            serialized_size_bytes: bytes.len() as u64,
            content_sha256: sha256_hex(bytes),
            bake_progress_callback_count: 1,
            final_bake_progress_millionths: 1_000_000,
        }
    }

    fn metadata_json(metadata: &ProbeBatchMetadata) -> String {
        serde_json::json!({
            "schema_version": metadata.schema_version,
            "steam_audio_version": metadata.steam_audio_version,
            "upstream_commit": metadata.upstream_commit,
            "probe_count": metadata.probe_count,
            "path_data_size_bytes": metadata.path_data_size_bytes,
            "serialized_size_bytes": metadata.serialized_size_bytes,
            "content_sha256": metadata.content_sha256,
            "bake_progress_callback_count": metadata.bake_progress_callback_count,
            "final_bake_progress_millionths": metadata.final_bake_progress_millionths,
        })
        .to_string()
    }
}

/// Correction 4 (a, b, c): mutation matrix proving the recorder's direct
/// occlusion contract and pathing comparison cannot be forged. Each test
/// mutates a structurally valid render output/request and asserts the recorder
/// rejects the mutation with a specific error, so a forged capture cannot pass.
#[cfg(test)]
mod recorder_mutation_tests {
    use super::*;
    use fightbox_steam_audio::{
        DirectOcclusionMode, DirectSnapshot, OwnedStereoPcm, S3RenderOutput, S3RenderRequest,
        S3SimulationSnapshot,
    };

    /// A request whose simulation carries the canonical contract: raycast
    /// direct occlusion with max_occlusion_samples=64 reserved as capacity.
    fn canonical_request() -> S3RenderRequest {
        S3RenderRequest::controlled_default(vec![0.0_f32; 128])
    }

    /// The canonical direct snapshot: requested and delivered both Raycast.
    fn canonical_direct_snapshot() -> DirectSnapshot {
        DirectSnapshot {
            distance_attenuation: 0.5,
            air_absorption: [1.0, 0.99, 0.98],
            directivity: 1.0,
            occlusion: 0.0,
            transmission: [0.0, 0.0, 0.0],
            requested_occlusion_mode: DirectOcclusionMode::Raycast,
            delivered_occlusion_mode: DirectOcclusionMode::Raycast,
        }
    }

    /// Build a minimal render output carrying only a direct snapshot. The path
    /// and reflection snapshots are unused by `assert_direct_occlusion_samples`,
    /// so we leave them at zeroed values via a private helper.
    fn output_with_direct(direct: DirectSnapshot) -> S3RenderOutput {
        S3RenderOutput {
            loaded_probe_count: 324,
            loaded_path_data_size_bytes: 1_024,
            snapshot: S3SimulationSnapshot {
                direct,
                path: empty_path_snapshot(),
                reflections: empty_reflection_snapshot(),
            },
            stems: canonical_pathing_stems(),
        }
    }

    /// Build a minimal render output carrying only the pathing stems. Used by
    /// the pathing-difference tests, which never touch the snapshots.
    fn output_with_stems(stems: fightbox_steam_audio::S3Stems) -> S3RenderOutput {
        S3RenderOutput {
            loaded_probe_count: 324,
            loaded_path_data_size_bytes: 1_024,
            snapshot: S3SimulationSnapshot {
                direct: canonical_direct_snapshot(),
                path: empty_path_snapshot(),
                reflections: empty_reflection_snapshot(),
            },
            stems,
        }
    }

    /// Two genuinely different pathing on/off sums (a real spectral difference
    /// at the documented bins) so `assert_pathing_sums_differ` accepts the
    /// unmutated baseline.
    fn canonical_pathing_stems() -> fightbox_steam_audio::S3Stems {
        let silence = OwnedStereoPcm {
            sample_rate_hz: 48_000,
            frame_count: 4_800,
            interleaved: vec![0.0_f32; 9_600],
        };
        // pathing_on adds a 1 kHz tone at -12 dBFS; pathing_off is silent. The
        // on/off sums therefore differ and compare_pathing reports differs=true.
        let tone: Vec<f32> = (0..9_600)
            .map(|i| (i as f32 * 2.0 * core::f32::consts::PI * 1_000.0 / 48_000.0).sin() * 0.25)
            .collect();
        let on = OwnedStereoPcm {
            sample_rate_hz: 48_000,
            frame_count: 4_800,
            interleaved: tone,
        };
        fightbox_steam_audio::S3Stems {
            direct: silence.clone(),
            path: silence.clone(),
            reflections: silence.clone(),
            pathing_on_sum: on,
            pathing_off_sum: silence,
        }
    }

    /// A zeroed PathSnapshot. Only `assert_path_validation_*` reads its fields,
    /// which the direct-occlusion and pathing-difference tests do not call.
    fn empty_path_snapshot() -> fightbox_steam_audio::PathSnapshot {
        fightbox_steam_audio::PathSnapshot {
            eq_coeffs: [0.0, 0.0, 0.0],
            sh_coeffs: vec![],
            configured_order: 0,
            direction: None,
            validation_segments: vec![],
        }
    }

    /// A zeroed ReflectionSnapshot, for the same reason as above.
    fn empty_reflection_snapshot() -> fightbox_steam_audio::ReflectionSnapshot {
        fightbox_steam_audio::ReflectionSnapshot {
            requested_effect_type: fightbox_steam_audio::ReflectionEffectType::Convolution,
            delivered_effect_type: fightbox_steam_audio::ReflectionEffectType::Convolution,
            num_channels: 0,
            sdk_num_channels: 0,
            ir_size: 0,
            reverb_times: [0.0, 0.0, 0.0],
            eq: [0.0, 0.0, 0.0],
            delay_samples: 0,
            configured_hybrid_transition_time_s: None,
            configured_hybrid_overlap_percent: None,
            applied_reverb_times: None,
            applied_hybrid_eq: None,
            applied_hybrid_delay_samples: None,
        }
    }

    #[test]
    fn canonical_baseline_is_accepted_by_recorder_assertions() {
        // The unmutated canonical output/request must satisfy both recorder
        // assertions. This is the control: every rejection below is a real
        // mutation of an otherwise-valid capture, not a malformed fixture.
        let output = output_with_direct(canonical_direct_snapshot());
        let request = canonical_request();
        assert_direct_occlusion_samples(&output, &request).expect(
            "canonical raycast + max_occlusion_samples=64 must be accepted by the recorder",
        );
        assert_pathing_sums_differ(&output)
            .expect("canonical pathing on/off difference must be accepted by the recorder");
    }

    // ---- Correction 4(a): canonical S3 volumetric substitution rejected ----

    #[test]
    fn recorder_rejects_request_that_substitutes_volumetric_for_raycast() {
        // A request that silently substitutes Volumetric for the canonical
        // Raycast must be rejected. The bare occlusion_samples=64 capacity is
        // NOT a positive volumetric radius and must never be reinterpreted as
        // a volumetric request.
        let output = output_with_direct(canonical_direct_snapshot());
        let mut request = canonical_request();
        request.simulation.direct_occlusion = DirectOcclusionMode::Volumetric {
            radius_m: 0.5,
            sample_count: 64,
        };
        let err = assert_direct_occlusion_samples(&output, &request).unwrap_err();
        assert!(
            err.message().contains("raycast") && err.message().contains("Volumetric"),
            "expected volumetric-substitution rejection, got: {}",
            err.message()
        );
    }

    #[test]
    fn recorder_rejects_delivered_volumetric_when_request_was_raycast() {
        // Even if the request is canonical raycast, a delivered snapshot that
        // fell back to Volumetric must be rejected: no silent mode fallback.
        let mut direct = canonical_direct_snapshot();
        let request = canonical_request();
        direct.delivered_occlusion_mode = DirectOcclusionMode::Volumetric {
            radius_m: 0.5,
            sample_count: 64,
        };
        let output = output_with_direct(direct);
        let err = assert_direct_occlusion_samples(&output, &request).unwrap_err();
        assert!(
            err.message().contains("does not match requested raycast"),
            "expected delivered-mode mismatch rejection, got: {}",
            err.message()
        );
    }

    // ---- Correction 4(b): requested vs delivered direct modes cannot be forged ----

    #[test]
    fn recorder_rejects_non_raycast_requested_mode() {
        // Any non-raycast requested mode is a contract violation, even if the
        // delivered mode happens to be raycast. Requested and delivered must
        // BOTH be raycast.
        let output = output_with_direct(canonical_direct_snapshot());
        let mut request = canonical_request();
        // A second Volumetric variant: radius/sample_count chosen so the
        // delivered raycast would "match" if the check only looked at delivered.
        request.simulation.direct_occlusion = DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 64,
        };
        let err = assert_direct_occlusion_samples(&output, &request).unwrap_err();
        assert!(
            err.message().contains("requests raycast"),
            "expected requested-mode rejection, got: {}",
            err.message()
        );
    }

    #[test]
    fn recorder_rejects_max_occlusion_samples_other_than_64() {
        // The 64-sample budget is reserved as simulator capacity. A request
        // that uses a different capacity (e.g. 32, masquerading as a volumetric
        // sample count) must be rejected.
        let output = output_with_direct(canonical_direct_snapshot());
        let mut request = canonical_request();
        request.simulation.max_occlusion_samples = 32;
        let err = assert_direct_occlusion_samples(&output, &request).unwrap_err();
        assert!(
            err.message().contains("max_occlusion_samples=64"),
            "expected capacity rejection, got: {}",
            err.message()
        );
    }

    // ---- Correction 4(c): altered pathing metrics rejected despite intact PCM ----

    #[test]
    fn recorder_rejects_identical_pathing_on_off_sums() {
        // If the path stem had no effect, the on/off sums are identical and the
        // recorder rejects before even computing compare_pathing. This catches
        // a capture where pathing was silently disabled.
        let silence = OwnedStereoPcm {
            sample_rate_hz: 48_000,
            frame_count: 4_800,
            interleaved: vec![0.0_f32; 9_600],
        };
        let stems = fightbox_steam_audio::S3Stems {
            direct: silence.clone(),
            path: silence.clone(),
            reflections: silence.clone(),
            pathing_on_sum: silence.clone(),
            pathing_off_sum: silence,
        };
        let output = output_with_stems(stems);
        let err = assert_pathing_sums_differ(&output).unwrap_err();
        assert!(
            err.message().contains("identical"),
            "expected identical-sums rejection, got: {}",
            err.message()
        );
    }

    #[test]
    fn recorder_rejects_pathing_sums_below_difference_threshold() {
        // Two sums that differ in PCM (so the hash check passes) but whose
        // spectral/level difference is below the compare_pathing threshold must
        // be rejected. This proves the recorder runs the real public metric,
        // not just a hash inequality check.
        let base: Vec<f32> = (0..9_600)
            .map(|i| (i as f32 * 2.0 * core::f32::consts::PI * 1_000.0 / 48_000.0).sin() * 0.25)
            .collect();
        let mut negligible = base.clone();
        // A 1e-7 perturbation: changes the bytes (and the hash) but is far below
        // any audible or float-order-significant spectral difference threshold.
        for sample in &mut negligible {
            *sample += 1.0e-7;
        }
        let silence = OwnedStereoPcm {
            sample_rate_hz: 48_000,
            frame_count: 4_800,
            interleaved: vec![0.0_f32; 9_600],
        };
        let stems = fightbox_steam_audio::S3Stems {
            direct: silence.clone(),
            path: silence.clone(),
            reflections: silence.clone(),
            pathing_on_sum: OwnedStereoPcm {
                sample_rate_hz: 48_000,
                frame_count: 4_800,
                interleaved: base,
            },
            pathing_off_sum: OwnedStereoPcm {
                sample_rate_hz: 48_000,
                frame_count: 4_800,
                interleaved: negligible,
            },
        };
        let output = output_with_stems(stems);
        let err = assert_pathing_sums_differ(&output).unwrap_err();
        assert!(
            err.message().contains("do not differ"),
            "expected below-threshold rejection, got: {}",
            err.message()
        );
    }

    #[test]
    fn recorder_pathing_payload_mirrors_public_compare_pathing_exactly() {
        // The payload the recorder records is the exact output of the public
        // compare_pathing — no private re-derivation. Run compare_pathing
        // directly on the canonical PCM and confirm the payload fields match.
        let output = output_with_stems(canonical_pathing_stems());
        let on = &output.stems.pathing_on_sum;
        let off = &output.stems.pathing_off_sum;
        let spec = fightbox_evidence::WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        let direct = fightbox_evidence::compare_pathing(
            spec,
            &on.interleaved,
            &off.interleaved,
            crate::schema::S3_PATHING_COMPARISON_BINS_HZ,
        )
        .unwrap();
        let payload = assert_pathing_sums_differ(&output).unwrap();
        assert_eq!(payload.bins_hz, direct.bins_hz);
        assert_eq!(payload.on_rms_dbfs, direct.on_rms_dbfs);
        assert_eq!(payload.off_rms_dbfs, direct.off_rms_dbfs);
        assert_eq!(payload.level_difference_db, direct.level_difference_db);
        assert_eq!(payload.energy, direct.energy.as_str());
        assert_eq!(payload.differs, direct.differs);
        assert!((payload.spectral_l1_difference - direct.spectral_l1_difference).abs() < 1.0e-6);
        assert!((payload.spectral_l2_difference - direct.spectral_l2_difference).abs() < 1.0e-6);
    }

    /// Guard: the canonical request really does carry the contract values, so
    /// the rejection tests above are meaningful (they mutate a valid baseline).
    #[test]
    fn canonical_request_carries_raycast_and_capacity_64() {
        let request = canonical_request();
        assert_eq!(
            request.simulation.direct_occlusion,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(request.simulation.max_occlusion_samples, 64);
    }
}
