//! Deterministic, versioned manifest and metrics structs for capture bundles.
//!
//! These are emitted by serde structs with fixed declaration order. They bind a
//! bundle to its fixture, the on-disk world (S3), the source calibration chain,
//! every stem hash, and the explicit claims/non-claims. The verifier recomputes
//! every hash and metric from the artifacts, so a manifest never establishes a
//! pass by itself.

use serde::{Deserialize, Serialize};

use crate::schema::{S0_METRICS, S3_METRICS, S3_TRAJECTORY_METRICS, WORLD_MANIFEST};

/// World directory manifest (`world-manifest.json`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldManifest {
    pub schema_version: String,
    pub fixture_id: String,
    pub fixture_content_sha256: String,
    pub probe_batch_content_sha256: String,
    pub serialized_size_bytes: u64,
    pub probe_count: u32,
    pub path_data_size_bytes: u64,
    pub files: Vec<String>,
}

impl WorldManifest {
    pub fn new(
        fixture_id: String,
        fixture_content_sha256: String,
        probe_batch_content_sha256: String,
        serialized_size_bytes: u64,
        probe_count: u32,
        path_data_size_bytes: u64,
        files: Vec<String>,
    ) -> Self {
        Self {
            schema_version: WORLD_MANIFEST.into(),
            fixture_id,
            fixture_content_sha256,
            probe_batch_content_sha256,
            serialized_size_bytes,
            probe_count,
            path_data_size_bytes,
            files,
        }
    }
}

/// Capture bundle manifest (`manifest.json` in an S0 or S3 bundle).
///
/// This is the CLI-owned artifact index. The richer `CaptureRunManifest` from
/// `fightbox-evidence` is written alongside it as `metrics.json` so both the
/// index and the authority-note §ν provenance are available.
///
/// ## Digest semantics (resolved honestly)
///
/// A JSON object cannot contain the SHA-256 of its own final bytes: any digest
/// field changes the serialized form, and therefore the digest. This manifest
/// therefore uses two digests with distinct, honest meanings:
///
/// - `unsigned_manifest_sha256` (in-manifest) is the SHA-256 over the canonical
///   serialized form of the manifest *with the digest field nulled*. It is
///   stable, can be recomputed verbatim by any reader, and is the canonical
///   binding key for listening-record provenance.
/// - `manifest.sha256` (detached sidecar file, beside `manifest.json`) is the
///   SHA-256 over the exact final bytes of `manifest.json` on disk. It binds
///   "this is the file that was committed" without any self-reference paradox,
///   because it is NOT recorded inside the manifest.
///
/// The legacy single field `manifest_content_sha256` is retained as an alias of
/// the unsigned digest for backwards-compatible readers, but the canonical,
/// recomputable binding is `unsigned_manifest_sha256`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub schema_version: String,
    pub gate: String,
    pub fixture_id: String,
    pub fixture_content_sha256: String,
    pub asset_id: String,
    pub asset_descriptor_sha256: String,
    pub files: Vec<BundleFile>,
    /// SHA-256 over the canonical serialized manifest with the digest field
    /// nulled. The canonical, recomputable binding key. See type docs.
    pub unsigned_manifest_sha256: Option<String>,
    /// Legacy alias of `unsigned_manifest_sha256`, retained for readers written
    /// against the v0 single-field form. Prefer `unsigned_manifest_sha256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest_content_sha256: Option<String>,
}

/// The filename of the detached final-file digest sidecar beside `manifest.json`.
pub const MANIFEST_DIGEST_SIDECAR: &str = "manifest.sha256";

/// One file recorded in a bundle manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleFile {
    pub name: String,
    pub kind: String,
    pub content_sha256: String,
    pub size_bytes: u64,
}

impl BundleManifest {
    pub const SCHEMA: &'static str = "fightbox.bundle-manifest.v1";

    /// Find a file by name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&BundleFile> {
        self.files.iter().find(|file| file.name == name)
    }

    /// The canonical unsigned-manifest digest, recomputed from this manifest's
    /// own fields with the digest field nulled and re-serialized canonically.
    /// This is the stable, recomputable binding key for listening/provenance.
    #[must_use]
    pub fn recompute_unsigned_digest(&self) -> String {
        let mut clone = self.clone();
        clone.unsigned_manifest_sha256 = None;
        clone.manifest_content_sha256 = None;
        // Canonical deterministic serialization: pretty-printed serde_json.
        let bytes = serde_json::to_vec_pretty(&clone)
            .expect("BundleManifest must serialize for its unsigned digest");
        fightbox_evidence::sha256_hex(&bytes)
    }

    /// The canonical unsigned digest recorded in the manifest, or `None` if the
    /// manifest was never finalized with one.
    #[must_use]
    pub fn unsigned_digest(&self) -> Option<&str> {
        self.unsigned_manifest_sha256.as_deref()
    }
}

/// Per-trajectory distance metric for an S0 capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S0TrajectoryMetric {
    pub distance_m: f32,
    pub index: usize,
    pub air_absorption_enabled: bool,
    pub channel: ChannelMetricPayload,
    pub distance_attenuation: f32,
    pub air_absorption: [f32; 3],
    pub relative_direction_steam: [f32; 3],
}

/// A channel-health payload (mirrors `fightbox-evidence::ChannelMetrics` fields
/// needed by the metrics sidecar; serialized deterministically here).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelMetricPayload {
    pub frame_count: usize,
    pub channels: u16,
    pub sample_rate_hz: u32,
    pub all_finite: bool,
    pub peak_per_channel: Vec<f32>,
    pub rms_per_channel: Vec<f32>,
    pub rms_dbfs_per_channel: Vec<Option<f32>>,
    pub silent_channel_count: usize,
    pub stereo_difference_rms: Option<f32>,
}

/// S0 capture metrics (`metrics.json` in an S0 bundle).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S0Metrics {
    pub schema_version: String,
    pub fixture_id: String,
    pub sample_rate_hz: u32,
    pub frame_count_per_distance: usize,
    pub calibration: CalibrationPayload,
    pub trajectory: Vec<S0TrajectoryMetric>,
    pub control_100m_air_disabled: S0TrajectoryMetric,
    /// Inverse-distance contribution from 100 m to 1 m in dB (≈ +40 dB).
    pub inverse_distance_100m_to_1m_db: f32,
    /// Tolerance on the inverse-distance assertion (dB).
    pub inverse_distance_tolerance_db: f32,
    /// High-band energy bound: enabled-air 100 m high-band must not exceed
    /// disabled-air 100 m high-band at the same pose.
    pub high_band_energy: HighBandComparison,
    pub claims: Vec<String>,
    pub non_claims: Vec<String>,
}

/// The recorded one-gain chain (ADR 0002), mirrored for the metrics sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CalibrationPayload {
    pub reference_spl_db: f32,
    pub reference_pcm_rms_dbfs: f32,
    pub reference_distance_m: f32,
    pub program_rms_dbfs: f32,
    pub target_source_rms_dbfs: f32,
    pub drive_gain_db: f32,
    pub linear_gain: f32,
}

/// High-band energy comparison for the enabled-vs-disabled air-absorption control.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighBandComparison {
    /// Cutoff frequency separating low and high bands (Hz).
    pub cutoff_hz: f32,
    /// Enabled-air 100 m high-band RMS (linear).
    pub enabled_air_100m_high_band_rms: f32,
    /// Disabled-air 100 m high-band RMS (linear).
    pub disabled_air_100m_high_band_rms: f32,
    /// True when enabled-air high-band energy does not exceed disabled-air.
    pub enabled_does_not_exceed_disabled: bool,
}

impl S0Metrics {
    pub const SCHEMA: &'static str = S0_METRICS;
}

/// Path direction payload mirrored from `PathDirectionEstimate`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathDirectionPayload {
    pub mean_arrival_direction_enu: [f32; 3],
    pub azimuth_degrees_clockwise_from_north: f32,
    pub first_order_magnitude: f32,
    pub zeroth_order_coefficient: f32,
    pub is_some: bool,
}

/// S3 simulation snapshot recorded into the metrics sidecar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3SnapshotPayload {
    pub direct: DirectSnapshotPayload,
    pub path: PathSnapshotPayload,
    pub reflections: ReflectionSnapshotPayload,
    pub loaded_probe_count: u32,
    pub loaded_path_data_size_bytes: u64,
    pub validation_segments_total: usize,
    pub validation_segments_occluded: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectSnapshotPayload {
    pub distance_attenuation: f32,
    pub air_absorption: [f32; 3],
    pub directivity: f32,
    pub occlusion: f32,
    pub transmission: [f32; 3],
    pub requested_occlusion_mode: OcclusionModePayload,
    pub delivered_occlusion_mode: OcclusionModePayload,
}

/// Mirrors `fightbox_steam_audio::DirectOcclusionMode` for the metrics sidecar.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcclusionModePayload {
    pub kind: OcclusionModeKind,
    /// Volumetric radius in metres; 0.0 for raycast.
    pub volumetric_radius_m: f32,
    /// Volumetric sample count; 0 for raycast.
    pub volumetric_sample_count: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcclusionModeKind {
    Raycast,
    Volumetric,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathSnapshotPayload {
    pub eq_coeffs: [f32; 3],
    pub sh_coeffs: Vec<f32>,
    pub configured_order: i32,
    pub direction: PathDirectionPayload,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReflectionSnapshotPayload {
    pub num_channels: i32,
    pub ir_size: i32,
    pub reverb_times: [f32; 3],
    pub eq: [f32; 3],
    pub delay_samples: i32,
}

/// Pathing on/off comparison recorded for S3.
///
/// Mirrors the public `fightbox_evidence::compare_pathing` result (run on the
/// exact delivered pathing-on/off PCM with [`crate::schema::S3_PATHING_COMPARISON_BINS_HZ`])
/// plus the two whole-file SHA-256 digests. The verifier independently decodes the
/// WAVs, reruns the same public `compare_pathing`, and cross-checks every field —
/// so a sidecar whose numbers were edited while the WAV hashes stayed valid is
/// rejected, and altering the PCM (even with the manifest hashes rewritten) cannot
/// preserve a pass because the recomputed spectral/level values move.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathingComparisonPayload {
    pub on_sum_hash_sha256: String,
    pub off_sum_hash_sha256: String,
    /// The exact spectral bins (Hz) the comparison was run at.
    pub bins_hz: Vec<f32>,
    /// RMS dBFS of the pathing-on mono mixdown, or `None` if silent.
    pub on_rms_dbfs: Option<f32>,
    /// RMS dBFS of the pathing-off mono mixdown, or `None` if silent.
    pub off_rms_dbfs: Option<f32>,
    /// `on - off` in dB when both captures are energetic; `None` otherwise.
    pub level_difference_db: Option<f32>,
    /// Explicit energy state string (mirrors `ComparisonEnergy::as_str`).
    pub energy: String,
    pub spectral_l1_difference: f32,
    pub spectral_l2_difference: f32,
    pub differs: bool,
}

/// S3 capture metrics (`metrics.json` in an S3 bundle).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Metrics {
    pub schema_version: String,
    pub fixture_id: String,
    pub sample_rate_hz: u32,
    pub frame_count: usize,
    pub calibration: CalibrationPayload,
    pub world: WorldPayload,
    pub snapshot: S3SnapshotPayload,
    pub pathing_comparison: PathingComparisonPayload,
    pub analytic: AnalyticPayload,
    pub stems: Vec<StemHashPayload>,
    pub claims: Vec<String>,
    pub non_claims: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldPayload {
    pub world_dir: String,
    pub world_content_sha256: String,
    pub probe_batch_content_sha256: String,
    pub serialized_size_bytes: u64,
    pub probe_count: u32,
    pub path_data_size_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyticPayload {
    pub arrival_azimuth_degrees_clockwise_from_north: f32,
    pub analytic_azimuth_degrees_clockwise_from_north: f32,
    pub tolerance_degrees: f32,
    pub absolute_delta_degrees: f32,
    pub within_tolerance: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StemHashPayload {
    pub kind: String,
    pub file: String,
    pub content_sha256: String,
    pub frame_count: usize,
}

impl S3Metrics {
    pub const SCHEMA: &'static str = S3_METRICS;
}

/// One block's pose/occlusion/path evidence in the retained trajectory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrajectoryBlockPayload {
    pub block_index: usize,
    /// Listener pose ENU `[x, y, z]` for this block.
    pub listener_position_enu: [f32; 3],
    pub direct_occlusion: f32,
    pub path_strength: f32,
    /// SHA-256 of this block's summed stereo PCM (for cross-binding).
    pub summed_hash_sha256: String,
}

/// One summed-boundary continuity measurement between adjacent blocks.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BoundaryMeasurementPayload {
    pub after_block_index: usize,
    pub max_step_full_scale: f32,
    pub local_peak_full_scale: f32,
    pub step_to_local_peak_ratio: f32,
}

/// Retained-session construction counters proving one context/scene/probe/
/// simulator/source/HRTF/effect graph was held across the whole trajectory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedSessionStatsPayload {
    pub context_generations: u32,
    pub scene_generations: u32,
    pub probe_batch_loads: u32,
    pub simulator_generations: u32,
    pub source_generations: u32,
    pub hrtf_generations: u32,
    pub effect_graph_generations: u32,
    pub rendered_blocks: u32,
}

/// S3 retained-trajectory metrics (`trajectory-metrics.json`). Strict evidence
/// for the summed-output handoff: pose/block/frame accounting, occlusion/path
/// strength per pose, retained generation/load counters, the recomputable
/// summed-boundary continuity, threshold, and pass result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3TrajectoryMetrics {
    pub schema_version: String,
    pub fixture_id: String,
    /// SHA-256 of the `trajectory-sum.wav` bytes this evidence binds.
    pub trajectory_sum_hash_sha256: String,
    pub sample_rate_hz: u32,
    /// One `block_size_frames` block per listener pose.
    pub block_size_frames: usize,
    pub block_count: usize,
    pub total_frames: usize,
    /// Whether the trajectory contains an occlusion-state transition from the
    /// initial shadowed region to direct line of sight.
    pub occlusion_transition_observed: bool,
    pub blocks: Vec<TrajectoryBlockPayload>,
    pub boundaries: Vec<BoundaryMeasurementPayload>,
    pub maximum_step_to_local_peak_ratio: f32,
    pub step_to_local_peak_threshold: f32,
    pub window_frames: usize,
    pub continuity_passed: bool,
    pub retained: RetainedSessionStatsPayload,
    pub non_claims: Vec<String>,
}

impl S3TrajectoryMetrics {
    pub const SCHEMA: &'static str = S3_TRAJECTORY_METRICS;
}
