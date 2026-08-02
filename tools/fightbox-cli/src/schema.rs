//! Stable, locally-owned JSON schema identifiers for the artifacts this CLI emits.
//!
//! The evidence and backend crates own their own schemas (`CaptureRunManifest`,
//! `ListeningRecord`, `ProbeBatchMetadata`, the asset descriptor). These
//! identifiers cover the artifacts introduced by the integration lane: the
//! world directory manifest, the per-bundle metrics sidecars, and the verify
//! result. Each is emitted as the `schema_version` field of a deterministic
//! serde struct so a consumer can bind a file to its contract.

/// World directory manifest (`world-manifest.json`).
pub const WORLD_MANIFEST: &str = "fightbox.world-manifest.v1";
/// S0 capture bundle metrics (`metrics.json` in an S0 bundle).
pub const S0_METRICS: &str = "fightbox.s0-metrics.v1";
/// S3 capture bundle metrics (`metrics.json` in an S3 bundle).
pub const S3_METRICS: &str = "fightbox.s3-metrics.v1";
/// S3 retained-trajectory metrics (`trajectory-metrics.json` in an S3 bundle).
pub const S3_TRAJECTORY_METRICS: &str = "fightbox.s3-trajectory-metrics.v1";
/// Verify command result JSON (`verify --bundle ...`).
pub const VERIFY_RESULT: &str = "fightbox.verify-result.v1";

/// The fixture schema version this CLI accepts (mirrors the frozen
/// `fixture.schema.json`; optional source directivity is a backward-compatible,
/// strictly validated parser extension under the same version).
pub const FIXTURE: &str = "fightbox.fixture.v1";
/// The asset descriptor schema version this CLI accepts.
pub const ASSET_DESCRIPTOR: &str = "fightbox.asset-descriptor.v1";

/// The documented spectral bins (Hz) used by the S3 pathing on/off comparison.
///
/// Both the recorder (`s3-render`) and the verifier (`verify`) call the public
/// `fightbox_evidence::compare_pathing` with exactly these bins on the delivered
/// pathing-on/off PCM. This is the documented *input* to the public metric, not a
/// private recorder helper: the verifier independently decodes the WAVs and reruns
/// the same public function rather than sharing the recorder's computation.
pub const S3_PATHING_COMPARISON_BINS_HZ: &[f32] = &[500.0, 1_000.0, 2_000.0, 4_000.0];
