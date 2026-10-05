//! `fightbox phase-a s3-bake` — generate probes, invoke the real
//! `iplPathBakerBake`, serialize the probe batch, and write the world
//! directory.
//!
//! This is the first of two process invocations: it bakes and exits. The
//! resulting world directory is reloaded by a separate `s3-render` invocation.
//! No in-memory bake state is reused across the two commands.
//!
//! Artifact contract for the world directory:
//!   `probe-batch.bin`, `probe-batch-metadata.json`, `world-manifest.json`,
//!   `fixture.json` (exact input bytes).

use std::path::Path;
use std::time::Instant;

use fightbox_steam_audio::{
    BackendError, BakedProbeBatch, PROBE_BATCH_METADATA_SCHEMA, STEAM_AUDIO_UPSTREAM_COMMIT,
    STEAM_AUDIO_VERSION, bake_s3, sha256_hex,
};

use crate::asset::{AssetDescriptor, ResolvedAsset};
use crate::atomicio::{
    AtomicDir, validate_output_path, write_bytes_atomic, write_bytes_plain, write_json_atomic,
    write_json_string_atomic,
};
use crate::bundle::WorldManifest;
use crate::error::{CliError, Result};
use crate::fixture::Fixture;
use crate::provenance::{self, ENGINE_IDENTITY, SdkBinary};
use crate::scene::FixtureScene;

/// Run the S3 bake. `out` is the world directory to create atomically.
pub fn run(fixture_path: &Path, out: &Path) -> Result<()> {
    require_linked()?;
    let fixture = load_fixture(fixture_path)?;
    let asset = load_asset(&fixture)?;
    let out = validate_output_path(out)?;
    let sdk = SdkBinary::detect();
    // An artifact-producing capture is invalid unless the actual dylib path and
    // checksum were established from STEAM_AUDIO_SDK_DIR at runtime.
    sdk.require_established()?;

    let scene = FixtureScene::build(fixture.clone(), &asset)?;

    let bake_request = scene.s3_bake_request().map_err(CliError::new)?;
    let started = Instant::now();
    let baked = bake_s3(&bake_request).map_err(map_backend_error)?;
    let bake_duration_s = started.elapsed().as_secs_f32() as f32;

    // The backend already validates metadata, but the authority note requires
    // every gate condition to be evidenced explicitly.
    assert_bake_metadata(&baked)?;

    let dir = AtomicDir::create(out.clone())?;
    let temp = dir.temp_path().to_path_buf();

    // probe-batch.bin: exact bytes returned by iplProbeBatchSave.
    write_bytes_atomic(&temp.join("probe-batch.bin"), &baked.bytes)?;
    let probe_hash = baked.metadata.content_sha256.clone();

    // probe-batch-metadata.json: the backend's own deterministic JSON sidecar.
    let metadata_json = baked.metadata.to_json();
    write_json_string_atomic(&temp.join("probe-batch-metadata.json"), &metadata_json)?;

    // fixture.json: exact input bytes.
    let fixture_bytes = std::fs::read(fixture_path).map_err(|e| {
        CliError::new(format!(
            "cannot read fixture {}: {e}",
            fixture_path.display()
        ))
    })?;
    let fixture_hash = sha256_hex(&fixture_bytes);
    write_bytes_plain(&temp.join("fixture.json"), &fixture_bytes)?;

    let manifest = WorldManifest::new(
        fixture.fixture_id.clone(),
        fixture_hash.clone(),
        probe_hash.clone(),
        baked.metadata.serialized_size_bytes,
        baked.metadata.probe_count,
        baked.metadata.path_data_size_bytes,
        vec![
            "probe-batch.bin".into(),
            "probe-batch-metadata.json".into(),
            "world-manifest.json".into(),
            "fixture.json".into(),
        ],
    );
    write_json_atomic(&temp.join("world-manifest.json"), &manifest)?;

    let provenance = serde_json::json!({
        "engine_identity": ENGINE_IDENTITY,
        "platform": provenance::platform(),
        "cpu_class": provenance::cpu_class(),
        "fixture_id": fixture.fixture_id,
        "gate": "S3-bake",
        "steam_audio_version": sdk.version,
        "steam_audio_upstream_commit": sdk.upstream_commit,
        "probe_batch_metadata_schema": PROBE_BATCH_METADATA_SCHEMA,
        "steam_audio_expected_version": STEAM_AUDIO_VERSION,
        "steam_audio_expected_commit": STEAM_AUDIO_UPSTREAM_COMMIT,
        "binary_checksum_sha256": sdk.dylib_checksum_sha256,
        "dylib_path": sdk.dylib_path.as_deref().map(|p| p.to_string_lossy().into_owned()),
        "bake_duration_s": bake_duration_s,
        "bake_progress_callback_count": baked.metadata.bake_progress_callback_count,
        "final_bake_progress_millionths": baked.metadata.final_bake_progress_millionths,
        "streaming_cadence": "not_applicable",
        "callback_timing": "not_applicable",
        "non_claims": [
            crate::provenance::UNCOMMITTED_SOURCE_NONCLAIM,
            crate::provenance::REMAINING_PHASE_A_GATES_NONCLAIM,
            "S3 bake invocation exits; no in-memory state is reused by s3-render.",
        ],
    });
    write_json_string_atomic(&temp.join("bake-provenance.json"), &provenance.to_string())?;

    dir.commit()?;
    eprintln!(
        "fightbox: S3 world written to {} (probes={}, path_bytes={}, sha256={probe_hash})",
        out.display(),
        baked.metadata.probe_count,
        baked.metadata.path_data_size_bytes
    );
    Ok(())
}

/// Assert every authority-note gate condition on the bake metadata: nonzero
/// probes/path bytes, nonempty serialization, SHA match, at least one progress
/// callback, and final progress exactly 1_000_000.
fn assert_bake_metadata(baked: &BakedProbeBatch) -> Result<()> {
    if baked.metadata.probe_count == 0 {
        return Err(CliError::new("iplPathBakerBake produced zero probes"));
    }
    if baked.metadata.path_data_size_bytes == 0 {
        return Err(CliError::new(
            "iplPathBakerBake returned no PATHING/DYNAMIC baked-data layer",
        ));
    }
    if baked.bytes.is_empty() {
        return Err(CliError::new(
            "iplProbeBatchSave produced an empty byte buffer",
        ));
    }
    let actual_hash = sha256_hex(&baked.bytes);
    if actual_hash != baked.metadata.content_sha256 {
        return Err(CliError::new(format!(
            "probe-batch content hash mismatch: metadata={} actual={actual_hash}",
            baked.metadata.content_sha256
        )));
    }
    if baked.metadata.bake_progress_callback_count == 0 {
        return Err(CliError::new(
            "iplPathBakerBake reported no progress callbacks",
        ));
    }
    if baked.metadata.final_bake_progress_millionths != 1_000_000 {
        return Err(CliError::new(format!(
            "iplPathBakerBake final progress was {}, not 1_000_000",
            baked.metadata.final_bake_progress_millionths
        )));
    }
    baked
        .validate()
        .map_err(|e| CliError::new(format!("baked probe batch failed validation: {e}")))?;
    Ok(())
}

fn require_linked() -> Result<()> {
    if !fightbox_steam_audio::backend_availability()
        .to_json()
        .contains(r#""status":"available""#)
    {
        return Err(CliError::new(
            "s3-bake requires a linked Steam Audio SDK; rebuild with --features linked-sdk and STEAM_AUDIO_SDK_DIR set",
        ));
    }
    Ok(())
}

fn load_fixture(path: &Path) -> Result<Fixture> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::new(format!("cannot read fixture {}: {e}", path.display())))?;
    let fixture = Fixture::parse(&text).map_err(CliError::new)?;
    if fixture.gate() != Ok(crate::fixture::Gate::S3) {
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

fn map_backend_error(error: BackendError) -> CliError {
    CliError::new(format!("Steam Audio S3 bake failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_assertion_rejects_zero_probes() {
        let baked = baked_fixture(0, 1, 1);
        let error = assert_bake_metadata(&baked).unwrap_err();
        assert!(error.message().contains("zero probes"));
    }

    #[test]
    fn metadata_assertion_rejects_zero_path_bytes() {
        let baked = baked_fixture(1, 0, 1);
        assert!(
            assert_bake_metadata(&baked)
                .unwrap_err()
                .message()
                .contains("baked-data layer")
        );
    }

    #[test]
    fn metadata_assertion_rejects_incomplete_progress() {
        let baked = baked_fixture(1, 1, 999_999);
        assert!(
            assert_bake_metadata(&baked)
                .unwrap_err()
                .message()
                .contains("1_000_000")
        );
    }

    #[test]
    fn metadata_assertion_rejects_hash_mismatch() {
        let mut baked = baked_fixture(1, 1, 1_000_000);
        baked.metadata.content_sha256 = "deadbeef".into();
        assert!(
            assert_bake_metadata(&baked)
                .unwrap_err()
                .message()
                .contains("content hash mismatch")
        );
    }

    fn baked_fixture(probe_count: u32, path_bytes: u64, progress: u32) -> BakedProbeBatch {
        let bytes = vec![0_u8; 16];
        let bytes_len = bytes.len() as u64;
        let hash = sha256_hex(&bytes);
        BakedProbeBatch {
            metadata: fightbox_steam_audio::ProbeBatchMetadata {
                schema_version: PROBE_BATCH_METADATA_SCHEMA,
                steam_audio_version: STEAM_AUDIO_VERSION,
                upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                probe_count,
                path_data_size_bytes: path_bytes,
                serialized_size_bytes: bytes_len,
                content_sha256: hash,
                bake_progress_callback_count: 1,
                final_bake_progress_millionths: progress,
            },
            bytes,
        }
    }
}
