//! Additive C ABI types for the neutral pre-HRTF spatial route.
//!
//! The legacy `FbSessionConfig` and final-stereo entry points are frozen.  V2
//! therefore carries its own versioned, size-prefixed structures.  The public
//! structures in this module are transport layouts, not mirrors of the Rust
//! backend types.

use crate::{FbPose, FbQualityTier, FbResult, FbSourceUpdate, FbVec3};
use core::{mem, ptr, slice};

pub const FB_ABI_VERSION_V2: u32 = 2;
pub const FB_MAX_PROGRAM_CHANNELS_V2: u32 = 2;
pub const FB_MAX_PRESENTATION_FEEDS_PER_SOURCE_V2: u32 = 3;
pub const FB_MAX_PRESENTATION_FEEDS_V2: u32 = 48;
pub const FB_MAX_ENVIRONMENTAL_ORDER_V2: u32 = 2;
pub const FB_MAX_ENVIRONMENTAL_CHANNELS_V2: u32 = 9;
/// Wave 0's public `MultiPoint` descriptor has a policy-fixed 1 m radius.
///
/// `FbSourceProgramConfigV2.extent_m` still carries that scalar so a later
/// per-source-radius implementation does not require another C layout change.
pub const FB_MULTIPOINT_FIXED_EXTENT_METERS_V2: f32 = 1.0;
/// Current `ExtentDescriptor::MultiPoint` stores point-cloud multiplicity in a
/// `u8`. This is distinct from Steam Audio's separate occlusion sample policy.
pub const FB_MAX_MULTIPOINT_COUNT_V2: u32 = 255;
pub const FB_CELL_STREAM_PHASE_IDLE_V2: u32 = 0;
pub const FB_CELL_STREAM_PHASE_PUBLISHING_V2: u32 = 1;
pub const FB_CELL_STREAM_PHASE_PREPARED_V2: u32 = 2;
pub const FB_CELL_STREAM_PHASE_CROSSFADING_V2: u32 = 3;
pub const FB_CELL_STREAM_PHASE_TAIL_RETIRING_V2: u32 = 4;
pub const FB_CELL_STREAM_PHASE_TAIL_COMPLETE_V2: u32 = 5;
pub const FB_MAX_MACRO_ACTIVATIONS_V2: u32 = 4;
pub const FB_MACRO_ELIGIBILITY_DETAILED_DIRECT_V2: u32 = 1 << 0;
pub const FB_MACRO_ELIGIBILITY_STATISTICAL_GROUND_V2: u32 = 1 << 1;
pub const FB_MACRO_ELIGIBILITY_BAKED_REFLECTIONS_V2: u32 = 1 << 2;
pub const FB_MACRO_ELIGIBILITY_AUTHORED_ECHO_V2: u32 = 1 << 3;
pub const FB_MACRO_ELIGIBILITY_SHARED_DIFFUSE_V2: u32 = 1 << 4;

/// Fixed direct-bank component offsets within each logical source's 3 planes.
pub const FB_PRESENTATION_PLANE_DIRECT_CENTER_OFFSET_V2: u32 = 0;
pub const FB_PRESENTATION_PLANE_WIDTH_POSITIVE_OFFSET_V2: u32 = 1;
pub const FB_PRESENTATION_PLANE_WIDTH_NEGATIVE_OFFSET_V2: u32 = 2;

pub const FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2: u32 = 1 << 0;
pub const FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2: u32 = 1 << 1;
pub const FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2: u32 = 1 << 2;
/// Reserved in Wave 0. Current HRTF-decoded echo taps cannot enter this route.
pub const FB_PRESENTATION_COMPONENT_DISCRETE_ECHO_V2: u32 = 1 << 3;

pub const FB_SPATIAL_BLOCK_VALID_V2: u32 = 1 << 0;
pub const FB_SPATIAL_BLOCK_DISCONTINUITY_V2: u32 = 1 << 1;
pub const FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2: u32 = 1 << 2;
pub const FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2: u32 = 1 << 3;
pub const FB_SPATIAL_WORLD_UNROTATED_V2: u32 = 1 << 4;
pub const FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2: u32 = 1 << 5;
pub const FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2: u32 = 1 << 6;
pub const FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2: u32 = 1 << 7;

/// Construction-time route. A session never changes route after creation.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbRenderRouteV2 {
    FbRenderLegacyFinalStereoV2 = 0,
    FbRenderNeutralSpatialV2 = 1,
}

/// Source geometry admitted by the Wave 0 transport seam.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbSourceGeometryV2 {
    FbSourceGeometryPointV2 = 0,
    /// One physical source and one derived Point presentation feed. The extent
    /// only controls volumetric occlusion sampling; it does not create feeds.
    FbSourceGeometryMultiPointV2 = 1,
    FbSourceGeometryLineSegmentV2 = 2,
    /// Wave 0 structurally preserves two planes as L -> `WidthNegative` and
    /// R -> `WidthPositive`, with `DirectCenter` inactive. Perceptual width,
    /// PCA/M-S/crossfeed, distance-width law, and promotion remain Wave 2.
    FbSourceGeometryStereoImageV2 = 3,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbEnvironmentalChannelOrderV2 {
    FbEnvironmentalAcnV2 = 0,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbEnvironmentalNormalizationV2 {
    FbEnvironmentalN3dV2 = 0,
}

/// Steam Audio world axes: +X right/east, +Y up, +Z back/south.
///
/// The engine's ENU-to-Steam rotation is `(x, y, z) = (east, up, -north)`.
/// ACN channel numbers do not imply this axis convention, so it is carried
/// explicitly on every block.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbEnvironmentalBasisV2 {
    /// Engine domain axes: +X east, +Y north, +Z up.
    FbEnvironmentalRightHandedEnuV2 = 0,
    /// Steam Audio axes after the engine's one domain rotation.
    FbEnvironmentalSteamXRightYUpZBackV2 = 1,
}

impl TryFrom<u32> for FbEnvironmentalBasisV2 {
    type Error = FbResult;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            value if value == Self::FbEnvironmentalRightHandedEnuV2 as u32 => {
                Ok(Self::FbEnvironmentalRightHandedEnuV2)
            }
            value if value == Self::FbEnvironmentalSteamXRightYUpZBackV2 as u32 => {
                Ok(Self::FbEnvironmentalSteamXRightYUpZBackV2)
            }
            _ => Err(FbResult::FbInvalidArgument),
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbSpatialOutputValidityV2 {
    FbSpatialInvalidV2 = 0,
    FbSpatialValidV2 = 1,
    FbSpatialSilentDiscontinuityV2 = 2,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbPresentationPlacementV2 {
    FbPresentationPoseV2 = 0,
    FbPresentationDirectionV2 = 1,
}

/// Immutable V2 session construction settings.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSessionConfigV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub source_count: u32,
    pub default_source_level_db: f32,
    /// One of `FbQualityTier`.
    pub quality_tier: u32,
    /// One of `FbRenderRouteV2`.
    pub render_route: u32,
    /// Requested environmental order in the inclusive range 0...2.
    pub environmental_order: u32,
    /// Must be zero. A later ABI version, not a silent reinterpretation, owns it.
    pub reserved: [u32; 7],
}

impl Default for FbSessionConfigV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            sample_rate_hz: 48_000,
            block_size_frames: 128,
            source_count: 1,
            default_source_level_db: 0.0,
            quality_tier: FbQualityTier::FbQualityDesktop as u32,
            render_route: FbRenderRouteV2::FbRenderNeutralSpatialV2 as u32,
            environmental_order: FB_MAX_ENVIRONMENTAL_ORDER_V2,
            reserved: [0; 7],
        }
    }
}

/// Immutable identity and host-accounted size estimates for one prepared cell.
/// The 128-bit keys are opaque stable package identifiers split high/low for C.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbCellConfigV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub city_key_high: u64,
    pub city_key_low: u64,
    pub cell_key_high: u64,
    pub cell_key_low: u64,
    pub raw_cell_bytes: u64,
    pub prepared_resident_bytes: u64,
    pub reserved: [u64; 4],
}

impl Default for FbCellConfigV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            city_key_high: 0,
            city_key_low: 0,
            cell_key_high: 0,
            cell_key_low: 0,
            raw_cell_bytes: 0,
            prepared_resident_bytes: 0,
            reserved: [0; 4],
        }
    }
}

/// Atomic control-side view of the neutral render swap lifecycle.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbCellStreamStateV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub phase: u32,
    pub reserved_u32: u32,
    /// Generation currently owned by the simulation/control half. During
    /// `Prepared` this is still the old world; after offer it is the candidate.
    pub control_generation: u64,
    pub reserved: [u64; 4],
}

/// One of the four retained transient render reservations.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbMacroEventRoleV2 {
    FbMacroCinematicImpulseV2 = 0,
    FbMacroStandardImpulseV2 = 1,
    FbMacroBallisticCrackV2 = 2,
    FbMacroBallisticBlastV2 = 3,
}

/// Whether dormant source media can be recovered at an exact program frame.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbMacroAssetTransportV2 {
    FbMacroAssetSeekableV2 = 0,
    FbMacroAssetPreGeneratedV2 = 1,
    FbMacroAssetDeterministicGeneratorV2 = 2,
    FbMacroAssetNonSeekableLiveV2 = 3,
}

/// Which renderer owns an activated event's final local leg.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbMacroRenderAuthorityV2 {
    FbMacroRenderAuthorityNoneV2 = 0,
    FbMacroRenderAuthorityDetailedLocalV2 = 1,
    FbMacroRenderAuthorityFallbackV2 = 2,
}

/// Stable reason for audible macro/shared-diffuse fallback.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbMacroFallbackReasonV2 {
    FbMacroFallbackNoneV2 = 0,
    FbMacroFallbackMissingActiveCellV2 = 1,
    FbMacroFallbackStaleActiveCellV2 = 2,
    FbMacroFallbackMissingPackageAuthorityV2 = 3,
    FbMacroFallbackStalePackageAuthorityV2 = 4,
    FbMacroFallbackMissingProbeBakeAuthorityV2 = 5,
    FbMacroFallbackStaleProbeBakeAuthorityV2 = 6,
    FbMacroFallbackMissingEchoAuthorityV2 = 7,
    FbMacroFallbackStaleEchoAuthorityV2 = 8,
    FbMacroFallbackInvalidLocalLegV2 = 9,
}

/// Host weather frozen for macro and local spectral planning.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbAtmosphereObservationV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub temperature_c: f32,
    pub relative_humidity_percent: f32,
    pub pressure_kpa: f32,
    pub reserved_f32: f32,
    pub reserved: [u64; 4],
}

impl Default for FbAtmosphereObservationV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            temperature_c: 20.0,
            relative_humidity_percent: 50.0,
            pressure_kpa: 101.325,
            reserved_f32: 0.0,
            reserved: [0; 4],
        }
    }
}

/// One member of an atomically admitted macro-event group.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroEventRequestV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub event_id: u64,
    pub atomic_group_id: u64,
    pub role: u32,
    pub asset_transport: u32,
    pub asset_key: u64,
    pub emission_frame: u64,
    pub program_seek_frame: u64,
    pub retained_frames_after_activation: u64,
    pub emitter_position_enu: FbVec3,
    pub local_horizon_m: f32,
    /// Zero means the asset is dry; one means the recording carries motion.
    pub recording_carries_motion: u32,
    pub reserved_u32: u32,
    pub reserved: [u64; 4],
}

impl Default for FbMacroEventRequestV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            event_id: 0,
            atomic_group_id: 0,
            role: FbMacroEventRoleV2::FbMacroCinematicImpulseV2 as u32,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetSeekableV2 as u32,
            asset_key: 0,
            emission_frame: 0,
            program_seek_frame: 0,
            retained_frames_after_activation: 0,
            emitter_position_enu: FbVec3::default(),
            local_horizon_m: 600.0,
            recording_carries_motion: 0,
            reserved_u32: 0,
            reserved: [0; 4],
        }
    }
}

/// One due event after macro transport and adopted-cell authority resolution.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroActivationV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub active: u32,
    pub role: u32,
    pub render_authority: u32,
    pub fallback_reason: u32,
    pub eligibility_bits: u32,
    pub shared_diffuse_fallback: u32,
    pub event_id: u64,
    pub atomic_group_id: u64,
    pub asset_key: u64,
    pub emission_frame: u64,
    pub program_seek_frame: u64,
    pub macro_arrival_frame: u64,
    pub fallback_ear_arrival_frame: u64,
    pub tail_deadline_frame: u64,
    pub authority_world_generation: u64,
    pub macro_distance_gain: f32,
    pub macro_atmosphere_gain_db: [f32; 8],
    pub ingress_proxy_enu: FbVec3,
    pub remote_direction_enu: FbVec3,
    pub local_distance_m: f32,
    pub local_delay_frames: u64,
    pub local_distance_gain: f32,
    pub local_atmosphere_gain_db: [f32; 8],
    pub reserved: [u64; 4],
}

impl Default for FbMacroActivationV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            active: 0,
            role: 0,
            render_authority: FbMacroRenderAuthorityV2::FbMacroRenderAuthorityNoneV2 as u32,
            fallback_reason: FbMacroFallbackReasonV2::FbMacroFallbackNoneV2 as u32,
            eligibility_bits: 0,
            shared_diffuse_fallback: 0,
            event_id: 0,
            atomic_group_id: 0,
            asset_key: 0,
            emission_frame: 0,
            program_seek_frame: 0,
            macro_arrival_frame: 0,
            fallback_ear_arrival_frame: 0,
            tail_deadline_frame: 0,
            authority_world_generation: 0,
            macro_distance_gain: 0.0,
            macro_atmosphere_gain_db: [0.0; 8],
            ingress_proxy_enu: FbVec3::default(),
            remote_direction_enu: FbVec3::default(),
            local_distance_m: 0.0,
            local_delay_frames: 0,
            local_distance_gain: 0.0,
            local_atmosphere_gain_db: [0.0; 8],
            reserved: [0; 4],
        }
    }
}

/// Fixed-capacity result of one control-thread due-event activation pass.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbMacroActivationBatchV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub activation_count: u32,
    pub reserved_u32: u32,
    pub activations: [FbMacroActivationV2; FB_MAX_MACRO_ACTIVATIONS_V2 as usize],
    pub reserved: [u64; 4],
}

impl Default for FbMacroActivationBatchV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            activation_count: 0,
            reserved_u32: 0,
            activations: [FbMacroActivationV2::default(); FB_MAX_MACRO_ACTIVATIONS_V2 as usize],
            reserved: [0; 4],
        }
    }
}

/// One complete control-thread frame for a session's stable source set.
///
/// Source record ordinal is the stable zero-based source index. The caller
/// supplies exactly the session's configured source count; an extended stride
/// may append caller-private bytes to each current `FbSourceUpdate` prefix.
/// `source_updates` names one contiguous readable allocation spanning exactly
/// `(source_count - 1) * source_update_stride_bytes + sizeof(FbSourceUpdate)`
/// bytes (or more) for the duration of the call.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbControlFrameV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub source_updates: *const FbSourceUpdate,
    pub source_count: u32,
    pub source_update_stride_bytes: u32,
    pub listener_pose: FbPose,
    pub listener_linear_velocity_mps: FbVec3,
    /// Must be zero. A later ABI version, not a silent reinterpretation, owns it.
    pub reserved: [u64; 4],
}

impl Default for FbControlFrameV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            source_updates: core::ptr::null(),
            source_count: 0,
            source_update_stride_bytes: core::mem::size_of::<FbSourceUpdate>() as u32,
            listener_pose: FbPose {
                position: FbVec3 {
                    east_m: 0.0,
                    north_m: 0.0,
                    up_m: 0.0,
                },
                forward: FbVec3 {
                    east_m: 0.0,
                    north_m: 1.0,
                    up_m: 0.0,
                },
                up: FbVec3 {
                    east_m: 0.0,
                    north_m: 0.0,
                    up_m: 1.0,
                },
            },
            listener_linear_velocity_mps: FbVec3 {
                east_m: 0.0,
                north_m: 0.0,
                up_m: 0.0,
            },
            reserved: [0; 4],
        }
    }
}

/// Construction-time program and presentation shape for one logical source.
///
/// Wave 0 two-plane `StereoImage` preserves plane zero/left at the symmetric
/// `WidthNegative` endpoint and plane one/right at `WidthPositive`; its fixed
/// `DirectCenter` plane is inactive. Both planes share one physical trajectory.
/// A mono `StereoImage` inferably freezes `MonoExpanded` provenance; a two-plane
/// image inferably freezes `AuthoredStereo` provenance. Mono expansion remains
/// backend-unavailable in Wave 0, but the transport shape is reserved here.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSourceProgramConfigV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub source_index: u32,
    /// One or two planar channels. Both share one physical propagation clock.
    pub channel_count: u32,
    /// One of `FbSourceGeometryV2`.
    pub source_geometry: u32,
    /// Declared point-cloud multiplicity. Nonzero only for `MultiPoint` and
    /// currently constrained to 1...255. This does not configure Steam's
    /// separate volumetric-occlusion sample count.
    pub multipoint_count: u32,
    /// Geometry extent in metres: zero for Point, the policy-fixed 1 m radius
    /// for MultiPoint, or the positive full length/width for Line/StereoImage.
    pub extent_m: f32,
    pub reserved_f32: f32,
    pub reserved: [u64; 4],
}

impl Default for FbSourceProgramConfigV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            source_index: 0,
            channel_count: 1,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryPointV2 as u32,
            multipoint_count: 0,
            extent_m: 0.0,
            reserved_f32: 0.0,
            reserved: [0; 4],
        }
    }
}

/// One source-major planar input record.
///
/// Channel zero begins at `samples`; channel one, when present, begins at
/// `samples + channel_stride_samples`. Each active plane contains exactly the
/// configured block length of finite `float` samples. For a two-plane
/// `StereoImage`, channel zero is left/`WidthNegative` and channel one is
/// right/`WidthPositive`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSourceProgramInputV2 {
    pub source_index: u32,
    pub channel_count: u32,
    pub samples: *const f32,
    pub sample_count: usize,
    pub channel_stride_samples: usize,
    pub reserved: [u64; 4],
}

/// Caller-owned contiguous planar output bank.
///
/// Plane `n` starts at `samples + n * plane_stride_samples`. In the direct bank,
/// logical source `s` owns planes `s * 3 + {center, width+, width-}` using the
/// three `FB_PRESENTATION_PLANE_*_OFFSET_V2` constants. `DiscreteEcho` reserves
/// a component bit but has no plane. V2 writes only the configured block length
/// and never touches padding or guard samples.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbPlanarOutputV2 {
    pub samples: *mut f32,
    pub sample_capacity: usize,
    pub plane_capacity: u32,
    pub reserved_u32: u32,
    pub plane_stride_samples: usize,
    pub reserved: [u64; 4],
}

/// Metadata parallel to one direct-presentation output plane.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbPresentationFeedMetadataV2 {
    pub valid: u32,
    pub source_index: u32,
    /// Exactly one `FB_PRESENTATION_COMPONENT_*_V2` bit.
    pub component: u32,
    /// One of `FbPresentationPlacementV2`.
    pub placement: u32,
    /// Correlated absolute ENU pose; diagnostic when placement is Direction.
    pub pose: FbPose,
    /// Listener-to-feed unit ENU direction. Displacement with squared length
    /// at most `1e-12 m^2`, including coincidence, is +north.
    pub direction_enu: crate::FbVec3,
    pub processing_latency_frames: u32,
    pub reserved: [u32; 3],
}

impl Default for FbPresentationFeedMetadataV2 {
    fn default() -> Self {
        Self {
            valid: 0,
            source_index: 0,
            component: 0,
            placement: FbPresentationPlacementV2::FbPresentationPoseV2 as u32,
            pose: FbPose::default(),
            direction_enu: crate::FbVec3::default(),
            processing_latency_frames: 0,
            reserved: [0; 3],
        }
    }
}

/// Per-block truth accompanying the direct and environmental planar banks.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSpatialBlockMetadataV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub block_start_frame: u64,
    pub generation: u64,
    pub discontinuity_sequence: u64,
    /// One of `FbSpatialOutputValidityV2`.
    pub validity: u32,
    /// Count of valid entries across the fixed 48-plane presentation bank.
    pub active_presentation_feed_count: u32,
    pub environmental_order: u32,
    pub environmental_channel_count: u32,
    pub environmental_latency_frames: u32,
    pub component_mask: u32,
    pub flags: u32,
    pub environmental_channel_order: u32,
    pub environmental_normalization: u32,
    pub environmental_basis: u32,
    pub reserved_u32: u32,
    pub reserved: [u64; 4],
}

impl Default for FbSpatialBlockMetadataV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            sample_rate_hz: 0,
            block_size_frames: 0,
            block_start_frame: 0,
            generation: 0,
            discontinuity_sequence: 0,
            validity: FbSpatialOutputValidityV2::FbSpatialInvalidV2 as u32,
            active_presentation_feed_count: 0,
            environmental_order: 0,
            environmental_channel_count: 1,
            environmental_latency_frames: 0,
            component_mask: 0,
            flags: 0,
            environmental_channel_order: FbEnvironmentalChannelOrderV2::FbEnvironmentalAcnV2 as u32,
            environmental_normalization: FbEnvironmentalNormalizationV2::FbEnvironmentalN3dV2
                as u32,
            environmental_basis: FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2
                as u32,
            reserved_u32: 0,
            reserved: [0; 4],
        }
    }
}

/// One allocation-free spatial render call.
///
/// Record strides permit additive structure growth without changing this
/// function signature. V2 requires strides at least as large as the current
/// record and aligned for that record. All reserved fields must be zero.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSpatialRenderBlockV2 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub source_programs: *const FbSourceProgramInputV2,
    pub source_program_count: u32,
    pub source_program_stride_bytes: u32,
    pub direct_output: FbPlanarOutputV2,
    pub environmental_output: FbPlanarOutputV2,
    pub feed_metadata: *mut FbPresentationFeedMetadataV2,
    pub feed_metadata_capacity: u32,
    pub feed_metadata_stride_bytes: u32,
    pub block_metadata: *mut FbSpatialBlockMetadataV2,
    pub reserved: [u64; 4],
}

impl Default for FbSpatialRenderBlockV2 {
    fn default() -> Self {
        Self {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: core::mem::size_of::<Self>() as u32,
            source_programs: core::ptr::null(),
            source_program_count: 0,
            source_program_stride_bytes: core::mem::size_of::<FbSourceProgramInputV2>() as u32,
            direct_output: FbPlanarOutputV2 {
                samples: core::ptr::null_mut(),
                sample_capacity: 0,
                plane_capacity: 0,
                reserved_u32: 0,
                plane_stride_samples: 0,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: core::ptr::null_mut(),
                sample_capacity: 0,
                plane_capacity: 0,
                reserved_u32: 0,
                plane_stride_samples: 0,
                reserved: [0; 4],
            },
            feed_metadata: core::ptr::null_mut(),
            feed_metadata_capacity: 0,
            feed_metadata_stride_bytes: core::mem::size_of::<FbPresentationFeedMetadataV2>() as u32,
            block_metadata: core::ptr::null_mut(),
            reserved: [0; 4],
        }
    }
}

const _: () = assert!(FB_MAX_PROGRAM_CHANNELS_V2 == 2);
const _: () = assert!(FB_MAX_PRESENTATION_FEEDS_PER_SOURCE_V2 == 3);
const _: () = assert!(FB_MAX_PRESENTATION_FEEDS_V2 == 16 * 3);
const _: () = assert!(FB_MAX_ENVIRONMENTAL_CHANNELS_V2 == (2 + 1) * (2 + 1));
const _: () = assert!(FB_PRESENTATION_PLANE_DIRECT_CENTER_OFFSET_V2 == 0);
const _: () = assert!(FB_PRESENTATION_PLANE_WIDTH_POSITIVE_OFFSET_V2 == 1);
const _: () = assert!(FB_PRESENTATION_PLANE_WIDTH_NEGATIVE_OFFSET_V2 == 2);

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FbAbiHeaderV2 {
    abi_version: u32,
    struct_size: u32,
}

/// Copies the supported prefix of a size-prefixed V2 structure.
///
/// The eight-byte header is read before the code checks full-structure size or
/// alignment. This prevents an undersized caller allocation from being copied
/// as the current Rust structure merely to discover that it is undersized.
///
/// # Safety
///
/// `pointer` must name readable storage for at least [`FbAbiHeaderV2`]. If its
/// header advertises the current structure size, it must name readable storage
/// for `T` as required by the C ABI contract.
unsafe fn copy_abi_struct_v2<T: Copy>(pointer: *const T) -> Result<T, FbResult> {
    if pointer.is_null() {
        return Err(FbResult::FbInvalidArgument);
    }
    // Safety: the caller promises the universal eight-byte header is readable.
    // `read_unaligned` lets us reject a misaligned full structure afterward.
    let header = unsafe { ptr::read_unaligned(pointer.cast::<FbAbiHeaderV2>()) };
    let size = usize::try_from(header.struct_size).map_err(|_| FbResult::FbInvalidArgument)?;
    if header.abi_version != FB_ABI_VERSION_V2
        || size < mem::size_of::<T>()
        || !pointer.addr().is_multiple_of(mem::align_of::<T>())
    {
        return Err(FbResult::FbInvalidArgument);
    }
    // Safety: the header proved the declared size and alignment; the caller's
    // C ABI memory-validity precondition supplies readable storage for `T`.
    Ok(unsafe { ptr::read(pointer) })
}

/// Validates a caller-owned output structure without copying or writing it.
///
/// # Safety
///
/// `pointer` follows the same header/full-size readability contract as
/// [`copy_abi_struct_v2`].
unsafe fn validate_abi_output_v2<T>(pointer: *mut T) -> Result<(), FbResult> {
    if pointer.is_null() {
        return Err(FbResult::FbInvalidArgument);
    }
    // Safety: the caller promises the universal eight-byte header is readable.
    let header = unsafe { ptr::read_unaligned(pointer.cast::<FbAbiHeaderV2>()) };
    let size = usize::try_from(header.struct_size).map_err(|_| FbResult::FbInvalidArgument)?;
    if header.abi_version != FB_ABI_VERSION_V2
        || size < mem::size_of::<T>()
        || !pointer.addr().is_multiple_of(mem::align_of::<T>())
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

/// Reads a V2 session config after validating only its universal header first.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_session_config_v2(
    pointer: *const FbSessionConfigV2,
) -> Result<FbSessionConfigV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Reads a V2 cell config after validating only its universal header first.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_cell_config_v2(
    pointer: *const FbCellConfigV2,
) -> Result<FbCellConfigV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Reads a V2 atmosphere observation after validating its universal header.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_atmosphere_observation_v2(
    pointer: *const FbAtmosphereObservationV2,
) -> Result<FbAtmosphereObservationV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Reads one macro-event request after validating its universal header.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_macro_event_request_v2(
    pointer: *const FbMacroEventRequestV2,
) -> Result<FbMacroEventRequestV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Validates the writable current activation-batch layout.
///
/// # Safety
///
/// `pointer` must satisfy [`validate_abi_output_v2`]'s memory contract.
pub(crate) unsafe fn validate_macro_activation_batch_output_v2(
    pointer: *mut FbMacroActivationBatchV2,
) -> Result<(), FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { validate_abi_output_v2(pointer) }
}

/// Reads a V2 source config after validating only its universal header first.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_source_program_config_v2(
    pointer: *const FbSourceProgramConfigV2,
) -> Result<FbSourceProgramConfigV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Reads a V2 control frame after validating only its universal header first.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_control_frame_v2(
    pointer: *const FbControlFrameV2,
) -> Result<FbControlFrameV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

/// Reads a V2 render block after validating only its universal header first.
///
/// # Safety
///
/// `pointer` must satisfy [`copy_abi_struct_v2`]'s memory contract.
pub(crate) unsafe fn copy_spatial_render_block_v2(
    pointer: *const FbSpatialRenderBlockV2,
) -> Result<FbSpatialRenderBlockV2, FbResult> {
    // Safety: forwarded from this function's caller contract.
    unsafe { copy_abi_struct_v2(pointer) }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidatedSourceProgramV2 {
    pub source_index: usize,
    pub channel_count: usize,
    pub planes: [*const f32; FB_MAX_PROGRAM_CHANNELS_V2 as usize],
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidatedControlFrameV2 {
    pub listener_pose: FbPose,
    pub listener_linear_velocity_mps: FbVec3,
    pub source_updates: [FbSourceUpdate; 16],
}

impl Default for ValidatedSourceProgramV2 {
    fn default() -> Self {
        Self {
            source_index: 0,
            channel_count: 0,
            planes: [ptr::null(); FB_MAX_PROGRAM_CHANNELS_V2 as usize],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ValidatedSpatialRenderBlockV2 {
    pub programs: [ValidatedSourceProgramV2; 16],
    pub program_count: usize,
    pub direct_samples: *mut f32,
    pub direct_stride_samples: usize,
    pub environmental_samples: *mut f32,
    pub environmental_stride_samples: usize,
    pub feed_metadata: *mut FbPresentationFeedMetadataV2,
    pub feed_metadata_stride_bytes: usize,
    pub block_metadata: *mut FbSpatialBlockMetadataV2,
}

#[derive(Clone, Copy, Debug, Default)]
struct ByteRange {
    start: usize,
    end: usize,
}

pub(crate) fn validate_session_config_v2(config: &FbSessionConfigV2) -> Result<(), FbResult> {
    if config.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(config.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbSessionConfigV2>())
        || config.sample_rate_hz == 0
        || config.block_size_frames == 0
        || config.source_count == 0
        || config.source_count > 16
        || !config.default_source_level_db.is_finite()
        || i32::try_from(config.sample_rate_hz).is_err()
        || i32::try_from(config.block_size_frames).is_err()
        || !matches!(
            config.quality_tier,
            value if value == FbQualityTier::FbQualityDesktop as u32
                || value == FbQualityTier::FbQualityMobile as u32
        )
        || !matches!(
            config.render_route,
            value if value == FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32
                || value == FbRenderRouteV2::FbRenderNeutralSpatialV2 as u32
        )
        || config.environmental_order > FB_MAX_ENVIRONMENTAL_ORDER_V2
        || (config.render_route == FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32
            && config.environmental_order != 0)
        || config.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

pub(crate) fn validate_cell_config_v2(config: &FbCellConfigV2) -> Result<(), FbResult> {
    const MAX_RAW_CELL_BYTES: u64 = 64 * 1024 * 1024;
    if config.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(config.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbCellConfigV2>())
        || (config.city_key_high == 0 && config.city_key_low == 0)
        || (config.cell_key_high == 0 && config.cell_key_low == 0)
        || config.raw_cell_bytes == 0
        || config.raw_cell_bytes > MAX_RAW_CELL_BYTES
        || config.prepared_resident_bytes == 0
        || config.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

pub(crate) fn validate_atmosphere_observation_v2(
    observation: &FbAtmosphereObservationV2,
) -> Result<(), FbResult> {
    if observation.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(observation.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbAtmosphereObservationV2>())
        || observation.reserved_f32.to_bits() != 0
        || observation.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

pub(crate) fn validate_macro_event_request_v2(
    request: &FbMacroEventRequestV2,
) -> Result<(), FbResult> {
    if request.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(request.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbMacroEventRequestV2>())
        || request.event_id == 0
        || request.atomic_group_id == 0
        || request.asset_key == 0
        || request.role > FbMacroEventRoleV2::FbMacroBallisticBlastV2 as u32
        || request.asset_transport > FbMacroAssetTransportV2::FbMacroAssetNonSeekableLiveV2 as u32
        || !request.local_horizon_m.is_finite()
        || request.local_horizon_m < 1.0
        || request.recording_carries_motion > 1
        || request.reserved_u32 != 0
        || request.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

pub(crate) fn validate_source_program_config_v2(
    config: &FbSourceProgramConfigV2,
    source_count: usize,
) -> Result<(), FbResult> {
    let point = FbSourceGeometryV2::FbSourceGeometryPointV2 as u32;
    let multipoint = FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32;
    let line = FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32;
    let stereo_image = FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32;
    if config.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(config.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbSourceProgramConfigV2>())
        || usize::try_from(config.source_index).map_or(true, |index| index >= source_count)
        || !(1..=FB_MAX_PROGRAM_CHANNELS_V2).contains(&config.channel_count)
        || !matches!(config.source_geometry, value if value == point || value == multipoint || value == line || value == stereo_image)
        || !config.extent_m.is_finite()
        || config.reserved_f32.to_bits() != 0
        || config.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    let shape_valid = match config.source_geometry {
        value if value == point => {
            config.channel_count == 1
                && config.multipoint_count == 0
                && config.extent_m.to_bits() == 0
        }
        value if value == multipoint => {
            config.channel_count == 1
                && (1..=FB_MAX_MULTIPOINT_COUNT_V2).contains(&config.multipoint_count)
                && config.extent_m.to_bits() == FB_MULTIPOINT_FIXED_EXTENT_METERS_V2.to_bits()
        }
        value if value == line => {
            config.channel_count == 1 && config.multipoint_count == 0 && config.extent_m > 0.0
        }
        value if value == stereo_image => config.multipoint_count == 0 && config.extent_m > 0.0,
        _ => false,
    };
    shape_valid.then_some(()).ok_or(FbResult::FbInvalidArgument)
}

/// Validates the complete control-frame graph and copies every source prefix.
///
/// No caller-owned pointer remains in the returned value. This lets the FFI
/// entry point validate motion semantics for every record before mutating any
/// session state or revoking a prepared lifecycle.
///
/// # Safety
///
/// `frame.source_updates` must name one contiguous readable allocation spanning
/// `(source_count - 1) * source_update_stride_bytes + sizeof(FbSourceUpdate)`
/// bytes. The function rejects null, misaligned, undersized, and arithmetically
/// overflowing layouts before reading the records.
pub(crate) unsafe fn validate_control_frame_v2(
    frame: &FbControlFrameV2,
    session_source_count: usize,
) -> Result<ValidatedControlFrameV2, FbResult> {
    if frame.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(frame.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbControlFrameV2>())
        || frame.reserved.iter().any(|value| *value != 0)
        || session_source_count == 0
        || session_source_count > 16
        || usize::try_from(frame.source_count).ok() != Some(session_source_count)
        || !valid_const_pointer(frame.source_updates)
    {
        return Err(FbResult::FbInvalidArgument);
    }

    let stride = usize::try_from(frame.source_update_stride_bytes)
        .map_err(|_| FbResult::FbInvalidArgument)?;
    if stride < mem::size_of::<FbSourceUpdate>()
        || !stride.is_multiple_of(mem::align_of::<FbSourceUpdate>())
    {
        return Err(FbResult::FbInvalidArgument);
    }
    let span = required_strided_elements(
        session_source_count,
        stride,
        mem::size_of::<FbSourceUpdate>(),
    )?;
    byte_range(frame.source_updates.addr(), span, 1)?;

    let empty_source = FbSourceUpdate {
        active: 0,
        pose: FbPose {
            position: FbVec3 {
                east_m: 0.0,
                north_m: 0.0,
                up_m: 0.0,
            },
            forward: FbVec3 {
                east_m: 0.0,
                north_m: 1.0,
                up_m: 0.0,
            },
            up: FbVec3 {
                east_m: 0.0,
                north_m: 0.0,
                up_m: 1.0,
            },
        },
        linear_velocity_mps: FbVec3 {
            east_m: 0.0,
            north_m: 0.0,
            up_m: 0.0,
        },
    };
    let mut source_updates = [empty_source; 16];
    for (source_index, destination) in source_updates[..session_source_count]
        .iter_mut()
        .enumerate()
    {
        let offset = source_index
            .checked_mul(stride)
            .ok_or(FbResult::FbInvalidArgument)?;
        // Safety: the caller supplies the readable strided record graph. The
        // range and alignment checks above cover every current-size prefix.
        let source = unsafe {
            frame
                .source_updates
                .cast::<u8>()
                .add(offset)
                .cast::<FbSourceUpdate>()
                .read()
        };
        *destination = source;
    }

    Ok(ValidatedControlFrameV2 {
        listener_pose: frame.listener_pose,
        listener_linear_velocity_mps: frame.listener_linear_velocity_mps,
        source_updates,
    })
}

/// Validates every pointer, count, stride, finite input sample, and aliasing
/// relationship before the audio clock advances or an output byte is touched.
///
/// # Safety
///
/// The caller must provide readable outer and source-program records and
/// readable/writable sample/metadata storage for the declared capacities.
/// This is the ordinary C ABI memory-validity precondition. The function still
/// rejects null, misaligned, arithmetically overflowing, undersized, and
/// overlapping active ranges before constructing slices.
pub(crate) unsafe fn validate_spatial_render_block_v2(
    block: &FbSpatialRenderBlockV2,
    source_count: usize,
    configured_channels: &[u8; 16],
    block_size: usize,
) -> Result<ValidatedSpatialRenderBlockV2, FbResult> {
    if block.abi_version != FB_ABI_VERSION_V2
        || usize::try_from(block.struct_size)
            .ok()
            .is_none_or(|size| size < mem::size_of::<FbSpatialRenderBlockV2>())
        || block.reserved.iter().any(|value| *value != 0)
        || source_count == 0
        || source_count > configured_channels.len()
        || block_size == 0
    {
        return Err(FbResult::FbInvalidArgument);
    }

    let program_count =
        usize::try_from(block.source_program_count).map_err(|_| FbResult::FbInvalidArgument)?;
    if program_count > source_count
        || (program_count != 0 && !valid_const_pointer(block.source_programs))
    {
        return Err(FbResult::FbInvalidArgument);
    }
    let program_stride = usize::try_from(block.source_program_stride_bytes)
        .map_err(|_| FbResult::FbInvalidArgument)?;
    if program_stride < mem::size_of::<FbSourceProgramInputV2>()
        || !program_stride.is_multiple_of(mem::align_of::<FbSourceProgramInputV2>())
        || (program_count != 0
            && program_count
                .checked_sub(1)
                .and_then(|last| last.checked_mul(program_stride))
                .and_then(|offset| offset.checked_add(mem::size_of::<FbSourceProgramInputV2>()))
                .is_none())
    {
        return Err(FbResult::FbInvalidArgument);
    }
    if program_count != 0 {
        let span = required_strided_elements(
            program_count,
            program_stride,
            mem::size_of::<FbSourceProgramInputV2>(),
        )?;
        byte_range(block.source_programs.addr(), span, 1)?;
    }

    validate_planar_output(
        &block.direct_output,
        FB_MAX_PRESENTATION_FEEDS_V2 as usize,
        block_size,
    )?;
    validate_planar_output(
        &block.environmental_output,
        FB_MAX_ENVIRONMENTAL_CHANNELS_V2 as usize,
        block_size,
    )?;

    let feed_metadata_capacity =
        usize::try_from(block.feed_metadata_capacity).map_err(|_| FbResult::FbInvalidArgument)?;
    let feed_metadata_stride = usize::try_from(block.feed_metadata_stride_bytes)
        .map_err(|_| FbResult::FbInvalidArgument)?;
    if feed_metadata_capacity < FB_MAX_PRESENTATION_FEEDS_V2 as usize
        || !valid_mut_pointer(block.feed_metadata)
        || feed_metadata_stride < mem::size_of::<FbPresentationFeedMetadataV2>()
        || !feed_metadata_stride.is_multiple_of(mem::align_of::<FbPresentationFeedMetadataV2>())
        || !valid_mut_pointer(block.block_metadata)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    // Safety: the caller promises the output metadata's universal header is
    // readable and the advertised current-size object is writable.
    unsafe { validate_abi_output_v2(block.block_metadata) }?;
    let metadata_span = required_strided_elements(
        FB_MAX_PRESENTATION_FEEDS_V2 as usize,
        feed_metadata_stride,
        mem::size_of::<FbPresentationFeedMetadataV2>(),
    )?;
    byte_range(block.feed_metadata.addr(), metadata_span, 1)?;
    byte_range(
        block.block_metadata.addr(),
        1,
        mem::size_of::<FbSpatialBlockMetadataV2>(),
    )?;

    const MAX_RANGES: usize = 16 + 2 * 16 + 48 + 9 + 48 + 1;
    let mut ranges = [ByteRange::default(); MAX_RANGES];
    let mut range_count = 0;
    let mut programs = [ValidatedSourceProgramV2::default(); 16];
    let mut seen_sources = [false; 16];

    for (slot, validated) in programs[..program_count].iter_mut().enumerate() {
        let offset = slot
            .checked_mul(program_stride)
            .ok_or(FbResult::FbInvalidArgument)?;
        let address = block
            .source_programs
            .addr()
            .checked_add(offset)
            .ok_or(FbResult::FbInvalidArgument)?;
        if !address.is_multiple_of(mem::align_of::<FbSourceProgramInputV2>()) {
            return Err(FbResult::FbInvalidArgument);
        }
        let program_pointer = block
            .source_programs
            .cast::<u8>()
            .wrapping_add(offset)
            .cast::<FbSourceProgramInputV2>();
        // Safety: the C caller promises a readable record at every declared
        // stride; address arithmetic and alignment were validated above.
        let program = unsafe { ptr::read(program_pointer) };
        insert_nonoverlapping_range(
            &mut ranges,
            &mut range_count,
            byte_range(address, 1, mem::size_of::<FbSourceProgramInputV2>())?,
        )?;
        if program.reserved.iter().any(|value| *value != 0) {
            return Err(FbResult::FbInvalidArgument);
        }
        let source_index =
            usize::try_from(program.source_index).map_err(|_| FbResult::FbInvalidArgument)?;
        let channel_count =
            usize::try_from(program.channel_count).map_err(|_| FbResult::FbInvalidArgument)?;
        if source_index >= source_count
            || seen_sources[source_index]
            || !(1..=FB_MAX_PROGRAM_CHANNELS_V2 as usize).contains(&channel_count)
            || usize::from(configured_channels[source_index]) != channel_count
            || !valid_const_pointer(program.samples)
            || program.channel_stride_samples < block_size
        {
            return Err(FbResult::FbInvalidArgument);
        }
        let required_samples =
            required_strided_elements(channel_count, program.channel_stride_samples, block_size)?;
        if program.sample_count < required_samples {
            return Err(FbResult::FbInvalidArgument);
        }

        seen_sources[source_index] = true;
        validated.source_index = source_index;
        validated.channel_count = channel_count;
        for (channel, plane) in validated.planes[..channel_count].iter_mut().enumerate() {
            let sample_offset = channel
                .checked_mul(program.channel_stride_samples)
                .ok_or(FbResult::FbInvalidArgument)?;
            let sample_byte_offset = sample_offset
                .checked_mul(mem::size_of::<f32>())
                .ok_or(FbResult::FbInvalidArgument)?;
            let sample_address = program
                .samples
                .addr()
                .checked_add(sample_byte_offset)
                .ok_or(FbResult::FbInvalidArgument)?;
            if !sample_address.is_multiple_of(mem::align_of::<f32>()) {
                return Err(FbResult::FbInvalidArgument);
            }
            *plane = program.samples.wrapping_add(sample_offset);
            insert_nonoverlapping_range(
                &mut ranges,
                &mut range_count,
                byte_range(sample_address, block_size, mem::size_of::<f32>())?,
            )?;
            // Safety: the caller promises readable storage and the declared
            // capacity covers this exact active plane.
            let samples = unsafe { slice::from_raw_parts(*plane, block_size) };
            if samples.iter().any(|sample| !sample.is_finite()) {
                return Err(FbResult::FbInvalidArgument);
            }
        }
    }

    insert_planar_ranges(
        &mut ranges,
        &mut range_count,
        &block.direct_output,
        FB_MAX_PRESENTATION_FEEDS_V2 as usize,
        block_size,
    )?;
    insert_planar_ranges(
        &mut ranges,
        &mut range_count,
        &block.environmental_output,
        FB_MAX_ENVIRONMENTAL_CHANNELS_V2 as usize,
        block_size,
    )?;
    for slot in 0..FB_MAX_PRESENTATION_FEEDS_V2 as usize {
        let offset = slot
            .checked_mul(feed_metadata_stride)
            .ok_or(FbResult::FbInvalidArgument)?;
        let address = block
            .feed_metadata
            .addr()
            .checked_add(offset)
            .ok_or(FbResult::FbInvalidArgument)?;
        insert_nonoverlapping_range(
            &mut ranges,
            &mut range_count,
            byte_range(address, 1, mem::size_of::<FbPresentationFeedMetadataV2>())?,
        )?;
    }
    insert_nonoverlapping_range(
        &mut ranges,
        &mut range_count,
        byte_range(
            block.block_metadata.addr(),
            1,
            mem::size_of::<FbSpatialBlockMetadataV2>(),
        )?,
    )?;

    Ok(ValidatedSpatialRenderBlockV2 {
        programs,
        program_count,
        direct_samples: block.direct_output.samples,
        direct_stride_samples: block.direct_output.plane_stride_samples,
        environmental_samples: block.environmental_output.samples,
        environmental_stride_samples: block.environmental_output.plane_stride_samples,
        feed_metadata: block.feed_metadata,
        feed_metadata_stride_bytes: feed_metadata_stride,
        block_metadata: block.block_metadata,
    })
}

fn validate_planar_output(
    output: &FbPlanarOutputV2,
    required_planes: usize,
    block_size: usize,
) -> Result<(), FbResult> {
    if !valid_mut_pointer(output.samples)
        || usize::try_from(output.plane_capacity)
            .map_or(true, |capacity| capacity < required_planes)
        || output.plane_stride_samples < block_size
        || output.reserved_u32 != 0
        || output.reserved.iter().any(|value| *value != 0)
    {
        return Err(FbResult::FbInvalidArgument);
    }
    let required_samples =
        required_strided_elements(required_planes, output.plane_stride_samples, block_size)?;
    if output.sample_capacity < required_samples {
        return Err(FbResult::FbInvalidArgument);
    }
    byte_range(
        output.samples.addr(),
        required_samples,
        mem::size_of::<f32>(),
    )?;
    Ok(())
}

fn required_strided_elements(
    count: usize,
    stride: usize,
    element_span: usize,
) -> Result<usize, FbResult> {
    if count == 0 {
        return Ok(0);
    }
    count
        .checked_sub(1)
        .and_then(|last| last.checked_mul(stride))
        .and_then(|offset| offset.checked_add(element_span))
        .ok_or(FbResult::FbInvalidArgument)
}

fn insert_planar_ranges(
    ranges: &mut [ByteRange],
    range_count: &mut usize,
    output: &FbPlanarOutputV2,
    plane_count: usize,
    block_size: usize,
) -> Result<(), FbResult> {
    for plane in 0..plane_count {
        let sample_offset = plane
            .checked_mul(output.plane_stride_samples)
            .ok_or(FbResult::FbInvalidArgument)?;
        let address = output
            .samples
            .addr()
            .checked_add(
                sample_offset
                    .checked_mul(mem::size_of::<f32>())
                    .ok_or(FbResult::FbInvalidArgument)?,
            )
            .ok_or(FbResult::FbInvalidArgument)?;
        if !address.is_multiple_of(mem::align_of::<f32>()) {
            return Err(FbResult::FbInvalidArgument);
        }
        insert_nonoverlapping_range(
            ranges,
            range_count,
            byte_range(address, block_size, mem::size_of::<f32>())?,
        )?;
    }
    Ok(())
}

fn insert_nonoverlapping_range(
    ranges: &mut [ByteRange],
    count: &mut usize,
    candidate: ByteRange,
) -> Result<(), FbResult> {
    if ranges[..*count]
        .iter()
        .any(|existing| candidate.start < existing.end && existing.start < candidate.end)
        || *count >= ranges.len()
    {
        return Err(FbResult::FbInvalidArgument);
    }
    ranges[*count] = candidate;
    *count += 1;
    Ok(())
}

fn byte_range(address: usize, count: usize, element_size: usize) -> Result<ByteRange, FbResult> {
    let bytes = count
        .checked_mul(element_size)
        .ok_or(FbResult::FbInvalidArgument)?;
    let end = address
        .checked_add(bytes)
        .ok_or(FbResult::FbInvalidArgument)?;
    Ok(ByteRange {
        start: address,
        end,
    })
}

fn valid_const_pointer<T>(pointer: *const T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(mem::align_of::<T>())
}

fn valid_mut_pointer<T>(pointer: *mut T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(mem::align_of::<T>())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ValidBuffers {
        input: [f32; 16],
        programs: [FbSourceProgramInputV2; 1],
        direct: Vec<f32>,
        environmental: Vec<f32>,
        feed_metadata: [FbPresentationFeedMetadataV2; 48],
        block_metadata: FbSpatialBlockMetadataV2,
        block: FbSpatialRenderBlockV2,
    }

    impl ValidBuffers {
        fn new() -> Box<Self> {
            let input = [0.25; 16];
            let programs = [FbSourceProgramInputV2 {
                source_index: 0,
                channel_count: 2,
                samples: ptr::null(),
                sample_count: 16,
                channel_stride_samples: 8,
                reserved: [0; 4],
            }];
            let direct = vec![0.0; 48 * 8];
            let environmental = vec![0.0; 9 * 8];
            let feed_metadata = [FbPresentationFeedMetadataV2::default(); 48];
            let block_metadata = FbSpatialBlockMetadataV2::default();
            let mut value = Box::new(Self {
                input,
                programs,
                direct,
                environmental,
                feed_metadata,
                block_metadata,
                block: FbSpatialRenderBlockV2::default(),
            });
            value.refresh_pointers();
            value
        }

        fn refresh_pointers(&mut self) {
            self.programs[0].samples = self.input.as_ptr();
            self.block.source_programs = self.programs.as_ptr();
            self.block.source_program_count = 1;
            self.block.direct_output = FbPlanarOutputV2 {
                samples: self.direct.as_mut_ptr(),
                sample_capacity: self.direct.len(),
                plane_capacity: 48,
                reserved_u32: 0,
                plane_stride_samples: 8,
                reserved: [0; 4],
            };
            self.block.environmental_output = FbPlanarOutputV2 {
                samples: self.environmental.as_mut_ptr(),
                sample_capacity: self.environmental.len(),
                plane_capacity: 9,
                reserved_u32: 0,
                plane_stride_samples: 8,
                reserved: [0; 4],
            };
            self.block.feed_metadata = self.feed_metadata.as_mut_ptr();
            self.block.feed_metadata_capacity = 48;
            self.block.block_metadata = &mut self.block_metadata;
        }

        fn validate(&self) -> Result<ValidatedSpatialRenderBlockV2, FbResult> {
            // Safety: this fixture owns every declared buffer for the call.
            unsafe { validate_spatial_render_block_v2(&self.block, 1, &[2; 16], 8) }
        }
    }

    #[test]
    fn v2_construction_structures_are_explicitly_versioned_and_reserved() {
        assert_eq!(mem::size_of::<FbSessionConfigV2>(), 64);
        assert_eq!(mem::align_of::<FbSessionConfigV2>(), 4);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, abi_version), 0);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, struct_size), 4);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, sample_rate_hz), 8);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, block_size_frames), 12);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, source_count), 16);
        assert_eq!(
            mem::offset_of!(FbSessionConfigV2, default_source_level_db),
            20
        );
        assert_eq!(mem::offset_of!(FbSessionConfigV2, quality_tier), 24);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, render_route), 28);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, environmental_order), 32);
        assert_eq!(mem::offset_of!(FbSessionConfigV2, reserved), 36);

        assert_eq!(mem::size_of::<FbControlFrameV2>(), 104);
        assert_eq!(mem::align_of::<FbControlFrameV2>(), 8);
        assert_eq!(mem::offset_of!(FbControlFrameV2, abi_version), 0);
        assert_eq!(mem::offset_of!(FbControlFrameV2, struct_size), 4);
        assert_eq!(mem::offset_of!(FbControlFrameV2, source_updates), 8);
        assert_eq!(mem::offset_of!(FbControlFrameV2, source_count), 16);
        assert_eq!(
            mem::offset_of!(FbControlFrameV2, source_update_stride_bytes),
            20
        );
        assert_eq!(mem::offset_of!(FbControlFrameV2, listener_pose), 24);
        assert_eq!(
            mem::offset_of!(FbControlFrameV2, listener_linear_velocity_mps),
            60
        );
        assert_eq!(mem::offset_of!(FbControlFrameV2, reserved), 72);

        assert_eq!(mem::size_of::<FbSourceProgramConfigV2>(), 64);
        assert_eq!(mem::align_of::<FbSourceProgramConfigV2>(), 8);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, abi_version), 0);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, struct_size), 4);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, source_index), 8);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, channel_count), 12);
        assert_eq!(
            mem::offset_of!(FbSourceProgramConfigV2, source_geometry),
            16
        );
        assert_eq!(
            mem::offset_of!(FbSourceProgramConfigV2, multipoint_count),
            20
        );
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, extent_m), 24);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, reserved_f32), 28);
        assert_eq!(mem::offset_of!(FbSourceProgramConfigV2, reserved), 32);

        assert_eq!(mem::size_of::<FbSourceProgramInputV2>(), 64);
        assert_eq!(mem::align_of::<FbSourceProgramInputV2>(), 8);
        assert_eq!(mem::offset_of!(FbSourceProgramInputV2, source_index), 0);
        assert_eq!(mem::offset_of!(FbSourceProgramInputV2, channel_count), 4);
        assert_eq!(mem::offset_of!(FbSourceProgramInputV2, samples), 8);
        assert_eq!(mem::offset_of!(FbSourceProgramInputV2, sample_count), 16);
        assert_eq!(
            mem::offset_of!(FbSourceProgramInputV2, channel_stride_samples),
            24
        );
        assert_eq!(mem::offset_of!(FbSourceProgramInputV2, reserved), 32);

        assert_eq!(mem::size_of::<FbPlanarOutputV2>(), 64);
        assert_eq!(mem::align_of::<FbPlanarOutputV2>(), 8);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, samples), 0);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, sample_capacity), 8);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, plane_capacity), 16);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, reserved_u32), 20);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, plane_stride_samples), 24);
        assert_eq!(mem::offset_of!(FbPlanarOutputV2, reserved), 32);

        assert_eq!(mem::size_of::<FbPresentationFeedMetadataV2>(), 80);
        assert_eq!(mem::align_of::<FbPresentationFeedMetadataV2>(), 4);
        assert_eq!(mem::offset_of!(FbPresentationFeedMetadataV2, valid), 0);
        assert_eq!(
            mem::offset_of!(FbPresentationFeedMetadataV2, source_index),
            4
        );
        assert_eq!(mem::offset_of!(FbPresentationFeedMetadataV2, component), 8);
        assert_eq!(mem::offset_of!(FbPresentationFeedMetadataV2, placement), 12);
        assert_eq!(mem::offset_of!(FbPresentationFeedMetadataV2, pose), 16);
        assert_eq!(
            mem::offset_of!(FbPresentationFeedMetadataV2, direction_enu),
            52
        );
        assert_eq!(
            mem::offset_of!(FbPresentationFeedMetadataV2, processing_latency_frames),
            64
        );
        assert_eq!(mem::offset_of!(FbPresentationFeedMetadataV2, reserved), 68);

        assert_eq!(mem::size_of::<FbSpatialBlockMetadataV2>(), 120);
        assert_eq!(mem::align_of::<FbSpatialBlockMetadataV2>(), 8);
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, abi_version), 0);
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, struct_size), 4);
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, sample_rate_hz), 8);
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, block_size_frames),
            12
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, block_start_frame),
            16
        );
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, generation), 24);
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, discontinuity_sequence),
            32
        );
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, validity), 40);
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, active_presentation_feed_count),
            44
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_order),
            48
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_channel_count),
            52
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_latency_frames),
            56
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, component_mask),
            60
        );
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, flags), 64);
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_channel_order),
            68
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_normalization),
            72
        );
        assert_eq!(
            mem::offset_of!(FbSpatialBlockMetadataV2, environmental_basis),
            76
        );
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, reserved_u32), 80);
        assert_eq!(mem::offset_of!(FbSpatialBlockMetadataV2, reserved), 88);

        assert_eq!(mem::size_of::<FbSpatialRenderBlockV2>(), 208);
        assert_eq!(mem::align_of::<FbSpatialRenderBlockV2>(), 8);
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, abi_version), 0);
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, struct_size), 4);
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, source_programs), 8);
        assert_eq!(
            mem::offset_of!(FbSpatialRenderBlockV2, source_program_count),
            16
        );
        assert_eq!(
            mem::offset_of!(FbSpatialRenderBlockV2, source_program_stride_bytes),
            20
        );
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, direct_output), 24);
        assert_eq!(
            mem::offset_of!(FbSpatialRenderBlockV2, environmental_output),
            88
        );
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, feed_metadata), 152);
        assert_eq!(
            mem::offset_of!(FbSpatialRenderBlockV2, feed_metadata_capacity),
            160
        );
        assert_eq!(
            mem::offset_of!(FbSpatialRenderBlockV2, feed_metadata_stride_bytes),
            164
        );
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, block_metadata), 168);
        assert_eq!(mem::offset_of!(FbSpatialRenderBlockV2, reserved), 176);

        assert_eq!(
            validate_session_config_v2(&FbSessionConfigV2::default()),
            Ok(())
        );

        let mut config = FbSessionConfigV2::default();
        config.abi_version = 1;
        assert_eq!(
            validate_session_config_v2(&config),
            Err(FbResult::FbInvalidArgument)
        );
        config.abi_version = FB_ABI_VERSION_V2;
        config.struct_size -= 1;
        assert_eq!(
            validate_session_config_v2(&config),
            Err(FbResult::FbInvalidArgument)
        );
        config.struct_size = mem::size_of::<FbSessionConfigV2>() as u32;
        config.reserved[6] = 1;
        assert_eq!(
            validate_session_config_v2(&config),
            Err(FbResult::FbInvalidArgument)
        );
    }

    #[test]
    fn v2_control_frame_copies_exact_or_extended_source_prefixes_before_use() {
        #[repr(C)]
        #[derive(Clone, Copy)]
        struct ExtendedSourceUpdate {
            current: FbSourceUpdate,
            caller_extension: u32,
        }

        let source = |active, east_m| FbSourceUpdate {
            active,
            pose: FbPose {
                position: FbVec3 {
                    east_m,
                    north_m: 2.0,
                    up_m: 3.0,
                },
                forward: FbVec3 {
                    east_m: 0.0,
                    north_m: 1.0,
                    up_m: 0.0,
                },
                up: FbVec3 {
                    east_m: 0.0,
                    north_m: 0.0,
                    up_m: 1.0,
                },
            },
            linear_velocity_mps: FbVec3 {
                east_m: 4.0,
                north_m: 5.0,
                up_m: 6.0,
            },
        };
        let updates = [
            ExtendedSourceUpdate {
                current: source(1, 10.0),
                caller_extension: 0x1122_3344,
            },
            ExtendedSourceUpdate {
                current: source(0, 20.0),
                caller_extension: 0x5566_7788,
            },
        ];
        let frame = FbControlFrameV2 {
            source_updates: updates.as_ptr().cast::<FbSourceUpdate>(),
            source_count: 2,
            source_update_stride_bytes: mem::size_of::<ExtendedSourceUpdate>() as u32,
            ..FbControlFrameV2::default()
        };
        // Safety: both extended records are readable for the call.
        let validated = unsafe { validate_control_frame_v2(&frame, 2) }.unwrap();
        assert_eq!(validated.source_updates[0].active, 1);
        assert_eq!(validated.source_updates[0].pose.position.east_m, 10.0);
        assert_eq!(validated.source_updates[1].active, 0);
        assert_eq!(validated.source_updates[1].pose.position.east_m, 20.0);

        for invalid in [
            FbControlFrameV2 {
                source_count: 1,
                ..frame
            },
            FbControlFrameV2 {
                source_updates: ptr::null(),
                ..frame
            },
            FbControlFrameV2 {
                source_update_stride_bytes: (mem::size_of::<FbSourceUpdate>() - 1) as u32,
                ..frame
            },
            FbControlFrameV2 {
                source_update_stride_bytes: 53,
                ..frame
            },
            FbControlFrameV2 {
                source_updates: updates
                    .as_ptr()
                    .cast::<u8>()
                    .wrapping_add(1)
                    .cast::<FbSourceUpdate>(),
                ..frame
            },
            FbControlFrameV2 {
                reserved: [0, 0, 1, 0],
                ..frame
            },
        ] {
            // Safety: invalid layouts either retain the fixture-owned records
            // or are rejected before any record is read.
            assert_eq!(
                unsafe { validate_control_frame_v2(&invalid, 2) }.unwrap_err(),
                FbResult::FbInvalidArgument
            );
        }

        let overflowing = FbControlFrameV2 {
            source_updates: ptr::without_provenance(usize::MAX & !3),
            source_update_stride_bytes: mem::size_of::<FbSourceUpdate>() as u32,
            ..frame
        };
        // Safety: the forged address is rejected by checked span arithmetic
        // before any record read is attempted.
        assert_eq!(
            unsafe { validate_control_frame_v2(&overflowing, 2) }.unwrap_err(),
            FbResult::FbInvalidArgument
        );
    }

    #[test]
    fn v2_discriminants_component_slots_and_flags_are_frozen() {
        assert_eq!(
            FB_MULTIPOINT_FIXED_EXTENT_METERS_V2.to_bits(),
            1.0_f32.to_bits()
        );
        assert_eq!(FB_MAX_MULTIPOINT_COUNT_V2, u8::MAX as u32);
        assert_eq!(mem::size_of::<FbRenderRouteV2>(), 4);
        assert_eq!(FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32, 0);
        assert_eq!(FbRenderRouteV2::FbRenderNeutralSpatialV2 as u32, 1);
        assert_eq!(FbSourceGeometryV2::FbSourceGeometryPointV2 as u32, 0);
        assert_eq!(FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32, 1);
        assert_eq!(FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32, 2);
        assert_eq!(FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32, 3);
        assert_eq!(
            FbEnvironmentalChannelOrderV2::FbEnvironmentalAcnV2 as u32,
            0
        );
        assert_eq!(
            FbEnvironmentalNormalizationV2::FbEnvironmentalN3dV2 as u32,
            0
        );
        assert_eq!(
            FbEnvironmentalBasisV2::FbEnvironmentalRightHandedEnuV2 as u32,
            0
        );
        assert_eq!(
            FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2 as u32,
            1
        );
        assert_eq!(
            FbEnvironmentalBasisV2::try_from(0),
            Ok(FbEnvironmentalBasisV2::FbEnvironmentalRightHandedEnuV2)
        );
        assert_eq!(
            FbEnvironmentalBasisV2::try_from(1),
            Ok(FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2)
        );
        assert_eq!(
            FbEnvironmentalBasisV2::try_from(2),
            Err(FbResult::FbInvalidArgument)
        );
        assert_eq!(FbSpatialOutputValidityV2::FbSpatialInvalidV2 as u32, 0);
        assert_eq!(FbSpatialOutputValidityV2::FbSpatialValidV2 as u32, 1);
        assert_eq!(
            FbSpatialOutputValidityV2::FbSpatialSilentDiscontinuityV2 as u32,
            2
        );
        assert_eq!(FbPresentationPlacementV2::FbPresentationPoseV2 as u32, 0);
        assert_eq!(
            FbPresentationPlacementV2::FbPresentationDirectionV2 as u32,
            1
        );

        assert_eq!(FB_PRESENTATION_PLANE_DIRECT_CENTER_OFFSET_V2, 0);
        assert_eq!(FB_PRESENTATION_PLANE_WIDTH_POSITIVE_OFFSET_V2, 1);
        assert_eq!(FB_PRESENTATION_PLANE_WIDTH_NEGATIVE_OFFSET_V2, 2);
        assert_eq!(FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2, 1 << 0);
        assert_eq!(FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2, 1 << 1);
        assert_eq!(FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2, 1 << 2);
        assert_eq!(FB_PRESENTATION_COMPONENT_DISCRETE_ECHO_V2, 1 << 3);
        assert_eq!(FB_SPATIAL_BLOCK_VALID_V2, 1 << 0);
        assert_eq!(FB_SPATIAL_BLOCK_DISCONTINUITY_V2, 1 << 1);
        assert_eq!(FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2, 1 << 2);
        assert_eq!(FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2, 1 << 3);
        assert_eq!(FB_SPATIAL_WORLD_UNROTATED_V2, 1 << 4);
        assert_eq!(FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2, 1 << 5);
        assert_eq!(FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2, 1 << 6);
        assert_eq!(FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2, 1 << 7);
    }

    #[test]
    fn v2_prefix_copy_reads_only_the_universal_header_before_size_and_alignment() {
        #[repr(C, align(8))]
        struct GuardedShortHeader {
            header: FbAbiHeaderV2,
            canary: [u64; 2],
        }

        let short = GuardedShortHeader {
            header: FbAbiHeaderV2 {
                abi_version: FB_ABI_VERSION_V2,
                struct_size: mem::size_of::<FbAbiHeaderV2>() as u32,
            },
            canary: [0x1122_3344_5566_7788, 0x8877_6655_4433_2211],
        };
        // Safety: exactly the universal header is contractually readable. Its
        // declared size rejects every current-prefix copy.
        assert_eq!(
            unsafe {
                copy_session_config_v2(
                    (&short.header as *const FbAbiHeaderV2).cast::<FbSessionConfigV2>(),
                )
            }
            .unwrap_err(),
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            unsafe {
                copy_source_program_config_v2(
                    (&short.header as *const FbAbiHeaderV2).cast::<FbSourceProgramConfigV2>(),
                )
            }
            .unwrap_err(),
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            unsafe {
                copy_control_frame_v2(
                    (&short.header as *const FbAbiHeaderV2).cast::<FbControlFrameV2>(),
                )
            }
            .unwrap_err(),
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            unsafe {
                copy_spatial_render_block_v2(
                    (&short.header as *const FbAbiHeaderV2).cast::<FbSpatialRenderBlockV2>(),
                )
            }
            .unwrap_err(),
            FbResult::FbInvalidArgument
        );
        assert_eq!(short.canary, [0x1122_3344_5566_7788, 0x8877_6655_4433_2211]);

        let mut bytes = [0_u8; mem::size_of::<FbSpatialRenderBlockV2>() + 8];
        let base = bytes.as_mut_ptr();
        let offset = (0..8)
            .find(|offset| {
                !base
                    .wrapping_add(*offset)
                    .addr()
                    .is_multiple_of(mem::align_of::<FbSpatialRenderBlockV2>())
            })
            .expect("one byte offset must be misaligned");
        let misaligned = base.wrapping_add(offset);
        // Safety: the byte allocation holds the unaligned universal header.
        unsafe {
            ptr::write_unaligned(
                misaligned.cast::<FbAbiHeaderV2>(),
                FbAbiHeaderV2 {
                    abi_version: FB_ABI_VERSION_V2,
                    struct_size: mem::size_of::<FbSpatialRenderBlockV2>() as u32,
                },
            );
        }
        // Safety: the header is readable unaligned; the helper must reject the
        // full-structure alignment before copying it.
        assert_eq!(
            unsafe { copy_spatial_render_block_v2(misaligned.cast::<FbSpatialRenderBlockV2>()) }
                .unwrap_err(),
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            unsafe { copy_control_frame_v2(misaligned.cast::<FbControlFrameV2>()) }.unwrap_err(),
            FbResult::FbInvalidArgument
        );

        let extended = FbSessionConfigV2 {
            struct_size: (mem::size_of::<FbSessionConfigV2>() + 64) as u32,
            ..FbSessionConfigV2::default()
        };
        // Safety: the current readable prefix is present; a larger declared
        // additive tail is ignored by this ABI version.
        let copied = unsafe { copy_session_config_v2(&extended) }.expect("extended prefix");
        assert_eq!(copied.struct_size, extended.struct_size);
    }

    #[test]
    fn v2_session_config_rejects_every_invalid_scalar_and_reserved_value() {
        let default = FbSessionConfigV2::default();
        let legacy = FbSessionConfigV2 {
            render_route: FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32,
            environmental_order: 0,
            ..default
        };
        assert_eq!(validate_session_config_v2(&default), Ok(()));
        assert_eq!(validate_session_config_v2(&legacy), Ok(()));

        let invalid = [
            FbSessionConfigV2 {
                sample_rate_hz: 0,
                ..default
            },
            FbSessionConfigV2 {
                sample_rate_hz: i32::MAX as u32 + 1,
                ..default
            },
            FbSessionConfigV2 {
                block_size_frames: 0,
                ..default
            },
            FbSessionConfigV2 {
                block_size_frames: i32::MAX as u32 + 1,
                ..default
            },
            FbSessionConfigV2 {
                source_count: 0,
                ..default
            },
            FbSessionConfigV2 {
                source_count: 17,
                ..default
            },
            FbSessionConfigV2 {
                default_source_level_db: f32::NAN,
                ..default
            },
            FbSessionConfigV2 {
                default_source_level_db: f32::INFINITY,
                ..default
            },
            FbSessionConfigV2 {
                quality_tier: u32::MAX,
                ..default
            },
            FbSessionConfigV2 {
                render_route: u32::MAX,
                ..default
            },
            FbSessionConfigV2 {
                environmental_order: FB_MAX_ENVIRONMENTAL_ORDER_V2 + 1,
                ..default
            },
            FbSessionConfigV2 {
                render_route: FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32,
                environmental_order: 1,
                ..default
            },
            FbSessionConfigV2 {
                reserved: [0, 0, 0, 7, 0, 0, 0],
                ..default
            },
        ];
        for config in invalid {
            assert_eq!(
                validate_session_config_v2(&config),
                Err(FbResult::FbInvalidArgument)
            );
        }
    }

    #[test]
    fn v2_source_shape_rejects_contract_incompatible_geometry() {
        let point = FbSourceProgramConfigV2::default();
        let multipoint = FbSourceProgramConfigV2 {
            source_geometry: FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32,
            multipoint_count: 32,
            extent_m: FB_MULTIPOINT_FIXED_EXTENT_METERS_V2,
            ..point
        };
        let line = FbSourceProgramConfigV2 {
            source_geometry: FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32,
            extent_m: 2.0,
            ..point
        };
        let mono_image = FbSourceProgramConfigV2 {
            source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
            extent_m: 2.0,
            ..point
        };
        let stereo_image = FbSourceProgramConfigV2 {
            channel_count: 2,
            ..mono_image
        };
        for valid in [point, multipoint, line, mono_image, stereo_image] {
            assert_eq!(validate_source_program_config_v2(&valid, 1), Ok(()));
        }

        let invalid = [
            FbSourceProgramConfigV2 {
                abi_version: 1,
                ..point
            },
            FbSourceProgramConfigV2 {
                struct_size: (mem::size_of::<FbSourceProgramConfigV2>() - 1) as u32,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_index: 1,
                ..point
            },
            FbSourceProgramConfigV2 {
                channel_count: 0,
                ..point
            },
            FbSourceProgramConfigV2 {
                channel_count: 3,
                ..point
            },
            FbSourceProgramConfigV2 {
                channel_count: 2,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: u32::MAX,
                ..point
            },
            FbSourceProgramConfigV2 {
                extent_m: 1.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32,
                multipoint_count: 0,
                extent_m: FB_MULTIPOINT_FIXED_EXTENT_METERS_V2,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32,
                multipoint_count: FB_MAX_MULTIPOINT_COUNT_V2 + 1,
                extent_m: FB_MULTIPOINT_FIXED_EXTENT_METERS_V2,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32,
                multipoint_count: 32,
                extent_m: 0.5,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32,
                channel_count: 2,
                multipoint_count: 32,
                extent_m: FB_MULTIPOINT_FIXED_EXTENT_METERS_V2,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32,
                extent_m: 0.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32,
                channel_count: 2,
                extent_m: 1.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
                extent_m: f32::NAN,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
                extent_m: -1.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
                multipoint_count: 1,
                extent_m: 1.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                reserved_f32: -0.0,
                ..point
            },
            FbSourceProgramConfigV2 {
                reserved: [0, 0, 1, 0],
                ..point
            },
        ];
        for config in invalid {
            assert_eq!(
                validate_source_program_config_v2(&config, 1),
                Err(FbResult::FbInvalidArgument)
            );
        }
    }

    #[test]
    fn v2_planar_validation_accepts_a_complete_nonoverlapping_stereo_block() {
        let fixture = ValidBuffers::new();
        let validated = fixture.validate().expect("valid block");
        assert_eq!(validated.program_count, 1);
        assert_eq!(validated.programs[0].source_index, 0);
        assert_eq!(validated.programs[0].channel_count, 2);
        assert_eq!(validated.programs[0].planes[0], fixture.input.as_ptr());
        // Safety: the pointer remains within `fixture.input`.
        assert_eq!(validated.programs[0].planes[1], unsafe {
            fixture.input.as_ptr().add(8)
        });
        assert_eq!(validated.direct_stride_samples, 8);
        assert_eq!(
            validated.environmental_samples,
            fixture.environmental.as_ptr().cast_mut()
        );
        assert_eq!(validated.environmental_stride_samples, 8);
        assert_eq!(
            validated.feed_metadata,
            fixture.feed_metadata.as_ptr().cast_mut()
        );
        assert_eq!(validated.feed_metadata_stride_bytes, 80);
        assert_eq!(
            validated.block_metadata,
            &fixture.block_metadata as *const _ as *mut _
        );
    }

    #[test]
    fn v2_planar_validation_rejects_counts_strides_nulls_and_nonfinite_input() {
        let mut fixture = ValidBuffers::new();
        fixture.block.abi_version = 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.struct_size = (mem::size_of::<FbSpatialRenderBlockV2>() - 1) as u32;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.reserved[3] = 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.source_program_count = 2;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.source_programs = ptr::null();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.source_program_stride_bytes =
            (mem::size_of::<FbSourceProgramInputV2>() - 1) as u32;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.source_program_stride_bytes =
            (mem::size_of::<FbSourceProgramInputV2>() + 1) as u32;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].reserved[0] = 1;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].source_index = 1;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].channel_count = 1;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].samples = ptr::null();
        fixture.block.source_programs = fixture.programs.as_ptr();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].sample_count = 15;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].channel_stride_samples = 7;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].channel_stride_samples = usize::MAX;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.samples = ptr::null_mut();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.sample_capacity -= 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.plane_capacity = FB_MAX_PRESENTATION_FEEDS_V2 - 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.plane_stride_samples = 7;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.reserved_u32 = 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.reserved[3] = 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.environmental_output.samples = ptr::null_mut();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.environmental_output.sample_capacity -= 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.environmental_output.plane_capacity = FB_MAX_ENVIRONMENTAL_CHANNELS_V2 - 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.feed_metadata = ptr::null_mut();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.feed_metadata_capacity = FB_MAX_PRESENTATION_FEEDS_V2 - 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.input[9] = f32::NAN;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.input[0] = f32::INFINITY;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.feed_metadata_stride_bytes = 1;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.block_metadata = ptr::null_mut();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block_metadata.abi_version = 1;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block_metadata.struct_size =
            (mem::size_of::<FbSpatialBlockMetadataV2>() - 1) as u32;
        fixture.refresh_pointers();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);
    }

    #[test]
    fn v2_planar_validation_rejects_every_input_output_alias() {
        let mut fixture = ValidBuffers::new();
        fixture.programs[0].samples = fixture.direct.as_ptr();
        fixture.programs[0].sample_count = fixture.direct.len();
        fixture.block.source_programs = fixture.programs.as_ptr();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.environmental_output.samples = fixture.direct.as_mut_ptr();
        fixture.block.environmental_output.sample_capacity = fixture.direct.len();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        let shared_input = [0.25_f32; 8];
        let programs = [
            FbSourceProgramInputV2 {
                source_index: 0,
                channel_count: 1,
                samples: shared_input.as_ptr(),
                sample_count: shared_input.len(),
                channel_stride_samples: shared_input.len(),
                reserved: [0; 4],
            },
            FbSourceProgramInputV2 {
                source_index: 1,
                channel_count: 1,
                samples: shared_input.as_ptr(),
                sample_count: shared_input.len(),
                channel_stride_samples: shared_input.len(),
                reserved: [0; 4],
            },
        ];
        fixture.block.source_programs = programs.as_ptr();
        fixture.block.source_program_count = 2;
        // Safety: both records and the aliased input allocation are readable;
        // the validator must reject the active input/input overlap.
        assert_eq!(
            unsafe { validate_spatial_render_block_v2(&fixture.block, 2, &[1; 16], 8) }
                .unwrap_err(),
            FbResult::FbInvalidArgument
        );

        fixture = ValidBuffers::new();
        let mut record_and_output = Box::new([0_u64; 192]);
        let record = FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 2,
            samples: fixture.input.as_ptr(),
            sample_count: fixture.input.len(),
            channel_stride_samples: 8,
            reserved: [0; 4],
        };
        // Safety: the allocation is sufficiently aligned and large for both
        // the record and the declared direct bank.
        unsafe {
            ptr::write(
                record_and_output
                    .as_mut_ptr()
                    .cast::<FbSourceProgramInputV2>(),
                record,
            );
        }
        fixture.block.source_programs = record_and_output.as_ptr().cast::<FbSourceProgramInputV2>();
        fixture.block.direct_output.samples = record_and_output.as_mut_ptr().cast::<f32>();
        fixture.block.direct_output.sample_capacity = 48 * 8;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        let mut feed_and_output = Box::new([0_u64; 480]);
        fixture.block.direct_output.samples = feed_and_output.as_mut_ptr().cast::<f32>();
        fixture.block.direct_output.sample_capacity = 48 * 8;
        fixture.block.feed_metadata = feed_and_output
            .as_mut_ptr()
            .cast::<FbPresentationFeedMetadataV2>();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        let mut metadata_and_output = Box::new([0_u64; 192]);
        // Safety: the aligned allocation has room for the metadata prefix and
        // the complete direct bank that intentionally aliases it.
        unsafe {
            ptr::write(
                metadata_and_output
                    .as_mut_ptr()
                    .cast::<FbSpatialBlockMetadataV2>(),
                FbSpatialBlockMetadataV2::default(),
            );
        }
        fixture.block.direct_output.samples = metadata_and_output.as_mut_ptr().cast::<f32>();
        fixture.block.direct_output.sample_capacity = 48 * 8;
        fixture.block.block_metadata = metadata_and_output
            .as_mut_ptr()
            .cast::<FbSpatialBlockMetadataV2>();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);
    }

    #[test]
    fn v2_range_validation_rejects_address_and_stride_overflow_before_dereference() {
        let mut fixture = ValidBuffers::new();
        let aligned_max_f32 = usize::MAX & !(mem::align_of::<f32>() - 1);
        fixture.block.direct_output.samples = ptr::without_provenance_mut(aligned_max_f32);
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.programs[0].samples = ptr::without_provenance(aligned_max_f32);
        fixture.block.source_programs = fixture.programs.as_ptr();
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        let aligned_max_record = usize::MAX & !(mem::align_of::<FbSourceProgramInputV2>() - 1);
        fixture.block.source_programs = ptr::without_provenance(aligned_max_record);
        fixture.block.source_program_count = 2;
        // Safety: total record-span arithmetic must reject the forged address
        // before attempting to read either record.
        assert_eq!(
            unsafe { validate_spatial_render_block_v2(&fixture.block, 2, &[2; 16], 8) }
                .unwrap_err(),
            FbResult::FbInvalidArgument
        );

        fixture = ValidBuffers::new();
        let aligned_max_feed = usize::MAX & !(mem::align_of::<FbPresentationFeedMetadataV2>() - 1);
        fixture.block.feed_metadata = ptr::without_provenance_mut(aligned_max_feed);
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);

        fixture = ValidBuffers::new();
        fixture.block.direct_output.plane_stride_samples = usize::MAX;
        fixture.block.direct_output.sample_capacity = usize::MAX;
        assert_eq!(fixture.validate().unwrap_err(), FbResult::FbInvalidArgument);
    }

    #[test]
    fn v2_stride_validation_does_not_touch_padding_or_guard_samples() {
        let mut fixture = ValidBuffers::new();
        fixture.direct.resize(48 * 12, 7.0);
        fixture.environmental.resize(9 * 12, 11.0);
        fixture.direct.fill(7.0);
        fixture.environmental.fill(11.0);
        fixture.block.direct_output.plane_stride_samples = 12;
        fixture.block.environmental_output.plane_stride_samples = 12;
        fixture.refresh_pointers();
        fixture.block.direct_output.plane_stride_samples = 12;
        fixture.block.environmental_output.plane_stride_samples = 12;
        fixture.block.direct_output.sample_capacity = fixture.direct.len();
        fixture.block.environmental_output.sample_capacity = fixture.environmental.len();
        let validated = fixture.validate().expect("padded block");

        // Emulate the exact V2 writer span: block frames only, never padding.
        for plane in 0..48 {
            // Safety: validation proved this active plane range writable.
            unsafe {
                slice::from_raw_parts_mut(
                    validated
                        .direct_samples
                        .add(plane * validated.direct_stride_samples),
                    8,
                )
            }
            .fill(0.0);
        }
        for plane in 0..48 {
            assert!(
                fixture.direct[plane * 12..plane * 12 + 8]
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
            assert!(
                fixture.direct[plane * 12 + 8..plane * 12 + 12]
                    .iter()
                    .all(|sample| *sample == 7.0)
            );
        }
    }
}
