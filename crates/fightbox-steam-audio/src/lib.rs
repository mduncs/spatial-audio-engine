//! The only Rust crate allowed to touch the Steam Audio C API.
//!
//! Domain coordinates stay right-handed local ENU. The single conversion at this
//! boundary is Steam `(x, y, z) = (ENU.x, ENU.z, -ENU.y)`.
//!
//! The Phase A entry points deliberately accept and return only Rust-owned data.
//! In particular, [`bake_s3`] returns a byte blob after every baking handle has
//! been released, and [`render_s3`] reloads that blob into a fresh set of SDK
//! handles. No Steam Audio handle or borrowed simulation-output pointer crosses
//! this crate's public API.

#![deny(unsafe_code)]

#[cfg(any(feature = "linked-sdk", test))]
mod backend_snapshot;
mod governor;
#[cfg(any(feature = "linked-sdk", test))]
mod motion_smoothing;
mod status;
#[allow(unsafe_code)]
mod world_swap;
pub use governor::{
    DeliveredReflectionQuality, GovernorSimulationPass, GovernorTransitionReason, PathQualityLevel,
    QualityGovernorTelemetry, REVERB_RUNG_CAPABILITIES, ReflectionQualityLevel,
    ReflectionSettingAvailability, ReverbRungAvailability, ReverbRungCapability, ReverbStrategy,
    SourceQualityLevel, SourceQualityTelemetry,
};
pub use status::{
    CapabilityStatus, GateStatus, RuntimeStatus, runtime_status, steam_audio_provenance,
};

use core::fmt;

/// The pinned Steam Audio release version.
pub const STEAM_AUDIO_VERSION: &str = "4.8.1";
/// The authoritative upstream tag commit for [`STEAM_AUDIO_VERSION`].
pub const STEAM_AUDIO_UPSTREAM_COMMIT: &str = "0da1825";
/// Official immutable release-asset URL. The acquisition script verifies it before extraction.
pub const STEAM_AUDIO_ASSET_URL: &str =
    "https://github.com/ValveSoftware/steam-audio/releases/download/v4.8.1/steamaudio_4.8.1.zip";
/// Exact byte length of the official release archive.
pub const STEAM_AUDIO_ASSET_SIZE_BYTES: u64 = 181_171_027;
/// SHA-256 of the official release archive, in lowercase hexadecimal.
pub const STEAM_AUDIO_ASSET_SHA256: &str =
    "4a0aa5ec1176f38f0b0993a37c2259d9e86f27e22d5e24f83ec4c3cb9a1d5449";
/// Stable schema name for metadata accompanying an SDK probe-batch serialization.
pub const PROBE_BATCH_METADATA_SCHEMA: &str = "fightbox.steam-audio.probe-batch.v1";

/// A vector in the engine's right-handed local ENU coordinate system.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnuVector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl EnuVector3 {
    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    #[cfg(feature = "linked-sdk")]
    fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

/// A vector in Steam Audio's right-handed coordinate system.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SteamVector3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl SteamVector3 {
    #[must_use]
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Converts a position, direction, orientation axis, or winding vertex from ENU to Steam Audio.
///
/// This is a proper rotation, so it preserves handedness and triangle winding.
#[must_use]
pub const fn enu_to_steam(vector: EnuVector3) -> SteamVector3 {
    SteamVector3 {
        x: vector.x,
        y: vector.z,
        z: -vector.y,
    }
}

/// Converts an SDK vector back into domain ENU coordinates.
#[must_use]
pub const fn steam_to_enu(vector: SteamVector3) -> EnuVector3 {
    EnuVector3 {
        x: vector.x,
        y: -vector.z,
        z: vector.y,
    }
}

/// A machine-readable explanation for why SDK-dependent capability is absent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnavailableMetadata {
    pub status: &'static str,
    pub reason: &'static str,
    pub requested_feature: &'static str,
    pub required_environment: &'static str,
    pub expected_version: &'static str,
    pub upstream_commit: &'static str,
}

impl UnavailableMetadata {
    /// Stable, dependency-free JSON for CLIs and diagnostics.
    #[must_use]
    pub fn to_json(self) -> String {
        format!(
            concat!(
                r#"{{"status":"{}","reason":"{}","requested_feature":"{}","#,
                r#""required_environment":"{}","expected_version":"{}","upstream_commit":"{}"}}"#
            ),
            self.status,
            self.reason,
            self.requested_feature,
            self.required_environment,
            self.expected_version,
            self.upstream_commit,
        )
    }
}

/// Availability of the linked Steam Audio SDK at this build boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendAvailability {
    Available {
        version: &'static str,
        upstream_commit: &'static str,
    },
    Unavailable(UnavailableMetadata),
}

impl BackendAvailability {
    /// Stable, dependency-free JSON for capability negotiation.
    #[must_use]
    pub fn to_json(self) -> String {
        match self {
            Self::Available {
                version,
                upstream_commit,
            } => format!(
                r#"{{"status":"available","version":"{version}","upstream_commit":"{upstream_commit}"}}"#
            ),
            Self::Unavailable(metadata) => metadata.to_json(),
        }
    }
}

/// Reports SDK capability without probing arbitrary system locations or downloading anything.
#[must_use]
pub const fn backend_availability() -> BackendAvailability {
    #[cfg(feature = "linked-sdk")]
    {
        BackendAvailability::Available {
            version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
        }
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        BackendAvailability::Unavailable(unavailable_metadata())
    }
}

#[cfg(not(feature = "linked-sdk"))]
const fn unavailable_metadata() -> UnavailableMetadata {
    UnavailableMetadata {
        status: "unavailable",
        reason: "linked-sdk feature is disabled",
        requested_feature: "linked-sdk",
        required_environment: "STEAM_AUDIO_SDK_DIR",
        expected_version: STEAM_AUDIO_VERSION,
        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
    }
}

/// Errors produced while creating a Steam Audio context through the audited boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextError {
    SdkUnavailable(UnavailableMetadata),
    CreateFailed { status: i32 },
}

impl fmt::Display for ContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SdkUnavailable(metadata) => {
                write!(formatter, "Steam Audio SDK {}", metadata.reason)
            }
            Self::CreateFailed { status } => {
                write!(formatter, "iplContextCreate failed with status {status}")
            }
        }
    }
}

impl std::error::Error for ContextError {}

/// Errors returned by the owned Phase A backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    SdkUnavailable(UnavailableMetadata),
    InvalidInput(&'static str),
    SdkCall { function: &'static str, status: i32 },
    ProbeGenerationProducedNoProbes,
    PathBakeProducedNoData,
    EmptySerializedProbeBatch,
    InvalidProbeBatch(&'static str),
    InvalidSdkOutput(&'static str),
    NonFiniteOutput { output: &'static str },
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SdkUnavailable(metadata) => {
                write!(formatter, "Steam Audio SDK {}", metadata.reason)
            }
            Self::InvalidInput(message) => write!(formatter, "invalid Phase A input: {message}"),
            Self::SdkCall { function, status } => {
                write!(formatter, "{function} failed with status {status}")
            }
            Self::ProbeGenerationProducedNoProbes => write!(
                formatter,
                "iplProbeArrayGenerateProbes produced zero probes; uniform-floor generation requires solid floor geometry inside the probe volume"
            ),
            Self::PathBakeProducedNoData => write!(
                formatter,
                "iplPathBakerBake returned without a non-empty PATHING/DYNAMIC baked-data layer"
            ),
            Self::EmptySerializedProbeBatch => {
                write!(formatter, "iplProbeBatchSave produced an empty byte buffer")
            }
            Self::InvalidProbeBatch(message) => {
                write!(formatter, "invalid serialized probe batch: {message}")
            }
            Self::InvalidSdkOutput(message) => {
                write!(
                    formatter,
                    "invalid Steam Audio simulation output: {message}"
                )
            }
            Self::NonFiniteOutput { output } => {
                write!(formatter, "Steam Audio produced non-finite {output} data")
            }
        }
    }
}

impl std::error::Error for BackendError {}

/// Global audio settings used for Phase A effects and simulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate_hz: i32,
    pub frame_size: i32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            frame_size: 512,
        }
    }
}

/// Listener position and orientation in local ENU coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerPose {
    pub position_enu: EnuVector3,
    pub ahead_enu: EnuVector3,
    pub up_enu: EnuVector3,
}

impl ListenerPose {
    #[must_use]
    pub const fn at(position_enu: EnuVector3) -> Self {
        Self {
            position_enu,
            ahead_enu: EnuVector3::new(0.0, 1.0, 0.0),
            up_enu: EnuVector3::new(0.0, 0.0, 1.0),
        }
    }
}

/// Owned calibrated input for the S0 direct/binaural render.
#[derive(Clone, Debug, PartialEq)]
pub struct S0RenderRequest {
    pub audio: AudioConfig,
    pub source_position_enu: EnuVector3,
    pub listener: ListenerPose,
    /// Mono, finite, normalized floating-point PCM.
    pub input_mono: Vec<f32>,
    /// Linear gain that applies the caller's source calibration to actual samples.
    pub calibration_gain: f32,
    /// Whether to include Steam Audio's default three-band air-absorption model.
    pub apply_air_absorption: bool,
}

/// Rust-owned interleaved stereo PCM.
#[derive(Clone, Debug, PartialEq)]
pub struct OwnedStereoPcm {
    pub sample_rate_hz: i32,
    pub frame_count: usize,
    pub interleaved: Vec<f32>,
}

impl OwnedStereoPcm {
    #[must_use]
    pub fn is_finite(&self) -> bool {
        self.interleaved.iter().all(|sample| sample.is_finite())
    }
}

/// S0 output and the physical attenuation values applied to its PCM.
#[derive(Clone, Debug, PartialEq)]
pub struct S0RenderOutput {
    pub stereo: OwnedStereoPcm,
    pub distance_attenuation: f32,
    pub air_absorption: [f32; 3],
    pub relative_direction_steam: SteamVector3,
}

/// Acoustic properties for one material in a Phase A static mesh.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AcousticMaterial {
    pub absorption: [f32; 3],
    pub scattering: f32,
    pub transmission: [f32; 3],
}

impl AcousticMaterial {
    pub const MASONRY: Self = Self {
        absorption: [0.03, 0.05, 0.07],
        scattering: 0.1,
        transmission: [0.0, 0.0, 0.0],
    };

    pub const GROUND: Self = Self {
        absorption: [0.05, 0.07, 0.08],
        scattering: 0.05,
        transmission: [0.0, 0.0, 0.0],
    };
}

/// Ordinary owned geometry used to construct an `IPL_SCENETYPE_DEFAULT` scene and static mesh.
#[derive(Clone, Debug, PartialEq)]
pub struct SceneMesh {
    pub vertices_enu_m: Vec<EnuVector3>,
    pub triangles: Vec<[i32; 3]>,
    pub material_indices: Vec<i32>,
    pub materials: Vec<AcousticMaterial>,
}

impl SceneMesh {
    /// Small L-corner plus a solid floor for backend unit and lifetime tests.
    ///
    /// This is a convenience mesh, not the canonical S3 fixture and not acceptance evidence.
    /// Production Phase A callers must pass the exact accepted fixture geometry through
    /// [`S3BakeRequest::mesh`] and [`S3RenderRequest::mesh`]. Keeping a solid floor explicit is
    /// essential: 4.8.1 casts downward rays against scene geometry and otherwise generates zero
    /// probes.
    #[must_use]
    pub fn controlled_s3_corner() -> Self {
        let vertices_enu_m = vec![
            EnuVector3::new(-10.0, 0.0, 0.0),
            EnuVector3::new(0.0, 0.0, 0.0),
            EnuVector3::new(0.0, 0.0, 6.0),
            EnuVector3::new(-10.0, 0.0, 6.0),
            EnuVector3::new(0.0, -10.0, 0.0),
            EnuVector3::new(0.0, -10.0, 6.0),
            EnuVector3::new(-10.0, -10.0, 0.0),
            EnuVector3::new(8.0, -10.0, 0.0),
            EnuVector3::new(8.0, 10.0, 0.0),
            EnuVector3::new(-10.0, 10.0, 0.0),
        ];
        let triangles = vec![
            [0, 1, 2],
            [0, 2, 3],
            [2, 1, 0],
            [3, 2, 0],
            [4, 1, 2],
            [4, 2, 5],
            [2, 1, 4],
            [5, 2, 4],
            [6, 7, 8],
            [6, 8, 9],
            [8, 7, 6],
            [9, 8, 6],
        ];
        Self {
            vertices_enu_m,
            material_indices: vec![0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1],
            triangles,
            materials: vec![AcousticMaterial::MASONRY, AcousticMaterial::GROUND],
        }
    }
}

/// Axis-aligned ENU volume used for uniform-floor probe generation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProbeVolume {
    pub min_enu_m: EnuVector3,
    pub max_enu_m: EnuVector3,
    pub spacing_m: f32,
    pub height_above_floor_m: f32,
}

impl Default for ProbeVolume {
    fn default() -> Self {
        Self {
            min_enu_m: EnuVector3::new(-9.0, -9.0, 0.0),
            max_enu_m: EnuVector3::new(7.0, 9.0, 3.0),
            spacing_m: 2.0,
            height_above_floor_m: 1.5,
        }
    }
}

/// Settings passed to the real Steam Audio path baker.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathBakeConfig {
    pub num_visibility_samples: i32,
    pub probe_visibility_radius_m: f32,
    pub visibility_threshold: f32,
    pub visibility_range_m: f32,
    pub path_range_m: f32,
    pub num_threads: i32,
}

impl Default for PathBakeConfig {
    fn default() -> Self {
        Self {
            num_visibility_samples: 1,
            probe_visibility_radius_m: 0.0,
            visibility_threshold: 0.5,
            visibility_range_m: 6.0,
            path_range_m: 100.0,
            num_threads: 1,
        }
    }
}

/// Owned request for the process-independent S3 bake operation.
#[derive(Clone, Debug, PartialEq)]
pub struct S3BakeRequest {
    pub mesh: SceneMesh,
    pub probes: ProbeVolume,
    pub pathing: PathBakeConfig,
}

impl Default for S3BakeRequest {
    fn default() -> Self {
        Self {
            mesh: SceneMesh::controlled_s3_corner(),
            probes: ProbeVolume::default(),
            pathing: PathBakeConfig {
                num_visibility_samples: 1,
                probe_visibility_radius_m: 1.0,
                visibility_threshold: 0.1,
                visibility_range_m: 1_000.0,
                path_range_m: 100.0,
                num_threads: 1,
            },
        }
    }
}

/// Stable metadata for an SDK-owned serialization copied into Rust memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeBatchMetadata {
    pub schema_version: &'static str,
    pub steam_audio_version: &'static str,
    pub upstream_commit: &'static str,
    pub probe_count: u32,
    pub path_data_size_bytes: u64,
    pub serialized_size_bytes: u64,
    pub content_sha256: String,
    pub bake_progress_callback_count: u32,
    /// Final callback value, scaled so `1_000_000` means 100 percent.
    pub final_bake_progress_millionths: u32,
}

impl ProbeBatchMetadata {
    /// Deterministic dependency-free JSON suitable for an evidence sidecar.
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            concat!(
                r#"{{"schema_version":"{}","steam_audio_version":"{}","#,
                r#""upstream_commit":"{}","probe_count":{},"path_data_size_bytes":{},"#,
                r#""serialized_size_bytes":{},"content_sha256":"{}","#,
                r#""bake_progress_callback_count":{},"final_bake_progress_millionths":{}}}"#
            ),
            self.schema_version,
            self.steam_audio_version,
            self.upstream_commit,
            self.probe_count,
            self.path_data_size_bytes,
            self.serialized_size_bytes,
            self.content_sha256,
            self.bake_progress_callback_count,
            self.final_bake_progress_millionths,
        )
    }
}

/// The complete process-safe output of an S3 bake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BakedProbeBatch {
    pub metadata: ProbeBatchMetadata,
    /// Exact bytes returned by `iplProbeBatchSave`.
    pub bytes: Vec<u8>,
}

impl BakedProbeBatch {
    /// Verifies stable metadata before any byte is handed to Steam Audio.
    pub fn validate(&self) -> Result<(), BackendError> {
        if self.bytes.is_empty() {
            return Err(BackendError::InvalidProbeBatch("byte buffer is empty"));
        }
        if self.metadata.schema_version != PROBE_BATCH_METADATA_SCHEMA {
            return Err(BackendError::InvalidProbeBatch(
                "metadata schema version is not supported",
            ));
        }
        if self.metadata.steam_audio_version != STEAM_AUDIO_VERSION
            || self.metadata.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
        {
            return Err(BackendError::InvalidProbeBatch(
                "Steam Audio version provenance does not match this backend",
            ));
        }
        if self.metadata.probe_count == 0 || self.metadata.path_data_size_bytes == 0 {
            return Err(BackendError::InvalidProbeBatch(
                "metadata does not establish probes and a baked path layer",
            ));
        }
        if self.metadata.serialized_size_bytes != self.bytes.len() as u64 {
            return Err(BackendError::InvalidProbeBatch(
                "serialized byte length does not match metadata",
            ));
        }
        if self.metadata.content_sha256 != sha256_hex(&self.bytes) {
            return Err(BackendError::InvalidProbeBatch(
                "serialized content hash does not match metadata",
            ));
        }
        Ok(())
    }
}

/// Tunable but bounded Phase A simulator settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectionEffectType {
    Convolution,
    Parametric,
    Hybrid,
    /// AMD TrueAudio Next GPU convolution. Explicitly unsupported by the
    /// Phase A CPU/macOS backend and rejected during validation.
    TrueAudioNext,
}

impl ReflectionEffectType {
    /// Exact Steam Audio 4.8.1 discriminant for supported CPU modes.
    pub fn steam_audio_cpu_discriminant(self) -> Result<i32, BackendError> {
        match self {
            Self::Convolution => Ok(0),
            Self::Parametric => Ok(1),
            Self::Hybrid => Ok(2),
            Self::TrueAudioNext => Err(BackendError::InvalidInput(
                "TrueAudio Next requires unsupported GPU/TAN devices and is not part of the Phase A CPU sweep",
            )),
        }
    }
}

/// Strongly typed reflection algorithm and hybrid transition contract.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReflectionEffectConfig {
    pub effect_type: ReflectionEffectType,
    /// Hybrid convolution length in seconds. Must be `Some` only for hybrid.
    pub hybrid_transition_time_s: Option<f32>,
    /// Hybrid convolution/parametric crossfade fraction in `[0, 1)`.
    /// Must be `Some` only for hybrid.
    pub hybrid_overlap_percent: Option<f32>,
}

impl ReflectionEffectConfig {
    pub const CONVOLUTION: Self = Self {
        effect_type: ReflectionEffectType::Convolution,
        hybrid_transition_time_s: None,
        hybrid_overlap_percent: None,
    };

    pub const PARAMETRIC: Self = Self {
        effect_type: ReflectionEffectType::Parametric,
        hybrid_transition_time_s: None,
        hybrid_overlap_percent: None,
    };

    #[must_use]
    pub const fn hybrid(transition_time_s: f32, overlap_percent: f32) -> Self {
        Self {
            effect_type: ReflectionEffectType::Hybrid,
            hybrid_transition_time_s: Some(transition_time_s),
            hybrid_overlap_percent: Some(overlap_percent),
        }
    }

    /// An explicit request which validation rejects on this CPU/macOS backend.
    pub const TRUE_AUDIO_NEXT_UNSUPPORTED: Self = Self {
        effect_type: ReflectionEffectType::TrueAudioNext,
        hybrid_transition_time_s: None,
        hybrid_overlap_percent: None,
    };
}

impl Default for ReflectionEffectConfig {
    fn default() -> Self {
        Self::CONVOLUTION
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DirectOcclusionMode {
    /// One binary listener-to-source ray. Radius and sample count do not apply.
    Raycast,
    /// Samples the source sphere visible from the listener.
    Volumetric { radius_m: f32, sample_count: i32 },
}

impl DirectOcclusionMode {
    #[must_use]
    pub const fn steam_audio_discriminant(self) -> i32 {
        match self {
            Self::Raycast => 0,
            Self::Volumetric { .. } => 1,
        }
    }
}

impl Default for DirectOcclusionMode {
    fn default() -> Self {
        Self::Raycast
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct S3SimulationConfig {
    /// Capacity reserved by `IPLSimulationSettings::maxNumOcclusionSamples`.
    pub max_occlusion_samples: i32,
    /// Exact Steam Audio direct-occlusion algorithm and its applicable parameters.
    ///
    /// Raycast has no quality sample count. Volumetric's sample count must not
    /// exceed [`Self::max_occlusion_samples`].
    pub direct_occlusion: DirectOcclusionMode,
    pub reflection_rays: i32,
    pub diffuse_samples: i32,
    pub reflection_bounces: i32,
    pub reflection_duration_s: f32,
    pub reflection_order: i32,
    /// Canonical S3 remains convolution. Parametric and hybrid are explicit
    /// CPU sweep variants; TAN is rejected rather than remapped.
    pub reflection_effect: ReflectionEffectConfig,
    pub simulation_threads: i32,
    pub ray_batch_size: i32,
    /// Runtime Ambisonic order used to represent path directionality.
    ///
    /// Steam Audio 4.8.1's `IPLPathBakeParams` has no order field. A fixture's
    /// `path_bake.bake_order` therefore describes this runtime decode order; it is
    /// not a separate path-baker setting.
    pub pathing_order: i32,
    pub pathing_visibility_samples: i32,
    pub pathing_visibility_radius_m: f32,
    pub pathing_visibility_threshold: f32,
    pub pathing_visibility_range_m: f32,
    pub validate_paths: bool,
    pub find_alternate_paths: bool,
    /// Collect probe-segment observations through the SDK's synchronous path
    /// visualization callback. Intended for evidence and diagnostics, not the
    /// steady-state simulation hot path.
    pub trace_path_validation: bool,
}

/// One logical emitter in a retained Steam Audio world.
///
/// Descriptor order is the stable `source_index` used by the frozen runtime
/// seam. The initial position seeds simulation before the first
/// [`fightbox_runtime::backend::SimulationRunner::update_inputs`] call.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MultiSourceDescriptor {
    pub initial_position_enu: fightbox_api::EnuVector3,
    reference_level: fightbox_api::ReferenceLevel,
}

impl MultiSourceDescriptor {
    #[must_use]
    pub const fn at(initial_position_enu: fightbox_api::EnuVector3) -> Self {
        Self {
            initial_position_enu,
            reference_level: fightbox_api::ReferenceLevel::CreativeDb { db: 0.0 },
        }
    }

    /// Attaches the declared source level used for audibility ranking.
    ///
    /// Only `SplAtOneMeter` permits the governor to compare the prediction
    /// with the hearing threshold and virtualize a source. `CreativeDb`
    /// remains useful for relative ranking but can only select direct-only LOD.
    #[must_use]
    pub const fn with_reference_level(
        mut self,
        reference_level: fightbox_api::ReferenceLevel,
    ) -> Self {
        self.reference_level = reference_level;
        self
    }

    #[cfg(any(feature = "linked-sdk", test))]
    pub(crate) const fn declared_level_db(self) -> f32 {
        match self.reference_level {
            fightbox_api::ReferenceLevel::CreativeDb { db } => db,
            fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl } => db_spl,
        }
    }

    #[cfg(any(feature = "linked-sdk", test))]
    pub(crate) const fn is_physically_calibrated(self) -> bool {
        matches!(
            self.reference_level,
            fightbox_api::ReferenceLevel::SplAtOneMeter { .. }
        )
    }
}

/// Output gains for the three independently rendered Steam Audio stages.
///
/// Unity preserves the existing summed-output behavior. A live diagnostic UI
/// can publish a complete value through [`StageOutputGainControl`]; the render
/// graph adopts one complete snapshot at the next block boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageOutputGains {
    pub direct: f32,
    pub pathing: f32,
    pub reflections: f32,
}

impl StageOutputGains {
    pub const UNITY: Self = Self {
        direct: 1.0,
        pathing: 1.0,
        reflections: 1.0,
    };

    fn is_valid(self) -> bool {
        [self.direct, self.pathing, self.reflections]
            .into_iter()
            .all(|gain| gain.is_finite() && gain >= 0.0)
    }
}

/// Producer handle for block-atomic stage output-gain snapshots.
pub struct StageOutputGainControl {
    writer: fightbox_runtime::SnapshotWriter<StageOutputGains>,
}

impl StageOutputGainControl {
    /// Publishes all three stage gains as one immutable snapshot.
    ///
    /// Invalid (negative or non-finite) gains are rejected without changing the
    /// active render snapshot.
    pub fn publish(&mut self, gains: StageOutputGains) -> Result<(), InvalidStageOutputGains> {
        if !gains.is_valid() {
            return Err(InvalidStageOutputGains);
        }
        self.writer.publish(gains);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidStageOutputGains;

/// Reflection implementation present in one prepared world generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorldReflectionState {
    RealtimeConvolution,
    RealtimeParametric,
    RealtimeHybrid,
    UnsupportedTrueAudioNext,
}

impl WorldReflectionState {
    const fn from_effect(effect: ReflectionEffectType) -> Self {
        match effect {
            ReflectionEffectType::Convolution => Self::RealtimeConvolution,
            ReflectionEffectType::Parametric => Self::RealtimeParametric,
            ReflectionEffectType::Hybrid => Self::RealtimeHybrid,
            ReflectionEffectType::TrueAudioNext => Self::UnsupportedTrueAudioNext,
        }
    }
}

/// Capabilities owned by one complete world generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedWorldCapabilities {
    pub generation: u64,
    /// True only when a validated serialized probe batch with baked path data
    /// was loaded into this generation.
    pub baked_pathing: bool,
    pub reflections: WorldReflectionState,
}

/// Render-thread adoption state observed without consulting SDK handles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveredWorldState {
    pub capabilities: PreparedWorldCapabilities,
    /// Old and new graphs are both rendered while this count is nonzero.
    pub transition_blocks_remaining: u8,
}

/// Rust-owned proof values copied from the latest simulation snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldGenerationDiagnostics {
    pub generation: u64,
    /// Prefix of the validated bake content hash; zero when pathing is absent.
    pub baked_data_fingerprint: u64,
    pub path_eq: [f32; 3],
    pub path_sh_energy: f32,
    pub reflection_reverb_times: [f32; 3],
    pub reflection_ir_size: i32,
}

/// A fully constructed second world which has not yet been offered to the
/// callback. It can be primed with simulation updates before adoption.
pub struct PreparedSteamAudioWorld {
    #[cfg(feature = "linked-sdk")]
    inner: linked::PreparedMultiSourceWorld,
    stage_output_gain_control: Option<StageOutputGainControl>,
    #[cfg(not(feature = "linked-sdk"))]
    _private: (),
}

impl PreparedSteamAudioWorld {
    #[must_use]
    pub fn capabilities(&self) -> PreparedWorldCapabilities {
        #[cfg(feature = "linked-sdk")]
        return self.inner.capabilities();
        #[cfg(not(feature = "linked-sdk"))]
        unreachable!("an SDK-unavailable build cannot construct a prepared world")
    }

    #[must_use]
    pub fn diagnostics(&self) -> WorldGenerationDiagnostics {
        #[cfg(feature = "linked-sdk")]
        return self.inner.diagnostics();
        #[cfg(not(feature = "linked-sdk"))]
        unreachable!("an SDK-unavailable build cannot construct a prepared world")
    }

    /// Primes the prepared generation's governor off-callback before adoption.
    pub fn observe_render_timing(&mut self, elapsed_ns: u64) {
        #[cfg(feature = "linked-sdk")]
        self.inner.observe_render_timing(elapsed_ns);
        #[cfg(not(feature = "linked-sdk"))]
        let _ = elapsed_ns;
    }

    #[must_use]
    pub fn quality_governor_telemetry(&self) -> Option<QualityGovernorTelemetry> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.quality_governor_telemetry();
        #[cfg(not(feature = "linked-sdk"))]
        None
    }
}

impl fightbox_runtime::backend::SimulationRunner for PreparedSteamAudioWorld {
    fn update_inputs(&mut self, update: &fightbox_runtime::backend::SimulationUpdate) {
        #[cfg(feature = "linked-sdk")]
        self.inner.update_inputs(update);
        #[cfg(not(feature = "linked-sdk"))]
        let _ = update;
    }

    fn run_direct(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_direct();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }

    fn run_pathing(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_pathing();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }

    fn run_reflections(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_reflections();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreparedWorldSwapError {
    AdoptionPending,
}

/// Successful control-side publication of a prepared world.
pub struct PreparedWorldSwapReceipt {
    pub generation: u64,
    /// Stage-gain snapshots are generation-local. This producer controls the
    /// newly adopted graph; the preceding generation's producer may be dropped.
    pub stage_output_gain_control: StageOutputGainControl,
}

/// Simulation half of a retained multi-source Steam Audio session.
pub struct SteamAudioSimulationRunner {
    #[cfg(feature = "linked-sdk")]
    inner: linked::MultiSourceSimulation,
    #[cfg(not(feature = "linked-sdk"))]
    _private: (),
}

impl SteamAudioSimulationRunner {
    /// Feeds one already-measured callback/block duration to the control-side governor.
    ///
    /// Call this outside the audio callback. A quality change, when needed, is
    /// published as one immutable snapshot for adoption at a later block boundary.
    pub fn observe_render_timing(&mut self, elapsed_ns: u64) {
        #[cfg(feature = "linked-sdk")]
        self.inner.observe_render_timing(elapsed_ns);
        #[cfg(not(feature = "linked-sdk"))]
        let _ = elapsed_ns;
    }

    /// Records scheduling lateness observed by an external simulation worker.
    pub fn observe_simulation_lateness(&mut self, pass: GovernorSimulationPass, lateness_ns: u64) {
        #[cfg(feature = "linked-sdk")]
        self.inner.observe_simulation_lateness(pass, lateness_ns);
        #[cfg(not(feature = "linked-sdk"))]
        let _ = (pass, lateness_ns);
    }

    /// Returns the latest delivered-quality and governor timing telemetry.
    ///
    /// An unbaked generation returns `None`: the legacy governor path enum has
    /// no "absent" value, so returning a path quality would be a capability lie.
    #[must_use]
    pub fn quality_governor_telemetry(&self) -> Option<QualityGovernorTelemetry> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.quality_governor_telemetry();
        #[cfg(not(feature = "linked-sdk"))]
        None
    }

    /// Constructs a complete second scene/simulator/effect graph while the
    /// current render graph remains usable.
    ///
    /// `baked = None` creates committed empty probe state and truthfully
    /// disables path rendering for that generation.
    pub fn prepare_world(
        &mut self,
        mesh: &SceneMesh,
        baked: Option<&BakedProbeBatch>,
        simulation: S3SimulationConfig,
        sources: &[MultiSourceDescriptor],
    ) -> Result<PreparedSteamAudioWorld, BackendError> {
        #[cfg(feature = "linked-sdk")]
        {
            let mut inner = self.inner.prepare_world(mesh, baked, simulation, sources)?;
            let writer = inner
                .take_stage_output_gain_writer()
                .expect("a freshly prepared graph owns its stage-gain producer");
            return Ok(PreparedSteamAudioWorld {
                inner,
                stage_output_gain_control: Some(StageOutputGainControl { writer }),
            });
        }
        #[cfg(not(feature = "linked-sdk"))]
        {
            let _ = (mesh, baked, simulation, sources);
            Err(BackendError::SdkUnavailable(unavailable_metadata()))
        }
    }

    /// Offers a fully prepared generation to the render thread.
    ///
    /// The callback adopts it at a later block boundary and crossfades the
    /// summed output. This call never waits for the callback.
    pub fn swap_prepared_world(
        &mut self,
        mut prepared: PreparedSteamAudioWorld,
    ) -> Result<PreparedWorldSwapReceipt, PreparedWorldSwapError> {
        #[cfg(feature = "linked-sdk")]
        {
            let generation = prepared.inner.capabilities().generation;
            self.inner.swap_prepared_world(prepared.inner)?;
            return Ok(PreparedWorldSwapReceipt {
                generation,
                stage_output_gain_control: prepared
                    .stage_output_gain_control
                    .take()
                    .expect("prepared world retained its stage-gain producer"),
            });
        }
        #[cfg(not(feature = "linked-sdk"))]
        {
            let _ = prepared;
            Err(PreparedWorldSwapError::AdoptionPending)
        }
    }

    /// Returns the generation and capabilities actually adopted by the render
    /// thread, including whether its output crossfade is still in progress.
    #[must_use]
    pub fn delivered_world_state(&self) -> DeliveredWorldState {
        #[cfg(feature = "linked-sdk")]
        return self.inner.delivered_world_state();
        #[cfg(not(feature = "linked-sdk"))]
        unreachable!("an SDK-unavailable build cannot own a live session")
    }

    /// Copies proof values from the control-side snapshot of the current
    /// generation. No SDK pointer is exposed.
    #[must_use]
    pub fn world_diagnostics(&self) -> WorldGenerationDiagnostics {
        #[cfg(feature = "linked-sdk")]
        return self.inner.diagnostics();
        #[cfg(not(feature = "linked-sdk"))]
        unreachable!("an SDK-unavailable build cannot own a live session")
    }
}

/// Audio-thread half of a retained multi-source Steam Audio session.
pub struct SteamAudioRenderGraph {
    #[cfg(feature = "linked-sdk")]
    inner: linked::MultiSourceRenderGraph,
    stage_output_gain_control: Option<StageOutputGainControl>,
    #[cfg(not(feature = "linked-sdk"))]
    _private: (),
}

impl SteamAudioRenderGraph {
    /// Takes the unique producer handle for this graph's stage output gains.
    ///
    /// Existing callers need not use this accessor: all stages default to
    /// unity, and taking the handle does not itself change any gain.
    pub fn take_stage_output_gain_control(&mut self) -> Option<StageOutputGainControl> {
        self.stage_output_gain_control.take()
    }
}

impl fightbox_runtime::backend::SimulationRunner for SteamAudioSimulationRunner {
    fn update_inputs(&mut self, update: &fightbox_runtime::backend::SimulationUpdate) {
        #[cfg(feature = "linked-sdk")]
        self.inner.update_inputs(update);
        #[cfg(not(feature = "linked-sdk"))]
        let _ = update;
    }

    fn run_direct(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_direct();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }

    fn run_pathing(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_pathing();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }

    fn run_reflections(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.run_reflections();
        #[cfg(not(feature = "linked-sdk"))]
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    }
}

impl fightbox_runtime::backend::BackendRenderGraph for SteamAudioRenderGraph {
    fn render_block(
        &mut self,
        block: fightbox_runtime::backend::PropagationRenderBlock<'_>,
    ) -> Result<(), fightbox_runtime::backend::BackendRenderError> {
        #[cfg(feature = "linked-sdk")]
        return self.inner.render_block(block);
        #[cfg(not(feature = "linked-sdk"))]
        {
            let _ = block;
            Err(fightbox_runtime::backend::BackendRenderError::InactiveGraph)
        }
    }
}

impl Default for S3SimulationConfig {
    fn default() -> Self {
        Self {
            max_occlusion_samples: 64,
            direct_occlusion: DirectOcclusionMode::Raycast,
            reflection_rays: 1_024,
            diffuse_samples: 32,
            reflection_bounces: 2,
            reflection_duration_s: 1.0,
            reflection_order: 1,
            reflection_effect: ReflectionEffectConfig::CONVOLUTION,
            simulation_threads: 1,
            ray_batch_size: 64,
            pathing_order: 2,
            pathing_visibility_samples: 1,
            pathing_visibility_radius_m: 0.0,
            pathing_visibility_threshold: 0.5,
            pathing_visibility_range_m: 6.0,
            validate_paths: true,
            find_alternate_paths: true,
            trace_path_validation: false,
        }
    }
}

/// Owned input for a fresh-process S3 load, simulation, and stem render.
#[derive(Clone, Debug, PartialEq)]
pub struct S3RenderRequest {
    pub mesh: SceneMesh,
    pub audio: AudioConfig,
    pub simulation: S3SimulationConfig,
    pub source_position_enu: EnuVector3,
    pub listener: ListenerPose,
    pub input_mono: Vec<f32>,
    pub calibration_gain: f32,
}

impl S3RenderRequest {
    #[must_use]
    pub fn controlled_default(input_mono: Vec<f32>) -> Self {
        Self {
            mesh: SceneMesh::controlled_s3_corner(),
            audio: AudioConfig::default(),
            simulation: S3SimulationConfig::default(),
            source_position_enu: EnuVector3::new(-6.0, -4.0, 1.5),
            listener: ListenerPose::at(EnuVector3::new(4.0, 6.0, 1.5)),
            input_mono,
            calibration_gain: 1.0,
        }
    }
}

/// Rust-owned copy of Steam Audio's direct simulation results.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectSnapshot {
    pub distance_attenuation: f32,
    pub air_absorption: [f32; 3],
    pub directivity: f32,
    pub occlusion: f32,
    pub transmission: [f32; 3],
    pub requested_occlusion_mode: DirectOcclusionMode,
    /// Successfully configured Steam Audio algorithm. No fallback is performed.
    pub delivered_occlusion_mode: DirectOcclusionMode,
}

#[cfg(any(feature = "linked-sdk", test))]
pub(crate) fn validate_direct_snapshot(snapshot: &DirectSnapshot) -> Result<(), BackendError> {
    if !snapshot.distance_attenuation.is_finite()
        || !snapshot.air_absorption.into_iter().all(f32::is_finite)
        || !snapshot.directivity.is_finite()
        || !snapshot.occlusion.is_finite()
        || !snapshot.transmission.into_iter().all(f32::is_finite)
    {
        return Err(BackendError::NonFiniteOutput {
            output: "direct simulation",
        });
    }
    if !(0.0..=1.0).contains(&snapshot.occlusion) {
        return Err(BackendError::InvalidSdkOutput(
            "direct occlusion is outside the Steam Audio fraction range",
        ));
    }
    Ok(())
}

/// Directional first-order moment decoded from Steam Audio path SH coefficients.
///
/// Steam Audio 4.8.1 projects every valid path's virtual-source direction and
/// sums those projections using path weight times distance attenuation. This is
/// therefore a normalized gain-weighted mean arrival direction, not proof of a
/// unique or dominant path.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathDirectionEstimate {
    pub mean_arrival_direction_enu: EnuVector3,
    pub azimuth_degrees_clockwise_from_north: f32,
    /// Euclidean magnitude of the three first-order coefficients before normalization.
    pub first_order_magnitude: f32,
    pub zeroth_order_coefficient: f32,
}

/// One probe-to-probe visibility check performed while validating a baked path.
///
/// Steam Audio's visualization callback reports only segments that belong to a
/// baked path considered at runtime. Endpoint-to-probe neighborhood visibility
/// is handled separately inside the SDK and is not included here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathValidationSegment {
    pub from_enu_m: EnuVector3,
    pub to_enu_m: EnuVector3,
    pub occluded: bool,
}

/// Rust-owned copy of pathing output and validated configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct PathSnapshot {
    pub eq_coeffs: [f32; 3],
    pub sh_coeffs: Vec<f32>,
    pub configured_order: i32,
    /// `None` for order zero or a zero first-order directional moment.
    pub direction: Option<PathDirectionEstimate>,
    /// Exact SDK validation callback observations for this simulation run.
    ///
    /// Empty when path validation is disabled or no baked path reached
    /// validation. A non-empty trace does not by itself establish a valid path:
    /// inspect both [`Self::direction`] and each segment's `occluded` flag.
    pub validation_segments: Vec<PathValidationSegment>,
}

/// Decodes the first-order directional moment emitted by Steam Audio 4.8.1 pathing.
///
/// The pinned implementation stores coefficients in `(l, m)` order, so indices
/// 1, 2, and 3 are `(1,-1)`, `(1,0)`, and `(1,1)`. Its Google SH basis evaluates
/// those as `0.488603 * (d.y, d.z, d.x)` after converting a Steam direction to
/// `(-steam.z, -steam.x, steam.y)`. Undoing that transform and then applying this
/// crate's Steam-to-ENU rotation yields an ENU moment proportional to
/// `(-coeff[1], coeff[3], coeff[2])`.
///
/// Source audit: Valve Steam Audio tag `v4.8.1` (`0da1825`),
/// `core/src/core/path_simulator.cpp::calcAmbisonicsCoeffsForPaths`,
/// `core/src/core/sh.cpp::convertedDirection`, and
/// `core/src/core/sh/spherical_harmonics.cc::HardcodedSH1*`.
pub fn decode_path_direction_enu(
    configured_order: i32,
    sh_coeffs: &[f32],
) -> Result<Option<PathDirectionEstimate>, BackendError> {
    if !(0..=3).contains(&configured_order) {
        return Err(BackendError::InvalidInput(
            "Phase A pathing order must be between zero and three",
        ));
    }
    let expected = ((configured_order + 1) * (configured_order + 1)) as usize;
    if sh_coeffs.len() != expected {
        return Err(BackendError::InvalidInput(
            "path SH coefficient count does not match configured order",
        ));
    }
    if !sh_coeffs.iter().copied().all(f32::is_finite) {
        return Err(BackendError::NonFiniteOutput {
            output: "path simulation",
        });
    }
    if configured_order == 0 {
        return Ok(None);
    }

    let moment = EnuVector3::new(-sh_coeffs[1], sh_coeffs[3], sh_coeffs[2]);
    let magnitude = (moment.x * moment.x + moment.y * moment.y + moment.z * moment.z).sqrt();
    if !magnitude.is_finite() {
        return Err(BackendError::NonFiniteOutput {
            output: "path direction",
        });
    }
    if magnitude <= f32::EPSILON {
        return Ok(None);
    }
    let direction = EnuVector3::new(
        moment.x / magnitude,
        moment.y / magnitude,
        moment.z / magnitude,
    );
    let azimuth = direction
        .x
        .atan2(direction.y)
        .to_degrees()
        .rem_euclid(360.0);
    Ok(Some(PathDirectionEstimate {
        mean_arrival_direction_enu: direction,
        azimuth_degrees_clockwise_from_north: azimuth,
        first_order_magnitude: magnitude,
        zeroth_order_coefficient: sh_coeffs[0],
    }))
}

/// Rust-owned reflection metadata. The opaque IR never crosses the API.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReflectionSnapshot {
    pub requested_effect_type: ReflectionEffectType,
    /// Successfully created and applied CPU effect type. Steam Audio does not
    /// write the type field in `IPLSimulationOutputs`, so this records the
    /// validated simulator/effect construction rather than reading an unset field.
    pub delivered_effect_type: ReflectionEffectType,
    pub num_channels: i32,
    /// Raw `outputs.reflections.numChannels` (`0` for parametric in 4.8.1).
    pub sdk_num_channels: i32,
    /// Exact `outputs.reflections.irSize` used by the reflection effect.
    ///
    /// This remains positive and exact for convolution/hybrid. It is `0` for
    /// parametric because that algorithm does not consume an IR.
    pub ir_size: i32,
    pub reverb_times: [f32; 3],
    pub eq: [f32; 3],
    pub delay_samples: i32,
    pub configured_hybrid_transition_time_s: Option<f32>,
    pub configured_hybrid_overlap_percent: Option<f32>,
    pub applied_reverb_times: Option<[f32; 3]>,
    pub applied_hybrid_eq: Option<[f32; 3]>,
    pub applied_hybrid_delay_samples: Option<i32>,
}

/// All simulation data copied while the SDK source owns the output generation.
#[derive(Clone, Debug, PartialEq)]
pub struct S3SimulationSnapshot {
    pub direct: DirectSnapshot,
    pub path: PathSnapshot,
    pub reflections: ReflectionSnapshot,
}

/// Separately captured stems and pathing toggle sums.
#[derive(Clone, Debug, PartialEq)]
pub struct S3Stems {
    pub direct: OwnedStereoPcm,
    pub path: OwnedStereoPcm,
    pub reflections: OwnedStereoPcm,
    pub pathing_on_sum: OwnedStereoPcm,
    pub pathing_off_sum: OwnedStereoPcm,
}

/// Fresh-handle S3 load/simulation/render output.
#[derive(Clone, Debug, PartialEq)]
pub struct S3RenderOutput {
    pub loaded_probe_count: u32,
    pub loaded_path_data_size_bytes: u64,
    pub snapshot: S3SimulationSnapshot,
    pub stems: S3Stems,
}

/// Offline block trajectory rendered by one retained Steam Audio session.
///
/// `base.input_mono` must contain exactly one [`AudioConfig::frame_size`] block
/// per listener pose. The implementation advances the input and listener pose
/// together without recreating the loaded scene, simulator, source, HRTF, or
/// effects. This is deterministic evidence rendering, not a live/audio-thread API.
#[derive(Clone, Debug, PartialEq)]
pub struct S3TrajectoryRenderRequest {
    pub base: S3RenderRequest,
    pub listener_trajectory: Vec<ListenerPose>,
}

/// Construction counts proving which SDK state was retained for a trajectory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3RetainedSessionStats {
    pub context_generations: u32,
    pub scene_generations: u32,
    pub probe_batch_loads: u32,
    pub simulator_generations: u32,
    pub source_generations: u32,
    pub hrtf_generations: u32,
    pub effect_graph_generations: u32,
    /// Total effect-block groups executed, including declared warmups.
    pub rendered_blocks: u32,
}

/// Public safety ceilings enforced only by [`benchmark_s3_stages`].
pub const S3_BENCHMARK_MAX_STANDARD_ITERATIONS: u32 = 4_096;
pub const S3_BENCHMARK_MAX_REFLECTION_ITERATIONS: u32 = 512;
pub const S3_BENCHMARK_MAX_OCCLUSION_SAMPLES: i32 = 1_024;
pub const S3_BENCHMARK_MAX_REFLECTION_RAYS: i32 = 131_072;
pub const S3_BENCHMARK_MAX_DIFFUSE_SAMPLES: i32 = 4_096;
pub const S3_BENCHMARK_MAX_REFLECTION_BOUNCES: i32 = 64;
pub const S3_BENCHMARK_MAX_SIMULATION_THREADS: i32 = 32;
pub const S3_BENCHMARK_MAX_RAY_BATCH_SIZE: i32 = 4_096;
/// Maximum reflection IR capacity per channel: ten seconds at 48 kHz.
pub const S3_BENCHMARK_MAX_REFLECTION_IR_SAMPLES: i32 = 480_000;

/// Bounded offline iteration counts for [`benchmark_s3_stages`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3BenchmarkIterations {
    pub simulation_warmup: u32,
    pub simulation_measured: u32,
    pub reflection_warmup: u32,
    pub reflection_measured: u32,
    pub effect_warmup: u32,
    pub effect_measured: u32,
}

impl Default for S3BenchmarkIterations {
    fn default() -> Self {
        Self {
            simulation_warmup: 8,
            simulation_measured: 64,
            reflection_warmup: 2,
            reflection_measured: 8,
            effect_warmup: 8,
            effect_measured: 64,
        }
    }
}

/// One retained-session benchmark request.
///
/// This is an offline diagnostic. It makes no audio-thread, callback, or
/// multi-source performance claim.
#[derive(Clone, Debug, PartialEq)]
pub struct S3BenchmarkRequest {
    pub render: S3RenderRequest,
    pub iterations: S3BenchmarkIterations,
}

/// Raw integer nanoseconds for the six explicitly measured stages.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3StageTimingSamples {
    pub direct_simulation_ns: Vec<u64>,
    pub path_simulation_ns: Vec<u64>,
    pub reflection_simulation_ns: Vec<u64>,
    pub direct_effect_binaural_apply_ns: Vec<u64>,
    pub path_effect_apply_ns: Vec<u64>,
    pub reflection_effect_decode_apply_ns: Vec<u64>,
}

/// Finite-output checks performed outside the measured intervals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct S3BenchmarkFiniteChecks {
    pub direct_simulation: bool,
    pub path_simulation: bool,
    pub reflection_simulation: bool,
    pub direct_effect_binaural_apply: bool,
    pub path_effect_apply: bool,
    pub reflection_effect_decode_apply: bool,
    pub direct_simulation_samples_checked: u32,
    pub path_simulation_samples_checked: u32,
    pub reflection_simulation_samples_checked: u32,
    pub direct_effect_samples_checked: u32,
    pub path_effect_samples_checked: u32,
    pub reflection_effect_samples_checked: u32,
}

/// Evidence returned by one retained offline benchmark session.
#[derive(Clone, Debug, PartialEq)]
pub struct S3BenchmarkOutput {
    pub loaded_probe_count: u32,
    pub loaded_path_data_size_bytes: u64,
    pub retained: S3RetainedSessionStats,
    pub iterations: S3BenchmarkIterations,
    pub requested_simulation: S3SimulationConfig,
    pub delivered_simulation: S3SimulationConfig,
    pub snapshot: S3SimulationSnapshot,
    pub samples: S3StageTimingSamples,
    pub finite: S3BenchmarkFiniteChecks,
}

/// One adjacent summed-output boundary measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct S3BoundaryMeasurement {
    /// Boundary after this zero-based block index.
    pub after_block_index: usize,
    /// Largest per-channel boundary step in linear full-scale PCM units.
    pub max_step_full_scale: f32,
    /// Peak magnitude in the adjacent left/right windows, in linear full-scale units.
    pub local_peak_full_scale: f32,
    /// `max_step_full_scale / local_peak_full_scale`.
    pub step_to_local_peak_ratio: f32,
}

/// Continuity result measured only at boundaries between adjacent summed blocks.
///
/// Each boundary compares the last sample of the preceding summed block with
/// the first sample of the next block, and normalizes that step by the peak in
/// `window_frames` on both sides. Linear full-scale units keep the raw click
/// magnitude auditable; the ratio is level-independent. A ratio above `0.5`
/// means a one-sample jump exceeded half the adjacent signal peak, a conservative
/// mechanical click budget shared with the evidence layer.
#[derive(Clone, Debug, PartialEq)]
pub struct S3BoundaryContinuity {
    pub window_frames: usize,
    pub step_to_local_peak_threshold: f32,
    pub boundaries: Vec<S3BoundaryMeasurement>,
    pub maximum_step_to_local_peak_ratio: f32,
    pub passed: bool,
}

pub const S3_CONTINUITY_WINDOW_FRAMES: usize = 32;
pub const S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD: f32 = 0.5;

/// Measures boundary continuity on adjacent **summed-output** blocks.
///
/// Diagnostic stems are intentionally not accepted by this function's API.
pub fn measure_s3_summed_boundary_continuity(
    summed_blocks: &[OwnedStereoPcm],
    window_frames: usize,
    step_to_local_peak_threshold: f32,
) -> Result<S3BoundaryContinuity, BackendError> {
    if window_frames == 0 {
        return Err(BackendError::InvalidInput(
            "continuity window must contain at least one frame",
        ));
    }
    if !step_to_local_peak_threshold.is_finite() || step_to_local_peak_threshold <= 0.0 {
        return Err(BackendError::InvalidInput(
            "continuity threshold must be finite and positive",
        ));
    }
    if let Some(first) = summed_blocks.first() {
        for block in summed_blocks {
            if block.sample_rate_hz != first.sample_rate_hz
                || block.frame_count == 0
                || block.interleaved.len() != block.frame_count * 2
                || !block.is_finite()
            {
                return Err(BackendError::InvalidInput(
                    "summed continuity blocks must be finite nonempty stereo with one format",
                ));
            }
        }
    }

    let mut boundaries = Vec::with_capacity(summed_blocks.len().saturating_sub(1));
    for (index, pair) in summed_blocks.windows(2).enumerate() {
        let left = &pair[0];
        let right = &pair[1];
        let mut max_step = 0.0_f32;
        for channel in 0..2 {
            let previous = left.interleaved[(left.frame_count - 1) * 2 + channel];
            let next = right.interleaved[channel];
            max_step = max_step.max((next - previous).abs());
        }

        let left_start = left.frame_count.saturating_sub(window_frames);
        let right_end = right.frame_count.min(window_frames);
        let left_peak = left.interleaved[left_start * 2..]
            .iter()
            .chain(&right.interleaved[..right_end * 2])
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        let ratio = if left_peak > 0.0 {
            max_step / left_peak
        } else if max_step == 0.0 {
            0.0
        } else {
            f32::INFINITY
        };
        boundaries.push(S3BoundaryMeasurement {
            after_block_index: index,
            max_step_full_scale: max_step,
            local_peak_full_scale: left_peak,
            step_to_local_peak_ratio: ratio,
        });
    }
    let maximum_ratio = boundaries
        .iter()
        .map(|boundary| boundary.step_to_local_peak_ratio)
        .fold(0.0_f32, f32::max);
    Ok(S3BoundaryContinuity {
        window_frames,
        step_to_local_peak_threshold,
        passed: maximum_ratio <= step_to_local_peak_threshold,
        maximum_step_to_local_peak_ratio: maximum_ratio,
        boundaries,
    })
}

/// Rust-owned evidence for one retained-session trajectory block.
#[derive(Clone, Debug, PartialEq)]
pub struct S3TrajectoryBlock {
    pub block_index: usize,
    pub listener: ListenerPose,
    pub snapshot: S3SimulationSnapshot,
    pub direct_path_reflection_stems: S3Stems,
    /// Sum of direct, path, and reflections for this block.
    pub summed: OwnedStereoPcm,
    /// Copied explicitly for transition evidence; not used for continuity.
    pub direct_occlusion: f32,
    /// L2 norm of the path SH vector; not used for continuity.
    pub path_strength: f32,
}

/// Complete output of one retained trajectory session.
#[derive(Clone, Debug, PartialEq)]
pub struct S3TrajectoryRenderOutput {
    pub loaded_probe_count: u32,
    pub loaded_path_data_size_bytes: u64,
    pub retained: S3RetainedSessionStats,
    pub blocks: Vec<S3TrajectoryBlock>,
    pub summed: OwnedStereoPcm,
    pub continuity: S3BoundaryContinuity,
}

/// Rust-owned RAII wrapper for `IPLContext`.
///
/// It remains public only for the original linked-lifetime smoke test. All Phase A operations
/// use their own context internally and return no handle-bearing state.
pub struct Context {
    #[cfg(feature = "linked-sdk")]
    inner: linked::Context,
    #[cfg(not(feature = "linked-sdk"))]
    _private: (),
}

impl Context {
    /// Creates an SDK context using the pinned packed API version (`4 << 16 | 8 << 8 | 1`).
    pub fn create() -> Result<Self, ContextError> {
        #[cfg(feature = "linked-sdk")]
        {
            linked::Context::create()
                .map(|inner| Self { inner })
                .map_err(|status| ContextError::CreateFailed { status })
        }
        #[cfg(not(feature = "linked-sdk"))]
        {
            Err(ContextError::SdkUnavailable(unavailable_metadata()))
        }
    }

    /// Keeps the linked inner field observably used without exposing its raw handle.
    #[cfg(feature = "linked-sdk")]
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.inner.is_valid()
    }
}

/// Renders calibrated S0 mono PCM through Steam Audio direct and binaural effects.
pub fn render_s0(request: &S0RenderRequest) -> Result<S0RenderOutput, BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        linked::render_s0(request)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = request;
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Generates probes, invokes `iplPathBakerBake`, and returns copied serialized bytes.
pub fn bake_s3(request: &S3BakeRequest) -> Result<BakedProbeBatch, BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        linked::bake_s3(request)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = request;
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Reloads a baked probe batch into fresh handles, simulates one source, and renders all stems.
pub fn render_s3(
    request: &S3RenderRequest,
    baked: &BakedProbeBatch,
) -> Result<S3RenderOutput, BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        linked::render_s3(request, baked)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = (request, baked);
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Renders an ordered listener trajectory through one retained offline S3 session.
pub fn render_s3_trajectory(
    request: &S3TrajectoryRenderRequest,
    baked: &BakedProbeBatch,
) -> Result<S3TrajectoryRenderOutput, BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        linked::render_s3_trajectory(request, baked)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = (request, baked);
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Builds the bound simulation/render pair for one loaded world generation.
///
/// The pair supports at most
/// [`fightbox_runtime::backend::MAX_ACTIVE_SOURCES`] descriptors. The shared
/// world generation is retained by both halves, so the simulator, every
/// `IPLSource`, and reflection IR storage outlive all render callbacks even
/// when the simulation half is dropped first.
pub fn build_multi_source_session(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    simulation: S3SimulationConfig,
    sources: &[MultiSourceDescriptor],
) -> Result<(SteamAudioSimulationRunner, SteamAudioRenderGraph), BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        let (simulation, mut render) =
            linked::build_multi_source_session(mesh, baked, audio, simulation, sources)?;
        let stage_output_gain_writer = render
            .take_stage_output_gain_writer()
            .expect("new render graph owns its stage-gain producer");
        Ok((
            SteamAudioSimulationRunner { inner: simulation },
            SteamAudioRenderGraph {
                inner: render,
                stage_output_gain_control: Some(StageOutputGainControl {
                    writer: stage_output_gain_writer,
                }),
            },
        ))
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = (mesh, baked, audio, simulation, sources);
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Measures simulation and effect stages using one retained offline S3 graph.
pub fn benchmark_s3_stages(
    request: &S3BenchmarkRequest,
    baked: &BakedProbeBatch,
) -> Result<S3BenchmarkOutput, BackendError> {
    #[cfg(feature = "linked-sdk")]
    {
        linked::benchmark_s3_stages(request, baked)
    }
    #[cfg(not(feature = "linked-sdk"))]
    {
        let _ = (request, baked);
        Err(BackendError::SdkUnavailable(unavailable_metadata()))
    }
}

/// Dependency-free SHA-256 for stable binary provenance.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let padded_len = (bytes.len() + 9).div_ceil(64) * 64;
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    padded.resize(padded_len - 8, 0);
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    let mut schedule = [0_u32; 64];
    for chunk in padded.chunks_exact(64) {
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h].into_iter()) {
            *slot = slot.wrapping_add(value);
        }
    }

    let mut hex = String::with_capacity(64);
    for word in state {
        use fmt::Write as _;
        write!(&mut hex, "{word:08x}").expect("writing to a String cannot fail");
    }
    hex
}

#[cfg(feature = "linked-sdk")]
#[allow(unsafe_code)]
mod ffi;
#[cfg(feature = "linked-sdk")]
mod linked;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_each_enu_axis_exactly_once() {
        assert_eq!(
            enu_to_steam(EnuVector3::new(1.0, 0.0, 0.0)),
            SteamVector3::new(1.0, 0.0, 0.0)
        );
        assert_eq!(
            enu_to_steam(EnuVector3::new(0.0, 1.0, 0.0)),
            SteamVector3::new(0.0, 0.0, -1.0)
        );
        assert_eq!(
            enu_to_steam(EnuVector3::new(0.0, 0.0, 1.0)),
            SteamVector3::new(0.0, 1.0, 0.0)
        );
    }

    #[test]
    fn mapping_round_trips_positions_directions_and_orientation_axes() {
        for vector in [
            EnuVector3::new(12.5, -8.0, 1.75),
            EnuVector3::new(0.0, 1.0, 0.0),
            EnuVector3::new(0.0, 0.0, 1.0),
        ] {
            assert_eq!(steam_to_enu(enu_to_steam(vector)), vector);
        }
    }

    #[test]
    fn mapping_preserves_triangle_winding() {
        let east = enu_to_steam(EnuVector3::new(1.0, 0.0, 0.0));
        let north = enu_to_steam(EnuVector3::new(0.0, 1.0, 0.0));
        let up = enu_to_steam(EnuVector3::new(0.0, 0.0, 1.0));
        assert_eq!(cross(east, north), up);
    }

    #[test]
    fn default_build_reports_precise_machine_readable_unavailability() {
        #[cfg(not(feature = "linked-sdk"))]
        {
            let BackendAvailability::Unavailable(metadata) = backend_availability() else {
                panic!("default build must not claim an SDK link");
            };
            assert_eq!(metadata.expected_version, "4.8.1");
            assert_eq!(
                metadata.to_json(),
                r#"{"status":"unavailable","reason":"linked-sdk feature is disabled","requested_feature":"linked-sdk","required_environment":"STEAM_AUDIO_SDK_DIR","expected_version":"4.8.1","upstream_commit":"0da1825"}"#
            );
            let json = metadata.to_json();
            assert!(json.starts_with('{') && json.ends_with('}'));
            assert!(!json.contains(r#"\""#));
            assert!(matches!(
                render_s0(&S0RenderRequest {
                    audio: AudioConfig::default(),
                    source_position_enu: EnuVector3::new(1.0, 0.0, 0.0),
                    listener: ListenerPose::at(EnuVector3::default()),
                    input_mono: vec![0.0],
                    calibration_gain: 1.0,
                    apply_air_absorption: true,
                }),
                Err(BackendError::SdkUnavailable(_))
            ));
        }
    }

    #[test]
    fn sha256_matches_standard_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn decodes_pinned_first_order_sh_convention_into_enu_azimuth() {
        let expected = EnuVector3::new(-6.0, 4.0, 0.0);
        let length = (expected.x * expected.x + expected.y * expected.y).sqrt();
        let unit = EnuVector3::new(expected.x / length, expected.y / length, 0.0);
        let basis_scale = 0.488_603;
        let coefficients = [
            0.282_095,
            -basis_scale * unit.x,
            basis_scale * unit.z,
            basis_scale * unit.y,
        ];
        let decoded = decode_path_direction_enu(1, &coefficients)
            .expect("the audited first-order coefficients are valid")
            .expect("the directional moment is nonzero");
        assert!(decoded.first_order_magnitude.is_finite());
        assert!(decoded.first_order_magnitude > 0.0);
        assert!((decoded.azimuth_degrees_clockwise_from_north - 303.690_06).abs() < 0.001);
    }

    #[test]
    fn fixture_bake_order_is_the_runtime_pathing_order() {
        // IPLPathBakeParams in 4.8.1 contains no Ambisonic order. The fixture's
        // bake_order=2 is applied when runtime path coefficients are requested.
        let bake = PathBakeConfig::default();
        let simulation = S3SimulationConfig::default();
        assert_eq!(simulation.pathing_order, 2);
        assert_eq!(
            ((simulation.pathing_order + 1) * (simulation.pathing_order + 1)) as usize,
            9
        );
        assert_eq!(bake, PathBakeConfig::default());
    }

    #[test]
    fn direct_occlusion_mapping_is_exact_and_default_is_raycast() {
        let simulation = S3SimulationConfig::default();
        assert_eq!(simulation.direct_occlusion, DirectOcclusionMode::Raycast);
        assert_eq!(DirectOcclusionMode::Raycast.steam_audio_discriminant(), 0);
        assert_eq!(
            DirectOcclusionMode::Volumetric {
                radius_m: 0.5,
                sample_count: 16
            }
            .steam_audio_discriminant(),
            1
        );
    }

    #[test]
    fn direct_snapshot_rejects_out_of_range_occlusion() {
        let snapshot = DirectSnapshot {
            distance_attenuation: 1.0,
            air_absorption: [1.0; 3],
            directivity: 1.0,
            occlusion: 1.000_001,
            transmission: [1.0; 3],
            requested_occlusion_mode: DirectOcclusionMode::Raycast,
            delivered_occlusion_mode: DirectOcclusionMode::Raycast,
        };
        assert_eq!(
            validate_direct_snapshot(&snapshot),
            Err(BackendError::InvalidSdkOutput(
                "direct occlusion is outside the Steam Audio fraction range"
            ))
        );
    }

    #[test]
    fn reflection_effect_mapping_is_exact_and_tan_is_explicitly_unsupported() {
        assert_eq!(
            ReflectionEffectType::Convolution.steam_audio_cpu_discriminant(),
            Ok(0)
        );
        assert_eq!(
            ReflectionEffectType::Parametric.steam_audio_cpu_discriminant(),
            Ok(1)
        );
        assert_eq!(
            ReflectionEffectType::Hybrid.steam_audio_cpu_discriminant(),
            Ok(2)
        );
        assert!(
            ReflectionEffectType::TrueAudioNext
                .steam_audio_cpu_discriminant()
                .is_err()
        );
        assert_eq!(
            S3SimulationConfig::default().reflection_effect,
            ReflectionEffectConfig::CONVOLUTION
        );
    }

    #[test]
    fn summed_boundary_metric_catches_injected_discontinuity() {
        let sample_rate_hz = 48_000;
        let block_frames = 64;
        let mut samples = Vec::with_capacity(block_frames * 2);
        for frame in 0..block_frames * 2 {
            samples.push(
                (frame as f32 * 1_000.0 * core::f32::consts::TAU / sample_rate_hz as f32).sin()
                    * 0.1,
            );
        }
        let stereo_block = |mono: &[f32]| OwnedStereoPcm {
            sample_rate_hz,
            frame_count: mono.len(),
            interleaved: mono.iter().flat_map(|sample| [*sample, *sample]).collect(),
        };
        let left = stereo_block(&samples[..block_frames]);
        let right = stereo_block(&samples[block_frames..]);
        let smooth = measure_s3_summed_boundary_continuity(
            &[left.clone(), right.clone()],
            16,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .expect("continuous summed blocks are valid");
        assert!(smooth.passed);

        let mut injected = right;
        for sample in &mut injected.interleaved {
            *sample += 1.0;
        }
        let discontinuous = measure_s3_summed_boundary_continuity(
            &[left, injected],
            16,
            S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
        )
        .expect("finite injected-discontinuity blocks are valid");
        assert!(!discontinuous.passed);
        assert!(
            discontinuous.maximum_step_to_local_peak_ratio > S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD
        );
    }

    #[test]
    fn controlled_corner_includes_double_sided_floor() {
        let mesh = SceneMesh::controlled_s3_corner();
        assert_eq!(mesh.triangles.len(), 12);
        assert_eq!(mesh.material_indices.len(), mesh.triangles.len());
        assert!(
            mesh.vertices_enu_m
                .iter()
                .skip(6)
                .all(|vertex| vertex.z == 0.0)
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_sdk_creates_and_releases_a_context() {
        let context =
            Context::create().expect("verified Steam Audio 4.8.1 SDK should create a context");
        assert!(context.is_linked());
        drop(context);
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_s0_renders_calibrated_finite_stereo() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 256,
        };
        let input_mono = (0..1_024)
            .map(|frame| {
                let phase =
                    frame as f32 * 440.0 * core::f32::consts::TAU / audio.sample_rate_hz as f32;
                phase.sin() * 0.1
            })
            .collect();
        let output = render_s0(&S0RenderRequest {
            audio,
            source_position_enu: EnuVector3::new(10.0, 0.0, 0.0),
            listener: ListenerPose::at(EnuVector3::default()),
            input_mono,
            calibration_gain: 0.5,
            apply_air_absorption: true,
        })
        .expect("the verified SDK should render S0");

        assert_eq!(output.stereo.frame_count, 1_024);
        assert_eq!(output.stereo.interleaved.len(), 2_048);
        assert!(output.stereo.is_finite());
        assert!(output.distance_attenuation > 0.0);
        assert!(
            output
                .stereo
                .interleaved
                .iter()
                .any(|sample| sample.abs() > 1.0e-8)
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_lifetime_fixture_invokes_bake_and_serializes_path_data() {
        let baked = bake_s3(&S3BakeRequest::default())
            .expect("unit-test corner should generate probes and bake pathing");
        assert!(baked.metadata.probe_count > 0);
        assert!(baked.metadata.path_data_size_bytes > 0);
        assert_eq!(
            baked.metadata.serialized_size_bytes,
            baked.bytes.len() as u64
        );
        assert!(baked.metadata.bake_progress_callback_count > 0);
        assert_eq!(baked.metadata.final_bake_progress_millionths, 1_000_000);
        assert!(!baked.bytes.is_empty());
        baked.validate().expect("fresh bake metadata must validate");
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_lifetime_fixture_reloads_simulates_and_renders_separate_stems() {
        let baked = bake_s3(&S3BakeRequest::default())
            .expect("unit-test corner should generate probes and bake pathing");
        let audio = AudioConfig {
            sample_rate_hz: 24_000,
            frame_size: 256,
        };
        let input_mono = (0..512)
            .map(|frame| {
                (frame as f32 * 220.0 * core::f32::consts::TAU / audio.sample_rate_hz as f32).sin()
                    * 0.05
            })
            .collect();
        let mut request = S3RenderRequest::controlled_default(input_mono);
        request.audio = audio;
        request.simulation.reflection_rays = 256;
        request.simulation.diffuse_samples = 32;
        request.simulation.reflection_bounces = 1;
        request.simulation.reflection_duration_s = 0.1;
        request.simulation.reflection_order = 1;
        request.simulation.pathing_order = 1;
        let output =
            render_s3(&request, &baked).expect("fresh handles should load, simulate, and render");

        assert_eq!(output.loaded_probe_count, baked.metadata.probe_count);
        assert_eq!(
            output.loaded_path_data_size_bytes,
            baked.metadata.path_data_size_bytes
        );
        assert_eq!(
            output.snapshot.direct.requested_occlusion_mode,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(
            output.snapshot.direct.delivered_occlusion_mode,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(output.snapshot.path.sh_coeffs.len(), 4);
        assert!(output.snapshot.reflections.ir_size > 0);
        for stem in [
            &output.stems.direct,
            &output.stems.path,
            &output.stems.reflections,
            &output.stems.pathing_on_sum,
            &output.stems.pathing_off_sum,
        ] {
            assert!(stem.is_finite());
            assert_eq!(stem.frame_count, output.stems.direct.frame_count);
        }
        for ((on, off), path) in output
            .stems
            .pathing_on_sum
            .interleaved
            .iter()
            .zip(&output.stems.pathing_off_sum.interleaved)
            .zip(&output.stems.path.interleaved)
        {
            assert!(((on - off) - path).abs() < 1.0e-5);
        }
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn legacy_concave_s3_corner_exposes_validation_limit_and_unvalidated_path_moment() {
        let bake_request = legacy_concave_s3_bake_request(1.0, PathBakeConfig::default());
        let baked = bake_s3(&bake_request).expect("legacy concave path bake must succeed");
        assert_eq!(baked.metadata.probe_count, 317);

        let mut request = legacy_concave_s3_render_request(bake_request.mesh.clone());
        request.simulation.trace_path_validation = true;
        let validated =
            render_s3(&request, &baked).expect("legacy concave path simulation must succeed");
        assert_eq!(validated.snapshot.path.eq_coeffs, [1.0; 3]);
        assert!(
            validated
                .snapshot
                .path
                .sh_coeffs
                .iter()
                .all(|coefficient| *coefficient == 0.0)
        );
        assert_eq!(validated.snapshot.path.direction, None);
        assert_eq!(validated.snapshot.path.validation_segments.len(), 15);
        assert_eq!(
            validated
                .snapshot
                .path
                .validation_segments
                .iter()
                .filter(|segment| segment.occluded)
                .count(),
            5
        );
        assert!(
            validated
                .snapshot
                .path
                .validation_segments
                .iter()
                .any(is_surface_probe_rejection),
            "expected pinned 4.8.1 to reject a path segment ending on the y=0 masonry surface; trace={:?}",
            validated.snapshot.path.validation_segments
        );

        // With validation still enabled, pinned 4.8.1 retains the rejected
        // baked path when alternate lookup is disabled. This nonzero output is
        // explicitly not validated S3 evidence.
        let mut without_alternates_request = request.clone();
        without_alternates_request.simulation.find_alternate_paths = false;
        let without_alternates = render_s3(&without_alternates_request, &baked)
            .expect("alternate-path diagnostic render must succeed");
        assert!(without_alternates.snapshot.path.direction.is_some());
        assert!(
            without_alternates
                .snapshot
                .path
                .validation_segments
                .iter()
                .any(|segment| segment.occluded)
        );

        // In pinned 4.8.1, validation rejects every path for this exact static
        // legacy fixture. Disabling validation is a diagnostic only, not S3
        // acceptance evidence; it preserves the original matrix result used to
        // diagnose and replace this topology in ADR 0003.
        request.simulation.validate_paths = false;
        let output = render_s3(&request, &baked)
            .expect("unvalidated diagnostic path simulation must succeed");
        let direction = output.snapshot.path.direction.unwrap_or_else(|| {
            panic!(
                "legacy corner must have a nonzero first-order path moment; eq={:?}, sh={:?}",
                output.snapshot.path.eq_coeffs, output.snapshot.path.sh_coeffs
            )
        });
        let expected_azimuth = 213.690_068_f32;
        let delta = (direction.azimuth_degrees_clockwise_from_north - expected_azimuth)
            .abs()
            .min(360.0 - (direction.azimuth_degrees_clockwise_from_north - expected_azimuth).abs());
        assert!(direction.first_order_magnitude.is_finite());
        assert!(direction.first_order_magnitude > 0.0);
        assert!(
            delta <= 15.0,
            "decoded {}°, expected {expected_azimuth}° ±15°",
            direction.azimuth_degrees_clockwise_from_north
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn accepted_s3_convex_fixture_validates_nonzero_path_within_analytic_tolerance() {
        let bake_request = accepted_s3_fixture_bake_request(PathBakeConfig::default());
        let baked = bake_s3(&bake_request).expect("accepted S3 path bake must succeed");
        let mut request = accepted_s3_fixture_render_request(bake_request.mesh.clone());
        assert_eq!(
            request.simulation.direct_occlusion,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(request.simulation.reflection_rays, 4_096);
        assert_eq!(request.simulation.reflection_bounces, 2);
        assert_eq!(request.simulation.reflection_duration_s, 1.0);
        assert_eq!(request.simulation.pathing_order, 2);
        assert!(request.simulation.validate_paths);
        assert!(request.simulation.find_alternate_paths);
        request.simulation.trace_path_validation = true;
        let output = render_s3(&request, &baked)
            .expect("accepted S3 validated path simulation must succeed");
        assert_eq!(baked.metadata.probe_count, 324);
        assert_eq!(
            output.snapshot.direct.delivered_occlusion_mode,
            DirectOcclusionMode::Raycast
        );
        assert_eq!(output.snapshot.path.configured_order, 2);
        assert_eq!(
            output.snapshot.reflections.requested_effect_type,
            ReflectionEffectType::Convolution
        );
        assert_eq!(
            output.snapshot.reflections.delivered_effect_type,
            ReflectionEffectType::Convolution
        );
        assert_eq!(output.snapshot.reflections.ir_size, 24_000);
        assert_eq!(output.snapshot.reflections.sdk_num_channels, 4);
        let direction = output
            .snapshot
            .path
            .direction
            .expect("accepted S3 fixture must produce a validated path moment");
        let listener_to_corner = EnuVector3::new(-6.0, 4.0, 0.0);
        let analytic_azimuth = listener_to_corner
            .x
            .atan2(listener_to_corner.y)
            .to_degrees()
            .rem_euclid(360.0);
        assert!((analytic_azimuth - 303.690_06).abs() < 1.0e-4);
        let analytic_delta = azimuth_delta_degrees(
            direction.azimuth_degrees_clockwise_from_north,
            analytic_azimuth,
        );
        eprintln!(
            "accepted_s3_fixture probes={} direct_occlusion={} sh0={} azimuth={} delta={} segments={} occluded={}",
            baked.metadata.probe_count,
            output.snapshot.direct.occlusion,
            output.snapshot.path.sh_coeffs[0],
            direction.azimuth_degrees_clockwise_from_north,
            analytic_delta,
            output.snapshot.path.validation_segments.len(),
            output
                .snapshot
                .path
                .validation_segments
                .iter()
                .filter(|segment| segment.occluded)
                .count(),
        );
        assert_eq!(output.snapshot.direct.occlusion, 0.0);
        assert!(output.snapshot.path.sh_coeffs[0].abs() > 1.0e-8);
        assert!(!output.snapshot.path.validation_segments.is_empty());
        assert!(
            output
                .snapshot
                .path
                .validation_segments
                .iter()
                .all(|segment| !segment.occluded)
        );
        assert!(analytic_delta <= 15.0);
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_parametric_reflections_simulate_apply_and_report_mode() {
        let bake_request = accepted_s3_fixture_bake_request(PathBakeConfig::default());
        let baked = bake_s3(&bake_request).expect("accepted parametric test bake must succeed");
        let mut request = accepted_s3_fixture_render_request(bake_request.mesh.clone());
        request.simulation.reflection_rays = 256;
        request.simulation.reflection_bounces = 1;
        request.simulation.reflection_duration_s = 0.1;
        request.simulation.reflection_effect = ReflectionEffectConfig::PARAMETRIC;
        request.input_mono = (0..128)
            .map(|frame| {
                (frame as f32 * 220.0 * core::f32::consts::TAU
                    / request.audio.sample_rate_hz as f32)
                    .sin()
                    * 0.02
            })
            .collect();
        let started = std::time::Instant::now();
        let output =
            render_s3(&request, &baked).expect("parametric reflection render must succeed");
        let elapsed = started.elapsed();
        let snapshot = output.snapshot.reflections;
        eprintln!("parametric reflection render elapsed={elapsed:?} snapshot={snapshot:?}");
        assert_eq!(
            snapshot.requested_effect_type,
            ReflectionEffectType::Parametric
        );
        assert_eq!(
            snapshot.delivered_effect_type,
            ReflectionEffectType::Parametric
        );
        assert_eq!(snapshot.ir_size, 0);
        assert_eq!(snapshot.sdk_num_channels, 0);
        assert_eq!(snapshot.num_channels, 4);
        assert!(snapshot.applied_reverb_times.is_some_and(|times| {
            times
                .into_iter()
                .all(|value| value.is_finite() && value > 0.0)
        }));
        assert_eq!(snapshot.applied_hybrid_eq, None);
        assert_eq!(snapshot.applied_hybrid_delay_samples, None);
        assert_eq!(output.stems.reflections.frame_count, 2_560);
        assert_eq!(output.stems.reflections.interleaved.len(), 5_120);
        assert!(output.stems.reflections.is_finite());
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn linked_hybrid_reflections_simulate_apply_and_report_transition() {
        let bake_request = accepted_s3_fixture_bake_request(PathBakeConfig::default());
        let baked = bake_s3(&bake_request).expect("accepted hybrid test bake must succeed");
        let mut request = accepted_s3_fixture_render_request(bake_request.mesh.clone());
        request.simulation.reflection_rays = 4_096;
        request.simulation.reflection_bounces = 2;
        request.simulation.reflection_duration_s = 0.1;
        request.simulation.reflection_effect = ReflectionEffectConfig::hybrid(0.09, 0.25);
        request.input_mono = (0..128)
            .map(|frame| {
                (frame as f32 * 220.0 * core::f32::consts::TAU
                    / request.audio.sample_rate_hz as f32)
                    .sin()
                    * 0.02
            })
            .collect();
        let started = std::time::Instant::now();
        let output = render_s3(&request, &baked).expect("hybrid reflection render must succeed");
        let elapsed = started.elapsed();
        let snapshot = output.snapshot.reflections;
        eprintln!("hybrid reflection render elapsed={elapsed:?} snapshot={snapshot:?}");
        assert_eq!(snapshot.requested_effect_type, ReflectionEffectType::Hybrid);
        assert_eq!(snapshot.delivered_effect_type, ReflectionEffectType::Hybrid);
        assert_eq!(snapshot.ir_size, 2_400);
        assert_eq!(snapshot.sdk_num_channels, 4);
        assert_eq!(snapshot.num_channels, 4);
        assert_eq!(snapshot.configured_hybrid_transition_time_s, Some(0.09));
        assert_eq!(snapshot.configured_hybrid_overlap_percent, Some(0.25));
        assert!(snapshot.applied_reverb_times.is_some_and(|times| {
            times
                .into_iter()
                .all(|value| value.is_finite() && value > 0.0)
        }));
        assert!(snapshot.applied_hybrid_eq.is_some_and(|eq| {
            eq.into_iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
        }));
        assert!(
            snapshot
                .applied_hybrid_delay_samples
                .is_some_and(|delay| delay > 0)
        );
        assert_eq!(output.stems.reflections.frame_count, 2_560);
        assert_eq!(output.stems.reflections.interleaved.len(), 5_120);
        assert!(output.stems.reflections.is_finite());
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn reflection_effect_validation_rejects_inapplicable_invalid_and_tan_settings() {
        let invalid = [
            ReflectionEffectConfig {
                effect_type: ReflectionEffectType::Convolution,
                hybrid_transition_time_s: Some(0.05),
                hybrid_overlap_percent: Some(0.25),
            },
            ReflectionEffectConfig {
                effect_type: ReflectionEffectType::Hybrid,
                hybrid_transition_time_s: None,
                hybrid_overlap_percent: Some(0.25),
            },
            ReflectionEffectConfig::hybrid(2.0, 0.25),
            ReflectionEffectConfig::hybrid(0.05, 1.0),
            ReflectionEffectConfig::TRUE_AUDIO_NEXT_UNSUPPORTED,
        ];
        for reflection_effect in invalid {
            let mut request = S3RenderRequest::controlled_default(vec![0.0; 256]);
            request.simulation.reflection_effect = reflection_effect;
            let error = render_s3(
                &request,
                &BakedProbeBatch {
                    metadata: ProbeBatchMetadata {
                        schema_version: PROBE_BATCH_METADATA_SCHEMA,
                        steam_audio_version: STEAM_AUDIO_VERSION,
                        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                        probe_count: 1,
                        path_data_size_bytes: 1,
                        serialized_size_bytes: 1,
                        content_sha256: sha256_hex(&[0]),
                        bake_progress_callback_count: 1,
                        final_bake_progress_millionths: 1_000_000,
                    },
                    bytes: vec![0],
                },
            )
            .expect_err("reflection config validation must precede probe loading");
            assert!(matches!(error, BackendError::InvalidInput(_)));
        }
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn accepted_s3_trajectory_retains_one_session_and_passes_summed_boundary_continuity() {
        let bake_request = accepted_s3_fixture_bake_request(PathBakeConfig::default());
        let baked = bake_s3(&bake_request).expect("accepted S3 trajectory bake must succeed");
        let mut base = accepted_s3_fixture_render_request(bake_request.mesh.clone());
        let trajectory = [
            EnuVector3::new(6.0, -4.0, 1.5),
            EnuVector3::new(3.0, -3.0, 1.5),
            EnuVector3::new(1.0, -2.0, 1.5),
            EnuVector3::new(-0.5, -1.5, 1.5),
            EnuVector3::new(-2.0, -1.0, 1.5),
        ]
        .into_iter()
        .map(ListenerPose::at)
        .collect::<Vec<_>>();
        let total_frames = trajectory.len() * base.audio.frame_size as usize;
        base.input_mono = (0..total_frames)
            .map(|frame| {
                (frame as f32 * 220.0 * core::f32::consts::TAU / base.audio.sample_rate_hz as f32)
                    .sin()
                    * 0.02
            })
            .collect();
        let output = render_s3_trajectory(
            &S3TrajectoryRenderRequest {
                base,
                listener_trajectory: trajectory.clone(),
            },
            &baked,
        )
        .expect("accepted trajectory must render in one retained session");

        assert_eq!(output.retained.context_generations, 1);
        assert_eq!(output.retained.scene_generations, 1);
        assert_eq!(output.retained.probe_batch_loads, 1);
        assert_eq!(output.retained.simulator_generations, 1);
        assert_eq!(output.retained.source_generations, 1);
        assert_eq!(output.retained.hrtf_generations, 1);
        assert_eq!(output.retained.effect_graph_generations, 1);
        assert_eq!(output.retained.rendered_blocks, trajectory.len() as u32);
        assert_eq!(output.blocks.len(), trajectory.len());
        assert_eq!(output.summed.frame_count, total_frames);
        assert_eq!(output.summed.interleaved.len(), total_frames * 2);
        assert!(output.summed.is_finite());

        for (index, block) in output.blocks.iter().enumerate() {
            assert_eq!(block.block_index, index);
            assert_eq!(block.listener, trajectory[index]);
            assert_eq!(block.snapshot.reflections.ir_size, 24_000);
            assert!(block.path_strength.is_finite());
            for stem in [
                &block.direct_path_reflection_stems.direct,
                &block.direct_path_reflection_stems.path,
                &block.direct_path_reflection_stems.reflections,
                &block.summed,
            ] {
                assert_eq!(stem.frame_count, 128);
                assert_eq!(stem.interleaved.len(), 256);
                assert!(stem.is_finite());
            }
        }

        let first = output.blocks.first().expect("trajectory has a start");
        let last = output.blocks.last().expect("trajectory has an end");
        eprintln!(
            "accepted_s3_trajectory occlusions={:?} path_strengths={:?} continuity={:?}",
            output
                .blocks
                .iter()
                .map(|block| block.direct_occlusion)
                .collect::<Vec<_>>(),
            output
                .blocks
                .iter()
                .map(|block| block.path_strength)
                .collect::<Vec<_>>(),
            output.continuity
        );
        assert_eq!(first.direct_occlusion, 0.0);
        assert!(first.path_strength > 1.0e-8);
        assert_eq!(last.direct_occlusion, 1.0);
        assert!(last.direct_occlusion > first.direct_occlusion);
        assert_eq!(output.continuity.boundaries.len(), trajectory.len() - 1);
        assert!(
            output.continuity.passed,
            "summed boundary metric failed: {:?}",
            output.continuity
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    #[ignore = "manual legacy ADR 0003 matrix; run explicitly against verified Steam Audio 4.8.1"]
    fn legacy_concave_s3_validated_path_diagnostic_matrix() {
        use std::time::Instant;

        #[derive(Clone, Copy)]
        struct Case {
            name: &'static str,
            spacing_m: f32,
            bake: PathBakeConfig,
            runtime_samples: i32,
            runtime_radius_m: f32,
            runtime_threshold: f32,
            runtime_range_m: f32,
            validate: bool,
            alternate: bool,
            swap_endpoints: bool,
            horizontal_min_inset_m: f32,
            horizontal_max_inset_m: f32,
            expected_probe_count: u32,
            expected_nonzero_sh: bool,
        }

        let point_bake = PathBakeConfig::default();
        let volumetric_bake = PathBakeConfig {
            num_visibility_samples: 4,
            probe_visibility_radius_m: 0.5,
            visibility_threshold: 0.1,
            ..point_bake
        };
        let long_range_bake = PathBakeConfig {
            visibility_range_m: 100.0,
            path_range_m: 100.0,
            ..point_bake
        };
        let template = Case {
            name: "template",
            spacing_m: 1.0,
            bake: point_bake,
            runtime_samples: 1,
            runtime_radius_m: 0.0,
            runtime_threshold: 0.5,
            runtime_range_m: 6.0,
            validate: true,
            alternate: true,
            swap_endpoints: false,
            horizontal_min_inset_m: 0.0,
            horizontal_max_inset_m: 0.0,
            expected_probe_count: 317,
            expected_nonzero_sh: false,
        };
        let cases = [
            Case {
                name: "legacy_exact",
                ..template
            },
            Case {
                name: "unvalidated_control",
                validate: false,
                expected_nonzero_sh: true,
                ..template
            },
            Case {
                name: "validation_without_alternates",
                alternate: false,
                expected_nonzero_sh: true,
                ..template
            },
            Case {
                name: "runtime_volumetric",
                runtime_samples: 4,
                runtime_radius_m: 0.5,
                runtime_threshold: 0.1,
                ..template
            },
            Case {
                name: "matched_volumetric",
                bake: volumetric_bake,
                runtime_samples: 4,
                runtime_radius_m: 0.5,
                runtime_threshold: 0.1,
                ..template
            },
            Case {
                name: "long_visibility_ranges",
                bake: long_range_bake,
                runtime_range_m: 100.0,
                ..template
            },
            Case {
                name: "spacing_0_8",
                spacing_m: 0.8,
                expected_probe_count: 476,
                ..template
            },
            Case {
                name: "spacing_1_2",
                spacing_m: 1.2,
                expected_probe_count: 210,
                ..template
            },
            Case {
                name: "spacing_1_3",
                spacing_m: 1.3,
                expected_probe_count: 182,
                ..template
            },
            Case {
                name: "spacing_1_5",
                spacing_m: 1.5,
                expected_probe_count: 143,
                ..template
            },
            Case {
                name: "swapped_endpoints",
                swap_endpoints: true,
                ..template
            },
            Case {
                name: "symmetric_inset_0_25",
                horizontal_min_inset_m: 0.25,
                horizontal_max_inset_m: 0.25,
                expected_probe_count: 288,
                ..template
            },
        ];

        eprintln!(
            "case\ttriangles\tspacing_m\tmin_inset_m\tmax_inset_m\tprobes\tpath_bytes\tbake_samples\tbake_radius_m\tbake_threshold\tbake_vis_range_m\tpath_range_m\tbake_seconds\truntime_samples\truntime_radius_m\truntime_threshold\truntime_vis_range_m\tvalidate\talternate\tendpoints\trender_seconds\tsh_nonzero\tsh0\tazimuth_deg\tanalytic_delta_deg\tsegments\toccluded"
        );
        for case in cases {
            let mut bake_request = legacy_concave_s3_bake_request(case.spacing_m, case.bake);
            bake_request.probes.min_enu_m.x += case.horizontal_min_inset_m;
            bake_request.probes.min_enu_m.y += case.horizontal_min_inset_m;
            bake_request.probes.max_enu_m.x -= case.horizontal_max_inset_m;
            bake_request.probes.max_enu_m.y -= case.horizontal_max_inset_m;
            assert_eq!(bake_request.mesh.triangles.len(), 10);
            assert!(
                bake_request.probes.min_enu_m.x <= -6.0
                    && bake_request.probes.min_enu_m.y <= -4.0
                    && bake_request.probes.max_enu_m.x >= 4.0
                    && bake_request.probes.max_enu_m.y >= 6.0,
                "matrix probe volume must fully contain both endpoints"
            );
            let bake_started = Instant::now();
            let baked = bake_s3(&bake_request).expect("matrix path bake must succeed");
            let bake_seconds = bake_started.elapsed().as_secs_f64();
            assert_eq!(baked.metadata.probe_count, case.expected_probe_count);

            let mut request = legacy_concave_s3_render_request(bake_request.mesh.clone());
            request.simulation.pathing_visibility_samples = case.runtime_samples;
            request.simulation.pathing_visibility_radius_m = case.runtime_radius_m;
            request.simulation.pathing_visibility_threshold = case.runtime_threshold;
            request.simulation.pathing_visibility_range_m = case.runtime_range_m;
            request.simulation.validate_paths = case.validate;
            request.simulation.find_alternate_paths = case.alternate;
            request.simulation.trace_path_validation = case.validate;
            if case.swap_endpoints {
                request.source_position_enu = EnuVector3::new(4.0, 6.0, 1.5);
                request.listener = ListenerPose::at(EnuVector3::new(-6.0, -4.0, 1.5));
            }

            let render_started = Instant::now();
            let output = render_s3(&request, &baked).expect("matrix path render must succeed");
            let render_seconds = render_started.elapsed().as_secs_f64();
            let sh_nonzero = output
                .snapshot
                .path
                .sh_coeffs
                .iter()
                .any(|coefficient| coefficient.abs() > 1.0e-8);
            assert_eq!(
                sh_nonzero, case.expected_nonzero_sh,
                "unexpected SH result for {}",
                case.name
            );
            let azimuth = output
                .snapshot
                .path
                .direction
                .map_or(f32::NAN, |direction| {
                    direction.azimuth_degrees_clockwise_from_north
                });
            let analytic_delta = if azimuth.is_finite() {
                azimuth_delta_degrees(azimuth, 213.690_068)
            } else {
                f32::NAN
            };
            let occluded = output
                .snapshot
                .path
                .validation_segments
                .iter()
                .filter(|segment| segment.occluded)
                .count();
            let endpoints = if case.swap_endpoints {
                "swapped"
            } else {
                "legacy"
            };
            eprintln!(
                "{}\t10\t{:.3}\t{:.4}\t{:.4}\t{}\t{}\t{}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.6}\t{}\t{:.3}\t{:.3}\t{:.3}\t{}\t{}\t{}\t{:.6}\t{}\t{:.9}\t{:.6}\t{:.6}\t{}\t{}",
                case.name,
                case.spacing_m,
                case.horizontal_min_inset_m,
                case.horizontal_max_inset_m,
                baked.metadata.probe_count,
                baked.metadata.path_data_size_bytes,
                case.bake.num_visibility_samples,
                case.bake.probe_visibility_radius_m,
                case.bake.visibility_threshold,
                case.bake.visibility_range_m,
                case.bake.path_range_m,
                bake_seconds,
                case.runtime_samples,
                case.runtime_radius_m,
                case.runtime_threshold,
                case.runtime_range_m,
                case.validate,
                case.alternate,
                endpoints,
                render_seconds,
                sh_nonzero,
                output.snapshot.path.sh_coeffs[0],
                azimuth,
                analytic_delta,
                output.snapshot.path.validation_segments.len(),
                occluded,
            );
            if case.name == "legacy_exact" {
                eprintln!(
                    "legacy_rejected_segments={:?}",
                    output
                        .snapshot
                        .path
                        .validation_segments
                        .iter()
                        .filter(|segment| segment.occluded)
                        .collect::<Vec<_>>()
                );
            }
        }
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn render_validation_rejects_volumetric_samples_above_capacity() {
        let mut request = S3RenderRequest::controlled_default(vec![0.0; 256]);
        request.simulation.max_occlusion_samples = 63;
        request.simulation.direct_occlusion = DirectOcclusionMode::Volumetric {
            radius_m: 0.5,
            sample_count: 64,
        };
        let error = render_s3(
            &request,
            &BakedProbeBatch {
                metadata: ProbeBatchMetadata {
                    schema_version: PROBE_BATCH_METADATA_SCHEMA,
                    steam_audio_version: STEAM_AUDIO_VERSION,
                    upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                    probe_count: 1,
                    path_data_size_bytes: 1,
                    serialized_size_bytes: 1,
                    content_sha256: sha256_hex(&[0]),
                    bake_progress_callback_count: 1,
                    final_bake_progress_millionths: 1_000_000,
                },
                bytes: vec![0],
            },
        )
        .expect_err("render validation must run before serialized input is loaded");
        assert_eq!(
            error,
            BackendError::InvalidInput(
                "volumetric direct occlusion samples must not exceed simulator capacity"
            )
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn render_validation_rejects_invalid_volumetric_radius() {
        let mut request = S3RenderRequest::controlled_default(vec![0.0; 256]);
        request.simulation.direct_occlusion = DirectOcclusionMode::Volumetric {
            radius_m: 0.0,
            sample_count: 16,
        };
        let error = render_s3(
            &request,
            &BakedProbeBatch {
                metadata: ProbeBatchMetadata {
                    schema_version: PROBE_BATCH_METADATA_SCHEMA,
                    steam_audio_version: STEAM_AUDIO_VERSION,
                    upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                    probe_count: 1,
                    path_data_size_bytes: 1,
                    serialized_size_bytes: 1,
                    content_sha256: sha256_hex(&[0]),
                    bake_progress_callback_count: 1,
                    final_bake_progress_millionths: 1_000_000,
                },
                bytes: vec![0],
            },
        )
        .expect_err("render validation must reject zero volumetric radius before loading");
        assert_eq!(
            error,
            BackendError::InvalidInput(
                "volumetric direct occlusion radius must be finite and positive"
            )
        );
        request.simulation.direct_occlusion = DirectOcclusionMode::Volumetric {
            radius_m: 0.5,
            sample_count: 0,
        };
        let error = render_s3(
            &request,
            &BakedProbeBatch {
                metadata: ProbeBatchMetadata {
                    schema_version: PROBE_BATCH_METADATA_SCHEMA,
                    steam_audio_version: STEAM_AUDIO_VERSION,
                    upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                    probe_count: 1,
                    path_data_size_bytes: 1,
                    serialized_size_bytes: 1,
                    content_sha256: sha256_hex(&[0]),
                    bake_progress_callback_count: 1,
                    final_bake_progress_millionths: 1_000_000,
                },
                bytes: vec![0],
            },
        )
        .expect_err("render validation must reject zero volumetric samples before loading");
        assert_eq!(
            error,
            BackendError::InvalidInput("volumetric direct occlusion sample count must be positive")
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn benchmark_validation_rejects_zero_and_excessive_counts() {
        let baked = BakedProbeBatch {
            metadata: ProbeBatchMetadata {
                schema_version: PROBE_BATCH_METADATA_SCHEMA,
                steam_audio_version: STEAM_AUDIO_VERSION,
                upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
                probe_count: 1,
                path_data_size_bytes: 1,
                serialized_size_bytes: 1,
                content_sha256: sha256_hex(&[0]),
                bake_progress_callback_count: 1,
                final_bake_progress_millionths: 1_000_000,
            },
            bytes: vec![0],
        };
        let mut request = S3BenchmarkRequest {
            render: S3RenderRequest::controlled_default(vec![0.0; 512]),
            iterations: S3BenchmarkIterations::default(),
        };
        request.iterations.effect_measured = 0;
        assert_eq!(
            benchmark_s3_stages(&request, &baked),
            Err(BackendError::InvalidInput(
                "benchmark measured iteration counts must be positive"
            ))
        );
        request.iterations.effect_measured = 4_097;
        assert_eq!(
            benchmark_s3_stages(&request, &baked),
            Err(BackendError::InvalidInput(
                "benchmark iteration count exceeds the offline safety bound"
            ))
        );

        let resource_cases: [fn(&mut S3BenchmarkRequest); 6] = [
            |request| {
                request.render.simulation.max_occlusion_samples =
                    S3_BENCHMARK_MAX_OCCLUSION_SAMPLES + 1
            },
            |request| {
                request.render.simulation.reflection_rays = S3_BENCHMARK_MAX_REFLECTION_RAYS + 1
            },
            |request| {
                request.render.simulation.diffuse_samples = S3_BENCHMARK_MAX_DIFFUSE_SAMPLES + 1
            },
            |request| {
                request.render.simulation.reflection_bounces =
                    S3_BENCHMARK_MAX_REFLECTION_BOUNCES + 1
            },
            |request| {
                request.render.simulation.simulation_threads =
                    S3_BENCHMARK_MAX_SIMULATION_THREADS + 1
            },
            |request| {
                request.render.simulation.ray_batch_size = S3_BENCHMARK_MAX_RAY_BATCH_SIZE + 1
            },
        ];
        for exceed_bound in resource_cases {
            let mut request = S3BenchmarkRequest {
                render: S3RenderRequest::controlled_default(vec![0.0; 512]),
                iterations: S3BenchmarkIterations::default(),
            };
            exceed_bound(&mut request);
            assert_eq!(
                benchmark_s3_stages(&request, &baked),
                Err(BackendError::InvalidInput(
                    "benchmark simulation resource setting exceeds the offline safety bound"
                ))
            );
        }

        let mut request = S3BenchmarkRequest {
            render: S3RenderRequest::controlled_default(vec![0.0; 512]),
            iterations: S3BenchmarkIterations::default(),
        };
        request.render.simulation.reflection_duration_s = 10.1;
        assert_eq!(
            benchmark_s3_stages(&request, &baked),
            Err(BackendError::InvalidInput(
                "benchmark reflection IR capacity exceeds the offline safety bound"
            ))
        );
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    fn retained_benchmark_covers_direct_modes_and_cpu_reflection_types() {
        let mut pathing = PathBakeConfig::default();
        pathing.path_range_m = 100.0;
        let baked = bake_s3(&accepted_s3_fixture_bake_request(pathing))
            .expect("accepted fixture benchmark bake must succeed");
        let mut observed_occlusions = Vec::new();
        let cases = [
            (
                DirectOcclusionMode::Raycast,
                ReflectionEffectConfig::CONVOLUTION,
            ),
            (
                DirectOcclusionMode::Volumetric {
                    radius_m: 0.5,
                    sample_count: 16,
                },
                ReflectionEffectConfig::PARAMETRIC,
            ),
            (
                DirectOcclusionMode::Volumetric {
                    radius_m: 1.0,
                    sample_count: 64,
                },
                ReflectionEffectConfig::hybrid(0.025, 0.25),
            ),
        ];
        for (direct_occlusion, reflection_effect) in cases {
            let mut render =
                accepted_s3_fixture_render_request(accepted_s3_fixture_bake_request(pathing).mesh);
            // Straddle the finite north edge of the x=0 wall. The center ray is
            // blocked while a source sphere extends around the wall edge.
            render.source_position_enu = EnuVector3::new(-0.5, 9.75, 1.5);
            render.listener = ListenerPose::at(EnuVector3::new(0.5, 9.75, 1.5));
            render.simulation.direct_occlusion = direct_occlusion;
            render.simulation.reflection_effect = reflection_effect;
            render.simulation.reflection_rays = 256;
            render.simulation.diffuse_samples = 16;
            render.simulation.reflection_bounces = 1;
            render.simulation.reflection_duration_s = 0.05;
            render.simulation.reflection_order = 0;
            let iterations = S3BenchmarkIterations {
                simulation_warmup: 1,
                simulation_measured: 2,
                reflection_warmup: 1,
                reflection_measured: 2,
                effect_warmup: 1,
                effect_measured: 2,
            };
            let output = benchmark_s3_stages(&S3BenchmarkRequest { render, iterations }, &baked)
                .expect("retained benchmark case must succeed");
            assert_eq!(output.retained.context_generations, 1);
            assert_eq!(output.retained.scene_generations, 1);
            assert_eq!(output.retained.probe_batch_loads, 1);
            assert_eq!(output.retained.simulator_generations, 1);
            assert_eq!(output.retained.source_generations, 1);
            assert_eq!(output.retained.hrtf_generations, 1);
            assert_eq!(output.retained.effect_graph_generations, 1);
            assert_eq!(output.retained.rendered_blocks, 3);
            assert_eq!(output.requested_simulation, output.delivered_simulation);
            assert_eq!(
                output.snapshot.direct.requested_occlusion_mode,
                direct_occlusion
            );
            assert_eq!(
                output.snapshot.direct.delivered_occlusion_mode,
                direct_occlusion
            );
            assert_eq!(output.samples.direct_simulation_ns.len(), 2);
            assert_eq!(output.samples.path_simulation_ns.len(), 2);
            assert_eq!(output.samples.reflection_simulation_ns.len(), 2);
            assert_eq!(output.samples.direct_effect_binaural_apply_ns.len(), 2);
            assert_eq!(output.samples.path_effect_apply_ns.len(), 2);
            assert_eq!(output.samples.reflection_effect_decode_apply_ns.len(), 2);
            assert!(
                output
                    .samples
                    .direct_simulation_ns
                    .iter()
                    .chain(&output.samples.path_simulation_ns)
                    .chain(&output.samples.reflection_simulation_ns)
                    .chain(&output.samples.direct_effect_binaural_apply_ns)
                    .chain(&output.samples.path_effect_apply_ns)
                    .chain(&output.samples.reflection_effect_decode_apply_ns)
                    .all(|sample| *sample > 0)
            );
            assert_eq!(
                output.finite,
                S3BenchmarkFiniteChecks {
                    direct_simulation: true,
                    path_simulation: true,
                    reflection_simulation: true,
                    direct_effect_binaural_apply: true,
                    path_effect_apply: true,
                    reflection_effect_decode_apply: true,
                    direct_simulation_samples_checked: 2,
                    path_simulation_samples_checked: 2,
                    reflection_simulation_samples_checked: 2,
                    direct_effect_samples_checked: 2,
                    path_effect_samples_checked: 2,
                    reflection_effect_samples_checked: 2,
                }
            );
            let range = |samples: &[u64]| {
                (
                    samples.iter().copied().min().unwrap_or(0),
                    samples.iter().copied().max().unwrap_or(0),
                )
            };
            eprintln!(
                "retained_benchmark mode={direct_occlusion:?} reflection={:?} direct_sim_ns={:?} path_sim_ns={:?} reflection_sim_ns={:?} direct_apply_ns={:?} path_apply_ns={:?} reflection_apply_ns={:?}",
                reflection_effect.effect_type,
                range(&output.samples.direct_simulation_ns),
                range(&output.samples.path_simulation_ns),
                range(&output.samples.reflection_simulation_ns),
                range(&output.samples.direct_effect_binaural_apply_ns),
                range(&output.samples.path_effect_apply_ns),
                range(&output.samples.reflection_effect_decode_apply_ns),
            );
            observed_occlusions.push(output.snapshot.direct.occlusion);
        }
        assert!(observed_occlusions.iter().all(|value| value.is_finite()));
        assert!(
            observed_occlusions
                .iter()
                .all(|value| (0.0..=1.0).contains(value))
        );
        assert!(
            observed_occlusions[1..]
                .iter()
                .any(|value| (*value - observed_occlusions[0]).abs() > f32::EPSILON),
            "volumetric direct occlusion should meaningfully differ from the binary raycast at the corner"
        );
    }

    #[cfg(feature = "linked-sdk")]
    fn accepted_s3_fixture_bake_request(pathing: PathBakeConfig) -> S3BakeRequest {
        // Exact executable copy of fixtures/s3-corner/fixture.json as accepted
        // by ADR 0003. Do not substitute SceneMesh::controlled_s3_corner here.
        S3BakeRequest {
            mesh: SceneMesh {
                vertices_enu_m: vec![
                    EnuVector3::new(0.0, 0.0, 0.0),
                    EnuVector3::new(10.0, 0.0, 0.0),
                    EnuVector3::new(10.0, 0.0, 6.0),
                    EnuVector3::new(0.0, 0.0, 6.0),
                    EnuVector3::new(0.0, 10.0, 0.0),
                    EnuVector3::new(0.0, 10.0, 6.0),
                    EnuVector3::new(-9.0, -9.0, 0.0),
                    EnuVector3::new(9.0, -9.0, 0.0),
                    EnuVector3::new(9.0, 9.0, 0.0),
                    EnuVector3::new(-9.0, 9.0, 0.0),
                ],
                triangles: vec![
                    [0, 1, 2],
                    [0, 2, 3],
                    [2, 1, 0],
                    [3, 2, 0],
                    [0, 4, 5],
                    [0, 5, 3],
                    [5, 4, 0],
                    [3, 5, 0],
                    [6, 7, 8],
                    [6, 8, 9],
                ],
                material_indices: vec![0; 10],
                materials: vec![AcousticMaterial::MASONRY],
            },
            probes: ProbeVolume {
                min_enu_m: EnuVector3::new(-8.75, -8.75, 0.5),
                max_enu_m: EnuVector3::new(8.25, 8.25, 2.5),
                spacing_m: 1.0,
                height_above_floor_m: 1.5,
            },
            pathing,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn accepted_s3_fixture_render_request(mesh: SceneMesh) -> S3RenderRequest {
        let mut simulation = S3SimulationConfig::default();
        simulation.reflection_rays = 4_096;
        S3RenderRequest {
            mesh,
            audio: AudioConfig {
                sample_rate_hz: 24_000,
                frame_size: 128,
            },
            simulation,
            source_position_enu: EnuVector3::new(-4.0, 6.0, 1.5),
            listener: ListenerPose::at(EnuVector3::new(6.0, -4.0, 1.5)),
            input_mono: vec![0.0; 128],
            calibration_gain: 1.0,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn legacy_concave_s3_bake_request(spacing_m: f32, pathing: PathBakeConfig) -> S3BakeRequest {
        S3BakeRequest {
            mesh: SceneMesh {
                vertices_enu_m: vec![
                    EnuVector3::new(-10.0, 0.0, 0.0),
                    EnuVector3::new(0.0, 0.0, 0.0),
                    EnuVector3::new(0.0, 0.0, 6.0),
                    EnuVector3::new(-10.0, 0.0, 6.0),
                    EnuVector3::new(0.0, -10.0, 0.0),
                    EnuVector3::new(0.0, -10.0, 6.0),
                    EnuVector3::new(-9.0, -9.0, 0.0),
                    EnuVector3::new(7.0, -9.0, 0.0),
                    EnuVector3::new(7.0, 9.0, 0.0),
                    EnuVector3::new(-9.0, 9.0, 0.0),
                ],
                triangles: vec![
                    [0, 1, 2],
                    [0, 2, 3],
                    [2, 1, 0],
                    [3, 2, 0],
                    [4, 1, 2],
                    [4, 2, 5],
                    [2, 1, 4],
                    [5, 2, 4],
                    [6, 7, 8],
                    [6, 8, 9],
                ],
                material_indices: vec![0; 10],
                materials: vec![AcousticMaterial::MASONRY],
            },
            probes: ProbeVolume {
                min_enu_m: EnuVector3::new(-9.0, -9.0, 0.5),
                max_enu_m: EnuVector3::new(7.0, 9.0, 2.5),
                spacing_m,
                height_above_floor_m: 1.5,
            },
            pathing,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn legacy_concave_s3_render_request(mesh: SceneMesh) -> S3RenderRequest {
        let simulation = S3SimulationConfig {
            reflection_rays: 256,
            diffuse_samples: 16,
            reflection_bounces: 1,
            reflection_duration_s: 0.05,
            reflection_order: 0,
            pathing_order: 2,
            ..S3SimulationConfig::default()
        };
        S3RenderRequest {
            mesh,
            audio: AudioConfig {
                sample_rate_hz: 24_000,
                frame_size: 128,
            },
            simulation,
            source_position_enu: EnuVector3::new(-6.0, -4.0, 1.5),
            listener: ListenerPose::at(EnuVector3::new(4.0, 6.0, 1.5)),
            input_mono: vec![0.0; 128],
            calibration_gain: 1.0,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn is_surface_probe_rejection(segment: &PathValidationSegment) -> bool {
        segment.occluded
            && segment.from_enu_m.x < 0.0
            && (segment.from_enu_m.x - segment.to_enu_m.x).abs() < 1.0e-4
            && segment.from_enu_m.y < -0.9
            && segment.to_enu_m.y.abs() < 1.0e-4
    }

    #[cfg(feature = "linked-sdk")]
    fn azimuth_delta_degrees(actual: f32, expected: f32) -> f32 {
        let delta = (actual - expected).abs();
        delta.min(360.0 - delta)
    }

    fn cross(left: SteamVector3, right: SteamVector3) -> SteamVector3 {
        SteamVector3::new(
            left.y * right.z - left.z * right.y,
            left.z * right.x - left.x * right.z,
            left.x * right.y - left.y * right.x,
        )
    }
}
