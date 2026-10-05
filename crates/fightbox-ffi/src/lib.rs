//! Defensive C ABI for embedding the retained Fightbox Steam Audio graph.
//!
//! A session has exactly one control-thread owner and one audio-thread owner.
//! Listener/source updates and telemetry queries must be serialized on the
//! control thread. `fb_session_render_block` may run concurrently on one audio
//! thread. Destruction requires both roles to be stopped and joined.

#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    cell::UnsafeCell,
    ffi::{CStr, c_char},
    mem::{align_of, size_of},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    ptr, slice,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};

use fightbox_api::{
    AssetAnalysis, AssetMeasurementProvenance, Directivity, EngineConfig, EnuVector3,
    ExtentDescriptor, ListenerState, Pose, ReferenceLevel, SceneCalibration, SourceId,
    SourceProfile,
};
use fightbox_api::{
    atmosphere::AtmosphereObservation,
    macro_transport::{EventRole, MacroAssetTransport, MacroEmitter, MacroEventId, MacroListener},
};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS, PropagationRenderBlock,
    SimulationRunner, SimulationUpdate, SourceMotion, SpatialAmbisonicChannelOrder,
    SpatialAmbisonicNormalization, SpatialBackendRenderError, SpatialBackendRenderGraph,
    SpatialEnvironmentalBasis, SpatialFeedPlacement, SpatialOutputMetadata, SpatialOutputValidity,
    SpatialPresentationComponent, SpatialProcessBlock, SpatialProgramBlock, SpatialRenderError,
};
use fightbox_runtime::{
    CallbackTimingPublication, CallbackTimingReader, CallbackTimingWriter, CellArtifactIdentity,
    CellIdentity, EventReservationBatch, FrozenAtmosphere, IngressArrivalContext,
    IngressFallbackReason, IngressRenderAuthority, LocalCellAuthority, LocalIngressActivation,
    MacroEventScheduleRequest, MacroIngressTelemetry, MacroLocalIngress, MacroTransportConfig,
    PropagationSnapshot, RealtimeClock, RunTimingHistogram, RuntimeGraph,
    RuntimeGraphMemoryTelemetry, SnapshotPublication, SnapshotReader, SnapshotWriter,
    SourcePropagation, plan_macro_transport,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, GovernorTransitionReason, MAX_MACRO_ECHO_TAPS,
    MacroIngressControlOverlay, MacroIngressRenderSnapshot, MacroIngressSpatialRenderGraph,
    MemoryTrackingStatus, MultiSourceDescriptor, PROBE_BATCH_METADATA_SCHEMA, PathQualityLevel,
    PreparedSteamAudioSpatialWorld, ProbeBatchMetadata, QualityGovernorTelemetry, QualityTier,
    ReflectionQualityLevel, ReverbStrategy, S3SimulationConfig, STEAM_AUDIO_UPSTREAM_COMMIT,
    STEAM_AUDIO_VERSION, SceneMesh, SourceQualityLevel, SpatialCellStreamState,
    SpatialRenderMemoryTelemetry, SteamAudioRenderGraph, SteamAudioSimulationRunner,
    SteamAudioSpatialSimulationRunner, SteamMacroEchoPlan, SteamMacroEchoTap,
    SteamMacroIngressAcknowledgement, SteamMacroIngressAcknowledgementBatch,
    SteamMacroIngressAcknowledgementKind, SteamMacroIngressAcknowledgementPublisher,
    SteamMacroIngressAcknowledgementReceiver, SteamMacroIngressCommand,
    SteamMacroIngressCommandBatch, SteamMacroIngressPublisher, SteamMacroIngressReceiver,
    build_multi_source_session_for_tier, build_spatial_multi_source_session_for_tier,
    macro_ingress_render_snapshot_channel, steam_macro_ingress_acknowledgement_channel,
    steam_macro_ingress_acknowledgement_channel_payload_bytes,
    steam_macro_ingress_activation_channel, steam_macro_ingress_command_channel_payload_bytes,
};
use serde::Deserialize;

mod v2_abi;
mod v3_abi;
pub use v2_abi::*;
pub use v3_abi::*;

/// Stable status returned by every fallible FFI operation.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbResult {
    FbOk = 0,
    FbInvalidArgument = 1,
    FbInvalidState = 2,
    FbIoError = 3,
    FbInvalidPackage = 4,
    FbInvalidBake = 5,
    FbBackendUnavailable = 6,
    FbBackendError = 7,
    FbBufferTooSmall = 8,
    FbPanic = 9,
}

/// Named construction-time quality tier.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FbQualityTier {
    FbQualityDesktop = 0,
    FbQualityMobile = 1,
}

/// Three-component vector in right-handed local ENU coordinates.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbVec3 {
    pub east_m: f32,
    pub north_m: f32,
    pub up_m: f32,
}

/// Position and orientation in right-handed local ENU coordinates.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FbPose {
    pub position: FbVec3,
    pub forward: FbVec3,
    pub up: FbVec3,
}

/// Immutable session construction settings.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSessionConfig {
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub source_count: u32,
    /// Relative source level used by the quality governor.
    pub default_source_level_db: f32,
    /// One of `FbQualityTier`. Zero selects Desktop, preserving legacy
    /// zero-initialized caller behavior. Unknown values are rejected.
    pub quality_tier: u32,
}

impl Default for FbSessionConfig {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            block_size_frames: 512,
            source_count: 1,
            default_source_level_db: 0.0,
            quality_tier: FbQualityTier::FbQualityDesktop as u32,
        }
    }
}

/// Complete control-thread update for one stable source index.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FbSourceUpdate {
    /// Zero means inactive; any other value means active.
    pub active: u8,
    pub pose: FbPose,
    pub linear_velocity_mps: FbVec3,
}

/// Opaque retained session. Its layout is intentionally unavailable to C.
pub struct FbSession {
    _private: [u8; 0],
}

/// Opaque control-thread-owned neutral cell prepared for one live session.
pub struct FbPreparedCell {
    _private: [u8; 0],
}

struct PreparedCellReservation {
    state: Arc<AtomicBool>,
}

impl PreparedCellReservation {
    fn acquire(state: &Arc<AtomicBool>) -> Result<Self, FbResult> {
        state
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| FbResult::FbInvalidState)?;
        Ok(Self {
            state: Arc::clone(state),
        })
    }
}

impl Drop for PreparedCellReservation {
    fn drop(&mut self) {
        self.state.store(false, Ordering::Release);
    }
}

struct PreparedCellInner {
    _reservation: PreparedCellReservation,
    owner_session: usize,
    config: FbCellConfigV2,
    metadata_city_offset_enu: Option<EnuVector3>,
    authority: LocalCellAuthority,
    echo_authority: Option<Arc<fightbox_world::PackageEchoAuthority>>,
    world: Option<PreparedSteamAudioSpatialWorld>,
}

struct ControlState {
    runner: SteamAudioSimulationRunner,
    update: SimulationUpdate,
    update_sequence: u64,
    orientation_writer: SnapshotWriter<ListenerOrientation>,
    active_sources_writer: SnapshotWriter<[bool; MAX_ACTIVE_SOURCES]>,
}

struct RenderState {
    graph: SteamAudioRenderGraph,
    left: Vec<f32>,
    right: Vec<f32>,
    orientation_reader: SnapshotReader<ListenerOrientation>,
    active_sources_reader: SnapshotReader<[bool; MAX_ACTIVE_SOURCES]>,
    realtime_clock: RealtimeClock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacroBridgeReservationState {
    Reserved,
    Ready,
    Committed,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct MacroBridgeReservation {
    token_id: u64,
    events: EventReservationBatch,
    readiness_generation: [u64; EventRole::COUNT],
    ready_mask: u8,
    acknowledged_mask: u8,
    finalized_mask: u8,
    state: MacroBridgeReservationState,
}

impl MacroBridgeReservation {
    fn is_ready(self) -> bool {
        self.events.iter().all(|event| {
            let generation = self.readiness_generation[event.role.index()];
            generation != 0 && generation & 1 == 0
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct MacroEchoBridgeTelemetry {
    committed_queries: u64,
    committed_plans: u64,
    committed_silent_plans: u64,
    committed_taps: u64,
}

struct MacroBridgeControl {
    enabled: bool,
    next_token_id: u64,
    reservation: Option<MacroBridgeReservation>,
    overlay: MacroIngressControlOverlay,
    command_publisher: SteamMacroIngressPublisher,
    acknowledgement_receiver: SteamMacroIngressAcknowledgementReceiver,
    pending_acknowledgements: [Option<SteamMacroIngressAcknowledgement>; EventRole::COUNT],
    echo_authority_pins: [Option<Arc<fightbox_world::PackageEchoAuthority>>; EventRole::COUNT],
    echo_telemetry: MacroEchoBridgeTelemetry,
}

struct SpatialControlState {
    runner: SteamAudioSpatialSimulationRunner,
    active_world_offset_enu: Option<EnuVector3>,
    prepared_cell_reserved: Arc<AtomicBool>,
    macro_ingress: Box<MacroLocalIngress>,
    macro_bridge: MacroBridgeControl,
    macro_atmosphere: FrozenAtmosphere,
    macro_atmosphere_locked: bool,
    active_echo_authority: Option<Arc<fightbox_world::PackageEchoAuthority>>,
    pending_cell_authority: Option<LocalCellAuthority>,
    pending_echo_authority: Option<Arc<fightbox_world::PackageEchoAuthority>>,
    update: SimulationUpdate,
    update_sequence: u64,
    batched_frame_advances: u64,
    granular_listener_advances: u64,
    granular_source_advances: u64,
    listener_published: bool,
    source_published: [bool; MAX_ACTIVE_SOURCES],
    propagation_writer: SnapshotWriter<PropagationSnapshot>,
    timing_reader: CallbackTimingReader,
    callback_timing_run: RunTimingHistogram,
    callback_timing_run_max_observation: Option<u64>,
    preparation: SpatialPreparationTelemetry,
    memory: SpatialBindingMemoryTelemetry,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum MacroAudioRolePhase {
    #[default]
    Empty,
    Active,
    AudioAcknowledged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct MacroAudioRole {
    command: Option<SteamMacroIngressCommand>,
    next_program_frame: u64,
    staged_frame_count: u32,
    phase: MacroAudioRolePhase,
}

struct MacroBridgeAudio {
    enabled: bool,
    command_receiver: SteamMacroIngressReceiver,
    acknowledgement_publisher: SteamMacroIngressAcknowledgementPublisher,
    pending_commands: Option<SteamMacroIngressCommandBatch>,
    route_writer: SnapshotWriter<MacroIngressRenderSnapshot>,
    route_snapshot: MacroIngressRenderSnapshot,
    roles: [MacroAudioRole; EventRole::COUNT],
    pending_acknowledgements: [Option<SteamMacroIngressAcknowledgement>; EventRole::COUNT],
    staged_block_start_frame: Option<u64>,
}

struct SpatialRenderState {
    graph: RuntimeGraph,
    macro_audio: MacroBridgeAudio,
    presentation_bank: Vec<f32>,
    environmental_bank: Vec<f32>,
    metadata: SpatialOutputMetadata,
    timing_writer: CallbackTimingWriter,
}

/// Immutable, exact persistent payload accounting for the V2 render binding.
///
/// `neutral_graph` is already represented in the neutral governor's Steam-side
/// totals. `runtime_graph`, both FFI banks, and the two FFI-created shared
/// publications are the external additions made to the V2 tracked totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpatialBindingMemoryTelemetry {
    neutral_graph: SpatialRenderMemoryTelemetry,
    runtime_graph: RuntimeGraphMemoryTelemetry,
    ffi_presentation_bank_payload_bytes: u64,
    ffi_environmental_bank_payload_bytes: u64,
    propagation_snapshot_publication_payload_bytes: u64,
    callback_timing_publication_payload_bytes: u64,
    macro_route_publication_payload_bytes: u64,
    macro_command_mailbox_payload_bytes: u64,
    macro_acknowledgement_mailbox_payload_bytes: u64,
    macro_render_graph_payload_bytes: u64,
}

impl SpatialBindingMemoryTelemetry {
    fn ffi_bank_payload_bytes(self) -> u64 {
        self.ffi_presentation_bank_payload_bytes
            .saturating_add(self.ffi_environmental_bank_payload_bytes)
    }

    fn external_payload_bytes(self) -> u64 {
        self.runtime_graph
            .total_payload_bytes
            .saturating_add(self.ffi_bank_payload_bytes())
            .saturating_add(self.propagation_snapshot_publication_payload_bytes)
            .saturating_add(self.callback_timing_publication_payload_bytes)
            .saturating_add(self.macro_route_publication_payload_bytes)
            .saturating_add(self.macro_command_mailbox_payload_bytes)
            .saturating_add(self.macro_acknowledgement_mailbox_payload_bytes)
            .saturating_add(self.macro_render_graph_payload_bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionRoute {
    LegacyFinalStereo,
    NeutralSpatial,
}

#[derive(Clone, Copy, Debug, Default)]
struct SpatialSourceShape {
    channel_count: u8,
    source_geometry: u32,
    multipoint_count: u8,
    // Retained even though Wave 0 MultiPoint currently fixes this to 1 m.
    extent_m: f32,
    // Deterministically inferred while the immutable C source shape freezes.
    presentation_provenance: SpatialPresentationProvenance,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SpatialPresentationProvenance {
    #[default]
    NativeMono,
    MonoExpanded,
    AuthoredStereo,
}

impl SpatialPresentationProvenance {
    fn infer(source_geometry: u32, channel_count: u32) -> Self {
        if source_geometry == FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32 {
            if channel_count == 1 {
                Self::MonoExpanded
            } else {
                Self::AuthoredStereo
            }
        } else {
            Self::NativeMono
        }
    }
}

struct SpatialShellState {
    source_shapes: [SpatialSourceShape; MAX_ACTIVE_SOURCES],
    configured: [bool; MAX_ACTIVE_SOURCES],
    configured_count: usize,
    macro_bridge_requested: bool,
    macro_diffuse_profile: fightbox_api::diffuse::DiffuseFieldProfile,
    // Retained until the final immutable source shape lets construction bind
    // the neutral simulation/runtime/render pair atomically.
    build_inputs: Option<SpatialBuildInputs>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpatialPreparationTelemetry {
    attempts: u64,
    successes: u64,
    failures: u64,
    latest_duration_ns: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SpatialControlScheduleTelemetry {
    cadence_advances: u64,
    batched_frame_advances: u64,
    granular_listener_advances: u64,
    granular_source_advances: u64,
}

struct SpatialBuildInputs {
    mesh: SceneMesh,
    baked: BakedProbeBatch,
    cell_authority: CellAuthoritySeed,
    audio: AudioConfig,
    simulation: S3SimulationConfig,
    quality_tier: QualityTier,
    default_source_level_db: f32,
    environmental_order: usize,
}

#[derive(Clone, Debug)]
struct CellAuthoritySeed {
    cell: CellIdentity,
    metadata_city_offset_enu: Option<EnuVector3>,
    package_content_sha256: String,
    probe_bake_content_sha256: String,
    echo_authority: Option<Arc<fightbox_world::PackageEchoAuthority>>,
}

impl CellAuthoritySeed {
    fn bind(&self, world_generation: u64) -> LocalCellAuthority {
        let artifact = |content_sha256: &str| {
            CellArtifactIdentity::new(
                self.cell.clone(),
                world_generation,
                content_sha256.to_owned(),
            )
        };
        LocalCellAuthority {
            cell: self.cell.clone(),
            world_generation,
            package: Some(artifact(&self.package_content_sha256)),
            probe_bake: Some(artifact(&self.probe_bake_content_sha256)),
            echo_authority: self
                .echo_authority
                .as_ref()
                .map(|authority| artifact(&authority.content_sha256)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SpatialShellLifecycle {
    /// Immutable source shapes are still being collected on the control thread.
    Collecting = 0,
    /// All shapes and the immutable simulation/runtime/render pair are bound,
    /// but the explicit control-thread preparation barrier has not succeeded.
    BoundUnprepared = 1,
    /// Preparation succeeded and no public callback has advanced yet.
    PreparedNotStarted = 2,
    /// Private ownership claim held by the first validated audio callback.
    Starting = 3,
    /// At least one public callback advanced RuntimeGraph.
    Running = 4,
}

const SPATIAL_LIFECYCLE_BITS: u32 = 3;
const SPATIAL_LIFECYCLE_MASK: u64 = (1 << SPATIAL_LIFECYCLE_BITS) - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SpatialLifecycleSnapshot {
    raw: u64,
    lifecycle: SpatialShellLifecycle,
    control_update_epoch: u64,
}

struct SpatialLifecycleState {
    packed: AtomicU64,
}

impl SpatialLifecycleState {
    fn new(lifecycle: SpatialShellLifecycle) -> Self {
        Self {
            packed: AtomicU64::new(lifecycle as u64),
        }
    }

    fn decode(raw: u64) -> SpatialLifecycleSnapshot {
        let lifecycle = match raw & SPATIAL_LIFECYCLE_MASK {
            value if value == SpatialShellLifecycle::Collecting as u64 => {
                SpatialShellLifecycle::Collecting
            }
            value if value == SpatialShellLifecycle::BoundUnprepared as u64 => {
                SpatialShellLifecycle::BoundUnprepared
            }
            value if value == SpatialShellLifecycle::PreparedNotStarted as u64 => {
                SpatialShellLifecycle::PreparedNotStarted
            }
            value if value == SpatialShellLifecycle::Starting as u64 => {
                SpatialShellLifecycle::Starting
            }
            value if value == SpatialShellLifecycle::Running as u64 => {
                SpatialShellLifecycle::Running
            }
            _ => unreachable!("invalid internal spatial lifecycle"),
        };
        SpatialLifecycleSnapshot {
            raw,
            lifecycle,
            control_update_epoch: raw >> SPATIAL_LIFECYCLE_BITS,
        }
    }

    fn snapshot(&self) -> SpatialLifecycleSnapshot {
        Self::decode(self.packed.load(Ordering::Acquire))
    }

    fn lifecycle(&self) -> SpatialShellLifecycle {
        self.snapshot().lifecycle
    }

    fn replace_lifecycle(
        &self,
        expected: SpatialLifecycleSnapshot,
        lifecycle: SpatialShellLifecycle,
    ) -> Result<SpatialLifecycleSnapshot, SpatialLifecycleSnapshot> {
        let desired = (expected.raw & !SPATIAL_LIFECYCLE_MASK) | lifecycle as u64;
        self.packed
            .compare_exchange(expected.raw, desired, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self::decode(desired))
            .map_err(Self::decode)
    }

    fn mark_bound_after_configuration(&self) {
        let collecting = self.snapshot();
        debug_assert_eq!(collecting.lifecycle, SpatialShellLifecycle::Collecting);
        self.replace_lifecycle(collecting, SpatialShellLifecycle::BoundUnprepared)
            .expect("final binding is the sole construction transition");
    }

    fn begin_prepare(&self) -> Result<(), FbResult> {
        loop {
            let snapshot = self.snapshot();
            match snapshot.lifecycle {
                SpatialShellLifecycle::BoundUnprepared => return Ok(()),
                SpatialShellLifecycle::PreparedNotStarted => {
                    if self
                        .replace_lifecycle(snapshot, SpatialShellLifecycle::BoundUnprepared)
                        .is_ok()
                    {
                        return Ok(());
                    }
                }
                SpatialShellLifecycle::Collecting
                | SpatialShellLifecycle::Starting
                | SpatialShellLifecycle::Running => return Err(FbResult::FbInvalidState),
            }
        }
    }

    fn mark_prepared(&self) {
        loop {
            let snapshot = self.snapshot();
            debug_assert_eq!(snapshot.lifecycle, SpatialShellLifecycle::BoundUnprepared);
            if self
                .replace_lifecycle(snapshot, SpatialShellLifecycle::PreparedNotStarted)
                .is_ok()
            {
                return;
            }
        }
    }

    fn note_control_update(&self) {
        loop {
            let snapshot = self.snapshot();
            let lifecycle = match snapshot.lifecycle {
                SpatialShellLifecycle::PreparedNotStarted => SpatialShellLifecycle::BoundUnprepared,
                lifecycle => lifecycle,
            };
            let next_epoch = snapshot.control_update_epoch.wrapping_add(1);
            let desired = (next_epoch << SPATIAL_LIFECYCLE_BITS) | lifecycle as u64;
            if self
                .packed
                .compare_exchange(snapshot.raw, desired, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return;
            }
        }
    }

    fn claim_first_render(&self) -> Result<Option<u64>, FbResult> {
        loop {
            let snapshot = self.snapshot();
            match snapshot.lifecycle {
                SpatialShellLifecycle::Running => return Ok(None),
                SpatialShellLifecycle::PreparedNotStarted => {
                    if self
                        .replace_lifecycle(snapshot, SpatialShellLifecycle::Starting)
                        .is_ok()
                    {
                        return Ok(Some(snapshot.control_update_epoch));
                    }
                }
                SpatialShellLifecycle::Collecting
                | SpatialShellLifecycle::BoundUnprepared
                | SpatialShellLifecycle::Starting => return Err(FbResult::FbInvalidState),
            }
        }
    }

    fn finish_first_render_success(&self) {
        loop {
            let snapshot = self.snapshot();
            debug_assert_eq!(snapshot.lifecycle, SpatialShellLifecycle::Starting);
            if self
                .replace_lifecycle(snapshot, SpatialShellLifecycle::Running)
                .is_ok()
            {
                return;
            }
        }
    }

    fn finish_first_render_failure(&self, claimed_epoch: u64) {
        loop {
            let snapshot = self.snapshot();
            debug_assert_eq!(snapshot.lifecycle, SpatialShellLifecycle::Starting);
            let lifecycle = if snapshot.control_update_epoch == claimed_epoch {
                SpatialShellLifecycle::PreparedNotStarted
            } else {
                SpatialShellLifecycle::BoundUnprepared
            };
            if self.replace_lifecycle(snapshot, lifecycle).is_ok() {
                return;
            }
        }
    }

    fn public_name(&self) -> &'static str {
        match self.lifecycle() {
            SpatialShellLifecycle::Collecting => "collecting",
            SpatialShellLifecycle::BoundUnprepared => "bound_unprepared",
            SpatialShellLifecycle::PreparedNotStarted | SpatialShellLifecycle::Starting => {
                "prepared_not_started"
            }
            SpatialShellLifecycle::Running => "running",
        }
    }
}

impl SpatialShellState {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_build_inputs(None)
    }

    fn with_build_inputs(build_inputs: Option<SpatialBuildInputs>) -> Self {
        Self {
            source_shapes: [SpatialSourceShape::default(); MAX_ACTIVE_SOURCES],
            configured: [false; MAX_ACTIVE_SOURCES],
            configured_count: 0,
            macro_bridge_requested: false,
            macro_diffuse_profile: fightbox_api::diffuse::DiffuseFieldProfile::OFF,
            build_inputs,
        }
    }

    fn stage_source_shape(
        &mut self,
        source_index: usize,
        shape: SpatialSourceShape,
    ) -> Result<(), FbResult> {
        if self.configured[source_index] {
            return Err(FbResult::FbInvalidState);
        }
        self.source_shapes[source_index] = shape;
        self.configured[source_index] = true;
        self.configured_count += 1;
        Ok(())
    }

    fn rollback_source_shape(&mut self, source_index: usize) {
        debug_assert!(self.configured[source_index]);
        self.source_shapes[source_index] = SpatialSourceShape::default();
        self.configured[source_index] = false;
        self.configured_count -= 1;
    }

    fn mark_bound(&mut self, source_count: usize) {
        debug_assert_eq!(self.configured_count, source_count);
        debug_assert!(self.build_inputs.is_none());
    }

    fn configured_channels(&self) -> [u8; MAX_ACTIVE_SOURCES] {
        std::array::from_fn(|index| {
            if self.configured[index] {
                self.source_shapes[index].channel_count
            } else {
                0
            }
        })
    }
}

struct SessionInner {
    control: UnsafeCell<Option<ControlState>>,
    render: UnsafeCell<Option<RenderState>>,
    spatial_control: UnsafeCell<Option<SpatialControlState>>,
    spatial_render: UnsafeCell<Option<SpatialRenderState>>,
    source_count: usize,
    sample_rate_hz: u32,
    block_size: usize,
    last_render_ns: AtomicU64,
    ffi_render_buffers_bytes: u64,
    route: SessionRoute,
    spatial: UnsafeCell<Option<SpatialShellState>>,
    spatial_lifecycle: SpatialLifecycleState,
    // Preinitialized at neutral shell creation, then immutably owned by the
    // spatial audio thread. Keeping it outside `spatial_render` lets callback
    // timing begin before validation without aliasing a concurrent prepare.
    spatial_realtime_clock: Option<RealtimeClock>,
    // Advanced only after a neutral RuntimeGraph call returns Ok.
    spatial_block_start_frame: AtomicU64,
}

// Safety: the public contract assigns `control` and `render` to distinct,
// serialized threads. Cross-role state uses bounded snapshot publications or
// atomics, and the backend supports concurrent graph reads by construction.
unsafe impl Send for SessionInner {}
unsafe impl Sync for SessionInner {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeMetadataWire {
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

/// Creates a retained session.
///
/// Thread safety: call on the control thread before starting audio. `config`,
/// both UTF-8 NUL-terminated paths, and `out_session` are borrowed only for
/// this call. On failure, `*out_session` is null. The package path names a
/// `.fightbox` directory; the bake path names a directory containing
/// `probe-batch.bin`, `probe-batch-metadata.json`, and
/// `city-bake-manifest.json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_create(
    config: *const FbSessionConfig,
    package_path_utf8: *const c_char,
    bake_path_utf8: *const c_char,
    out_session: *mut *mut FbSession,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(out_session) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: `out_session` was checked for null and alignment.
        unsafe { out_session.write(ptr::null_mut()) };
        if !valid_const_ptr(config) || package_path_utf8.is_null() || bake_path_utf8.is_null() {
            return FbResult::FbInvalidArgument;
        }
        // Safety: pointers are valid for this call under the C API contract.
        let config = unsafe { *config };
        let package = match unsafe { path_from_c(package_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        let bake = match unsafe { path_from_c(bake_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        let session = match SessionInner::create(config, &package, &bake) {
            Ok(session) => session,
            Err(result) => return result,
        };
        let raw = Box::into_raw(Box::new(session)).cast::<FbSession>();
        // Safety: `out_session` was validated and is uniquely borrowed by this call.
        unsafe { out_session.write(raw) };
        FbResult::FbOk
    })
}

/// Creates an immutable-route V2 session.
///
/// The universal `abi_version`/`struct_size` header is validated before the
/// current config prefix is copied. Both paths are still parsed for every
/// otherwise-valid route. Neutral creation validates and retains immutable
/// world/config inputs, then collects every source shape. The final source
/// configuration binds Steam simulation, RuntimeGraph, and the packed render
/// scratch together on the control thread before rendering becomes available.
/// Legacy route construction delegates to the preserved builder. On every
/// failure, `*out_session` is null.
///
/// # Safety
///
/// `config` must name at least its readable eight-byte V2 header and, when that
/// header advertises the current size, the complete readable current prefix.
/// Both paths must be readable NUL-terminated strings. `out_session` must be a
/// writable, aligned pointer for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_create_v2(
    config: *const FbSessionConfigV2,
    package_path_utf8: *const c_char,
    bake_path_utf8: *const c_char,
    out_session: *mut *mut FbSession,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(out_session) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: `out_session` was checked for null and alignment.
        unsafe { out_session.write(ptr::null_mut()) };

        // Safety: the C contract requires the universal header, followed by
        // the current prefix only when its declared size is large enough.
        let config = match unsafe { copy_session_config_v2(config) } {
            Ok(config) => config,
            Err(result) => return result,
        };
        if let Err(result) = validate_session_config_v2(&config) {
            return result;
        }
        if package_path_utf8.is_null() || bake_path_utf8.is_null() {
            return FbResult::FbInvalidArgument;
        }
        // Safety: both non-null path pointers follow the C string contract.
        let package = match unsafe { path_from_c(package_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        // Safety: both non-null path pointers follow the C string contract.
        let bake = match unsafe { path_from_c(bake_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };

        let session = if config.render_route == FbRenderRouteV2::FbRenderNeutralSpatialV2 as u32 {
            match SessionInner::create_spatial_shell(config, &package, &bake) {
                Ok(session) => session,
                Err(result) => return result,
            }
        } else {
            let legacy = FbSessionConfig {
                sample_rate_hz: config.sample_rate_hz,
                block_size_frames: config.block_size_frames,
                source_count: config.source_count,
                default_source_level_db: config.default_source_level_db,
                quality_tier: config.quality_tier,
            };
            match SessionInner::create(legacy, &package, &bake) {
                Ok(session) => session,
                Err(result) => return result,
            }
        };
        let raw = Box::into_raw(Box::new(session)).cast::<FbSession>();
        // Safety: `out_session` was validated and is uniquely borrowed.
        unsafe { out_session.write(raw) };
        FbResult::FbOk
    })
}

/// Explicitly opts a neutral session into the additive tokened macro bridge.
///
/// Control thread only, immediately after `fb_session_create_v2` and before any
/// source configuration. V2 activation behavior remains unchanged for sessions
/// that never call this symbol.
///
/// # Safety
/// `session` is a live neutral handle and `config` is one readable current V3
/// scalar configuration.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_enable_macro_production_bridge_v3(
    session: *mut FbSession,
    config: *const FbMacroProductionBridgeConfigV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_const_ptr(config) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller promises one readable aligned current record.
        let config = unsafe { *config };
        let diffuse_profile = fightbox_api::diffuse::DiffuseFieldProfile {
            wet_gain: config.diffuse_wet_gain,
            rt60_s: config.diffuse_rt60_s,
            high_frequency_damping: config.diffuse_high_frequency_damping,
        };
        if config.abi_version != FB_ABI_VERSION_V3
            || usize::try_from(config.struct_size)
                .ok()
                .is_none_or(|size| size < size_of::<FbMacroProductionBridgeConfigV3>())
            || config.reserved_f32.to_bits() != 0
            || config.reserved.iter().any(|value| *value != 0)
            || diffuse_profile.validate().is_err()
        {
            return FbResult::FbInvalidArgument;
        }
        if session.route != SessionRoute::NeutralSpatial
            || session.source_count != MAX_ACTIVE_SOURCES
            || session.spatial_lifecycle.lifecycle() != SpatialShellLifecycle::Collecting
        {
            return FbResult::FbInvalidState;
        }
        // Safety: construction is serialized on the sole control owner and no
        // render half exists until all source shapes have been configured.
        let Some(spatial) = (unsafe { &mut *session.spatial.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        if spatial.configured_count != 0
            || spatial
                .build_inputs
                .as_ref()
                .is_none_or(|inputs| inputs.environmental_order != 2)
        {
            return FbResult::FbInvalidState;
        }
        spatial.macro_bridge_requested = true;
        spatial.macro_diffuse_profile = diffuse_profile;
        FbResult::FbOk
    })
}

/// Freezes one logical source's program and presentation shape.
///
/// This is a construction-time control-thread call. A source may be configured
/// once, before the audio thread begins. Legacy-route sessions reject it.
///
/// # Safety
///
/// `session` must be a live handle. `config` follows the same universal-header
/// and current-prefix readability contract as [`fb_session_create_v2`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_configure_source_v2(
    session: *mut FbSession,
    config: *const FbSourceProgramConfigV2,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        // Safety: the C contract requires the universal header and, when its
        // declared size admits it, the readable current config prefix.
        let config = match unsafe { copy_source_program_config_v2(config) } {
            Ok(config) => config,
            Err(result) => return result,
        };
        if let Err(result) = validate_source_program_config_v2(&config, session.source_count) {
            return result;
        }
        let shape = SpatialSourceShape {
            channel_count: config.channel_count as u8,
            source_geometry: config.source_geometry,
            multipoint_count: config.multipoint_count as u8,
            extent_m: config.extent_m,
            presentation_provenance: SpatialPresentationProvenance::infer(
                config.source_geometry,
                config.channel_count,
            ),
        };
        // Safety: construction/configuration is serialized on the control
        // thread and completes before any spatial render call.
        session.configure_spatial_source(config.source_index as usize, shape)
    })
}

/// Prepares a neutral spatial V2 session for its first render.
///
/// Thread safety: control thread only, after every source shape and the actual
/// initial listener/source states have been published. This mandatory barrier
/// performs no public render: a successful call leaves the first render at
/// frame and timing-observation zero. Legacy-route sessions reject it.
///
/// # Safety
///
/// `session` must be a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_prepare_spatial_v2(session: *mut FbSession) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        let Some(spatial) = (unsafe { &*session.spatial.get() }).as_ref() else {
            return FbResult::FbInvalidState;
        };
        if spatial.configured_count != session.source_count {
            return FbResult::FbInvalidState;
        }
        // A repeat before the first successful callback deliberately revokes
        // the prior prepared state before doing any fallible work. Kappa's
        // backend hook follows the same rollback rule.
        if let Err(result) = session.spatial_lifecycle.begin_prepare() {
            return result;
        }

        // Safety: preparation is serialized with listener/source updates on
        // the control thread, while BoundUnprepared excludes the audio graph.
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        if !control.listener_published
            || control.source_published[..session.source_count]
                .iter()
                .any(|published| !published)
        {
            return FbResult::FbInvalidState;
        }
        // Safety: the same lifecycle exclusion makes the render half uniquely
        // control-thread-owned for this explicit preparation call.
        let Some(render) = (unsafe { &mut *session.spatial_render.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };

        control.preparation.attempts = control.preparation.attempts.saturating_add(1);
        let started = Instant::now();
        let result = prepare_spatial_session_for_realtime(session, control, render);
        control.preparation.latest_duration_ns = Some(
            u64::try_from(started.elapsed().as_nanos())
                .unwrap_or(u64::MAX)
                .max(1),
        );
        match result {
            Ok(()) => {
                control.preparation.successes = control.preparation.successes.saturating_add(1);
                session.spatial_lifecycle.mark_prepared();
                FbResult::FbOk
            }
            Err(result) => {
                control.preparation.failures = control.preparation.failures.saturating_add(1);
                result
            }
        }
    })
}

/// Builds and primes one neighboring neutral cell on the control thread.
///
/// This call may perform file I/O, decompression, Steam scene construction,
/// simulation, and allocation. Hosts should dispatch it away from the audio
/// callback. The active session remains renderable throughout. The returned
/// handle is bound to `session` and must be offered or destroyed exactly once.
///
/// # Safety
///
/// `session` must be a live neutral handle. `config` follows the V2 readable
/// prefix contract, both paths are readable NUL-terminated UTF-8, and
/// `out_prepared_cell` is writable for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_prepare_cell_v2(
    session: *mut FbSession,
    package_path_utf8: *const c_char,
    bake_path_utf8: *const c_char,
    config: *const FbCellConfigV2,
    out_prepared_cell: *mut *mut FbPreparedCell,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(out_prepared_cell) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: validated writable output slot; null on every failure.
        unsafe { out_prepared_cell.write(ptr::null_mut()) };
        let Some(session_inner) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session_inner.route != SessionRoute::NeutralSpatial
            || !matches!(
                session_inner.spatial_lifecycle.lifecycle(),
                SpatialShellLifecycle::PreparedNotStarted | SpatialShellLifecycle::Running
            )
        {
            return FbResult::FbInvalidState;
        }
        let config = match unsafe { copy_cell_config_v2(config) } {
            Ok(config) => config,
            Err(result) => return result,
        };
        if let Err(result) = validate_cell_config_v2(&config) {
            return result;
        }
        if package_path_utf8.is_null() || bake_path_utf8.is_null() {
            return FbResult::FbInvalidArgument;
        }
        let package_path = match unsafe { path_from_c(package_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        let bake_path = match unsafe { path_from_c(bake_path_utf8) } {
            Ok(path) => path,
            Err(result) => return result,
        };
        // Reserve the session's sole candidate before package loading or
        // constructing a third Steam Audio world. The handle owns this lease
        // until successful offer consumption or explicit destruction.
        let Some(control) = (unsafe { &mut *session_inner.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let reservation = match PreparedCellReservation::acquire(&control.prepared_cell_reserved) {
            Ok(reservation) => reservation,
            Err(result) => return result,
        };
        let loaded = match fightbox_world::read_package(&package_path) {
            Ok(loaded) => loaded,
            Err(_) => return FbResult::FbInvalidPackage,
        };
        let mesh = match scene_mesh(&loaded) {
            Ok(mesh) => mesh,
            Err(result) => return result,
        };
        let bake = match load_bake(&bake_path) {
            Ok(bake) => bake,
            Err(result) => return result,
        };
        if let Err(result) = verify_bake_identity(&loaded, &bake_path, &bake) {
            return result;
        }
        let authority_seed = match cell_authority_seed(&loaded, &package_path, &bake) {
            Ok(authority) => authority,
            Err(result) => return result,
        };
        let prepared_world = match authority_seed.metadata_city_offset_enu {
            Some(offset) => control
                .runner
                .prepare_world_with_metadata_city_offset(&mesh, &bake, offset),
            None => control.runner.prepare_world(&mesh, &bake),
        };
        let mut world = match prepared_world {
            Ok(world) => world,
            Err(fightbox_steam_audio::BackendError::SdkUnavailable(_)) => {
                return FbResult::FbBackendUnavailable;
            }
            Err(_) => return FbResult::FbBackendError,
        };
        let target_update = translate_update_between_cell_frames(
            &control.update,
            control.active_world_offset_enu,
            authority_seed.metadata_city_offset_enu,
        );
        if world
            .prepare_simulation_for_realtime(&target_update)
            .is_err()
        {
            return FbResult::FbBackendError;
        }
        let authority = authority_seed.bind(world.generation());
        let prepared = PreparedCellInner {
            _reservation: reservation,
            owner_session: session.addr(),
            config,
            metadata_city_offset_enu: authority_seed.metadata_city_offset_enu,
            authority,
            echo_authority: authority_seed.echo_authority.clone(),
            world: Some(world),
        };
        let raw = Box::into_raw(Box::new(prepared)).cast::<FbPreparedCell>();
        // Safety: validated unique output slot.
        unsafe { out_prepared_cell.write(raw) };
        FbResult::FbOk
    })
}

/// Offers one prepared cell for block-boundary adoption.
///
/// Success consumes `prepared_cell`. Failure leaves the handle and its world
/// intact, so the host may retry or destroy it. The render thread performs the
/// eight-block direct/path transition and tail-only retirement.
///
/// # Safety
///
/// Both handles must be live, and the prepared handle must have been created
/// for this exact session. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_offer_prepared_cell_v2(
    session: *mut FbSession,
    prepared_cell: *mut FbPreparedCell,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session_inner) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session_inner.route != SessionRoute::NeutralSpatial
            || !valid_mut_ptr(prepared_cell.cast::<PreparedCellInner>())
        {
            return FbResult::FbInvalidArgument;
        }
        // Safety: caller owns the live opaque handle on this serialized call.
        let prepared = unsafe { &mut *prepared_cell.cast::<PreparedCellInner>() };
        if prepared.owner_session != session.addr() {
            return FbResult::FbInvalidState;
        }
        let Some(world) = prepared.world.take() else {
            return FbResult::FbInvalidState;
        };
        // Safety: control role is serialized and distinct from spatial render.
        let Some(control) = (unsafe { &mut *session_inner.spatial_control.get() }).as_mut() else {
            prepared.world = Some(world);
            return FbResult::FbInvalidState;
        };
        match control.runner.try_swap_prepared_world(world) {
            Ok(_receipt) => {
                let _identity = prepared.config;
                // Render adopts at the next block boundary. Hold new-event
                // authority until the control side observes that transition;
                // an event admitted in the offer-to-callback gap must not be
                // labeled as if the new local world were already audible.
                control.pending_cell_authority = Some(prepared.authority.clone());
                control.pending_echo_authority = prepared.echo_authority.clone();
                control.update = translate_update_between_cell_frames(
                    &control.update,
                    control.active_world_offset_enu,
                    prepared.metadata_city_offset_enu,
                );
                control.active_world_offset_enu = prepared.metadata_city_offset_enu;
                // The adopted simulation has its own direct-generation
                // sequence. Publish that token before the render side sees
                // the new graph; both retained simulations advance one exact
                // translated city-space truth during the bounded fade.
                let block_start_frame = session_inner
                    .spatial_block_start_frame
                    .load(Ordering::Acquire);
                control
                    .propagation_writer
                    .publish(propagation_snapshot_from_update(
                        &control.update,
                        control.runner.latest_direct_sequence(),
                        frame_time_ns(block_start_frame, session_inner.sample_rate_hz),
                    ));
                // Safety: successful publication consumes this unique handle.
                drop(unsafe { Box::from_raw(prepared_cell.cast::<PreparedCellInner>()) });
                FbResult::FbOk
            }
            Err((error, world)) => {
                prepared.world = Some(world);
                match error {
                    fightbox_steam_audio::PreparedSpatialWorldSwapError::PreparationFailed => {
                        FbResult::FbBackendError
                    }
                    fightbox_steam_audio::PreparedSpatialWorldSwapError::AdoptionPending
                    | fightbox_steam_audio::PreparedSpatialWorldSwapError::IncompatibleRoute => {
                        FbResult::FbInvalidState
                    }
                }
            }
        }
    })
}

/// Returns the lock-free neutral cell adoption/retirement phase.
///
/// # Safety
///
/// `session` is live and `out_state` is writable for this call. Control thread
/// only; no SDK object is touched.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_cell_stream_state_v2(
    session: *mut FbSession,
    out_state: *mut FbCellStreamStateV2,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(out_state) {
            return FbResult::FbInvalidArgument;
        }
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        synchronize_adopted_cell_authority(control);
        let phase = match control.runner.cell_stream_state() {
            SpatialCellStreamState::Idle => FB_CELL_STREAM_PHASE_IDLE_V2,
            SpatialCellStreamState::Publishing => FB_CELL_STREAM_PHASE_PUBLISHING_V2,
            SpatialCellStreamState::Prepared => FB_CELL_STREAM_PHASE_PREPARED_V2,
            SpatialCellStreamState::Crossfading => FB_CELL_STREAM_PHASE_CROSSFADING_V2,
            SpatialCellStreamState::TailRetiring => FB_CELL_STREAM_PHASE_TAIL_RETIRING_V2,
            SpatialCellStreamState::TailComplete => FB_CELL_STREAM_PHASE_TAIL_COMPLETE_V2,
        };
        let state = FbCellStreamStateV2 {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: size_of::<FbCellStreamStateV2>() as u32,
            phase,
            reserved_u32: 0,
            control_generation: control.runner.world_diagnostics().generation,
            reserved: [0; 4],
        };
        // Safety: validated writable output slot.
        unsafe { out_state.write(state) };
        FbResult::FbOk
    })
}

/// Collects a TailComplete cell generation on the control thread. Idempotent.
///
/// # Safety
///
/// `session` must be a live neutral handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_collect_retired_cell_v2(session: *mut FbSession) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        synchronize_adopted_cell_authority(control);
        control.runner.collect_retired_world();
        FbResult::FbOk
    })
}

/// Destroys an unoffered prepared-cell handle on the caller's control thread.
///
/// # Safety
///
/// `prepared_cell` must be a live handle returned by
/// [`fb_session_prepare_cell_v2`] and not previously consumed or destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_prepared_cell_destroy_v2(
    prepared_cell: *mut FbPreparedCell,
) -> FbResult {
    ffi_boundary(|| {
        if !valid_mut_ptr(prepared_cell.cast::<PreparedCellInner>()) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: caller transfers the unique allocation back exactly once.
        drop(unsafe { Box::from_raw(prepared_cell.cast::<PreparedCellInner>()) });
        FbResult::FbOk
    })
}

/// Freezes the atmosphere used by macro transport for this session.
///
/// A null observation explicitly selects the deterministic fallback. Invalid
/// host weather also freezes the complete fallback with its stable provenance.
/// The atmosphere may be selected only before the first macro group is
/// admitted or activated.
///
/// # Safety
///
/// `session` must be a live neutral handle. A non-null `observation` follows
/// the V2 readable-prefix contract. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_freeze_atmosphere_v2(
    session: *mut FbSession,
    observation: *const FbAtmosphereObservationV2,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        if control.macro_atmosphere_locked {
            return FbResult::FbInvalidState;
        }
        let observation = if observation.is_null() {
            None
        } else {
            let observation = match unsafe { copy_atmosphere_observation_v2(observation) } {
                Ok(observation) => observation,
                Err(result) => return result,
            };
            if let Err(result) = validate_atmosphere_observation_v2(&observation) {
                return result;
            }
            Some(AtmosphereObservation {
                temperature_c: observation.temperature_c,
                relative_humidity_percent: observation.relative_humidity_percent,
                pressure_kpa: observation.pressure_kpa,
            })
        };
        control.macro_atmosphere = FrozenAtmosphere::freeze(observation);
        FbResult::FbOk
    })
}

/// Plans and atomically admits one macro-event group into the session queue.
///
/// The engine derives delay, distance, ingress proxy, bearing, and atmospheric
/// conditioning from the current listener truth. A group contains one to four
/// contiguous requests with one unique retained role each. No active render
/// voice or decoded PCM is allocated while the event remains dormant.
///
/// # Safety
///
/// `session` is a live neutral handle and `requests` names `request_count`
/// readable current V2 request structures. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_admit_macro_event_group_v2(
    session: *mut FbSession,
    requests: *const FbMacroEventRequestV2,
    request_count: u32,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let request_count = match usize::try_from(request_count) {
            Ok(count) if (1..=FB_MAX_MACRO_ACTIVATIONS_V2 as usize).contains(&count) => count,
            _ => return FbResult::FbInvalidArgument,
        };
        if session.route != SessionRoute::NeutralSpatial || requests.is_null() {
            return FbResult::FbInvalidArgument;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        synchronize_adopted_cell_authority(control);
        let listener_position = control.update.listener.pose.position;
        let listener_time_s = session.spatial_block_start_frame.load(Ordering::Acquire) as f64
            / f64::from(session.sample_rate_hz);
        let mut planned = Vec::with_capacity(request_count);
        for index in 0..request_count {
            // Safety: the caller promises a contiguous readable request array.
            let request = match unsafe { copy_macro_event_request_v2(requests.add(index)) } {
                Ok(request) => request,
                Err(result) => return result,
            };
            if let Err(result) = validate_macro_event_request_v2(&request) {
                return result;
            }
            let Some(position_enu) = vector_from_ffi(request.emitter_position_enu) else {
                return FbResult::FbInvalidArgument;
            };
            let Some(role) = macro_event_role_from_ffi(request.role) else {
                return FbResult::FbInvalidArgument;
            };
            let Some(asset_transport) = macro_asset_transport_from_ffi(request.asset_transport)
            else {
                return FbResult::FbInvalidArgument;
            };
            let plan = match plan_macro_transport(
                MacroEmitter {
                    id: MacroEventId(request.event_id),
                    position_enu,
                    program_started_at_s: request.emission_frame as f64
                        / f64::from(session.sample_rate_hz),
                    asset_transport,
                    recording_carries_motion: request.recording_carries_motion != 0,
                },
                MacroListener {
                    position_enu: listener_position,
                    session_time_s: listener_time_s,
                },
                MacroTransportConfig {
                    local_horizon_m: request.local_horizon_m,
                },
                &control.macro_atmosphere,
            ) {
                Ok(plan) => plan,
                Err(_) => return FbResult::FbInvalidArgument,
            };
            let event = match plan.schedule_event(MacroEventScheduleRequest {
                event_id: MacroEventId(request.event_id),
                atomic_group_id: request.atomic_group_id,
                role,
                asset_key: request.asset_key,
                emission_frame: request.emission_frame,
                program_seek_frame: request.program_seek_frame,
                retained_frames_after_activation: request.retained_frames_after_activation,
                sample_rate_hz: session.sample_rate_hz,
            }) {
                Ok(event) => event,
                Err(_) => return FbResult::FbInvalidArgument,
            };
            planned.push(event);
        }
        match control.macro_ingress.admit_group(&planned) {
            Ok(()) => {
                control.macro_atmosphere_locked = true;
                FbResult::FbOk
            }
            Err(fightbox_runtime::EventAdmissionError::QueueFull) => FbResult::FbInvalidState,
            Err(_) => FbResult::FbInvalidArgument,
        }
    })
}

/// Binds one already-admitted dormant macro event to a real authored static
/// ingress anchor. The binding must precede token preparation; all-zero remains
/// structural echo Off and V2 admission never invents an anchor.
///
/// # Safety
/// `session` is a live bridge-enabled neutral handle and `binding` is one
/// readable, aligned current V3 scalar record. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_bind_macro_echo_anchor_v3(
    session: *mut FbSession,
    binding: *const FbMacroEchoAnchorBindingV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_const_ptr(binding) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller promises one readable aligned current record.
        let binding = unsafe { *binding };
        if binding.abi_version != FB_ABI_VERSION_V3
            || usize::try_from(binding.struct_size)
                .ok()
                .is_none_or(|size| size < size_of::<FbMacroEchoAnchorBindingV3>())
            || binding.event_id == 0
            || binding.anchor_key == [0; 16]
            || binding.reserved.iter().any(|value| *value != 0)
        {
            return FbResult::FbInvalidArgument;
        }
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidArgument;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        if !control.macro_bridge.enabled {
            return FbResult::FbInvalidState;
        }
        match control
            .macro_ingress
            .bind_echo_anchor(MacroEventId(binding.event_id), binding.anchor_key)
        {
            Ok(()) => FbResult::FbOk,
            Err(
                fightbox_runtime::EventEchoBindingError::ZeroIdentity
                | fightbox_runtime::EventEchoBindingError::IneligibleRole,
            ) => FbResult::FbInvalidArgument,
            Err(_) => FbResult::FbInvalidState,
        }
    })
}

/// Reserves the earliest common macro activation frame within a lookahead
/// horizon without consuming the dormant event or occupying a render role.
/// Swift uses the returned scalar records to prepare exact canonical seeks.
///
/// # Safety
/// `session` is a live bridge-enabled neutral handle. `out_batch` is writable,
/// aligned, and initialized with a current V3 header. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_prepare_macro_token_v3(
    session: *mut FbSession,
    lookahead_frame: u64,
    out_batch: *mut FbMacroPrepareBatchV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if unsafe { validate_v3_output(out_batch) }.is_err() {
            return FbResult::FbInvalidArgument;
        }
        let mut output = FbMacroPrepareBatchV3::default();
        output.lookahead_frame = lookahead_frame;
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        if session.route != SessionRoute::NeutralSpatial
            || !control.macro_bridge.enabled
            || control.macro_bridge.reservation.is_some()
            || lookahead_frame < session.spatial_block_start_frame.load(Ordering::Acquire)
        {
            return FbResult::FbInvalidState;
        }
        if !control.macro_bridge.command_publisher.try_reserve() {
            return FbResult::FbInvalidState;
        }
        let events = control
            .macro_ingress
            .reserve_next_activation(lookahead_frame);
        if events.is_empty() {
            let released = control.macro_bridge.command_publisher.discard_reservation();
            debug_assert!(released);
            // Safety: the output header/prefix was validated above.
            unsafe { out_batch.write(output) };
            return FbResult::FbOk;
        }
        let token_id = control.macro_bridge.next_token_id.max(1);
        control.macro_bridge.next_token_id = token_id.wrapping_add(1).max(1);
        control.macro_bridge.reservation = Some(MacroBridgeReservation {
            token_id,
            events,
            readiness_generation: [0; EventRole::COUNT],
            ready_mask: 0,
            acknowledged_mask: 0,
            finalized_mask: 0,
            state: MacroBridgeReservationState::Reserved,
        });
        output.token_id = token_id;
        output.event_count = events.count.into();
        output.status = FbMacroTokenStatusV3::FbMacroTokenReservedV3 as u32;
        for (index, event) in events.iter().copied().enumerate() {
            output.events[index] = FbMacroPrepareEventV3 {
                abi_version: FB_ABI_VERSION_V3,
                struct_size: size_of::<FbMacroPrepareEventV3>() as u32,
                event_id: event.event_id.0,
                atomic_group_id: event.atomic_group_id,
                role: macro_event_role_to_ffi(event.role),
                reserved_u32: 0,
                asset_key: event.asset_key,
                activation_frame: event.ingress_activation_frame,
                program_seek_frame: event.program_seek_frame,
                tail_deadline_frame: event.tail_deadline_frame,
                reserved: [0; 2],
            };
        }
        // Safety: the output header/prefix was validated above.
        unsafe { out_batch.write(output) };
        FbResult::FbOk
    })
}

/// Copies an all-or-nothing set of even canonical-provider readiness tokens
/// into the held dormant reservation. No async/provider object crosses C.
///
/// # Safety
/// `ready` names `ready_count` readable current V3 scalar records. Control
/// thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_stage_macro_ready_v3(
    session: *mut FbSession,
    ready: *const FbMacroReadyAssetV3,
    ready_count: u32,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Ok(count) = usize::try_from(ready_count) else {
            return FbResult::FbInvalidArgument;
        };
        if count == 0 || count > FB_MAX_MACRO_TOKEN_EVENTS_V3 as usize || !valid_const_ptr(ready) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller promises `count` readable aligned records.
        let records = unsafe { slice::from_raw_parts(ready, count) };
        let mut copied = [FbMacroReadyAssetV3::default(); EventRole::COUNT];
        copied[..count].copy_from_slice(records);
        if copied[..count].iter().any(|record| {
            record.abi_version != FB_ABI_VERSION_V3
                || usize::try_from(record.struct_size)
                    .ok()
                    .is_none_or(|size| size < size_of::<FbMacroReadyAssetV3>())
                || record.reserved.iter().any(|value| *value != 0)
                || record.token_id == 0
                || record.event_id == 0
                || record.asset_key == 0
                || record.discontinuity_sequence == 0
                || record.discontinuity_sequence & 1 != 0
        }) {
            return FbResult::FbInvalidArgument;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(reservation) = control.macro_bridge.reservation else {
            return FbResult::FbInvalidState;
        };
        if session.route != SessionRoute::NeutralSpatial
            || !control.macro_bridge.enabled
            || reservation.state == MacroBridgeReservationState::Committed
            || reservation.events.len() != count
        {
            return FbResult::FbInvalidState;
        }
        let slots = fightbox_steam_audio::MacroIngressSlotMap::default();
        let mut generations = [0_u64; EventRole::COUNT];
        let mut ready_mask = 0_u8;
        for record in &copied[..count] {
            let Some(role) = macro_event_role_from_ffi(record.role) else {
                return FbResult::FbInvalidArgument;
            };
            let Some(event) = reservation.events.iter().find(|event| event.role == role) else {
                return FbResult::FbInvalidState;
            };
            let role_bit = 1_u8 << role.index();
            if ready_mask & role_bit != 0
                || record.token_id != reservation.token_id
                || record.event_id != event.event_id.0
                || record.asset_key != event.asset_key
                || record.program_seek_frame != event.program_seek_frame
                || usize::try_from(record.source_index).ok() != Some(slots.source_index(role))
            {
                return FbResult::FbInvalidState;
            }
            ready_mask |= role_bit;
            generations[role.index()] = record.discontinuity_sequence;
        }
        let mut staged = reservation;
        staged.readiness_generation = generations;
        staged.ready_mask = ready_mask;
        staged.state = MacroBridgeReservationState::Ready;
        debug_assert!(staged.is_ready());
        control.macro_bridge.reservation = Some(staged);
        FbResult::FbOk
    })
}

fn build_macro_echo_plan(
    authority: &fightbox_world::PackageEchoAuthority,
    event: &fightbox_runtime::ScheduledMacroEvent,
    listener_position_enu: EnuVector3,
    atmosphere: &fightbox_runtime::FrozenAtmosphere,
    sample_rate_hz: u32,
) -> Result<SteamMacroEchoPlan, FbResult> {
    if event.echo_anchor_key == [0; 16] || !event.role.local_propagation_eligibility().authored_echo
    {
        return Ok(SteamMacroEchoPlan::OFF);
    }
    let query = fightbox_world::EchoAuthorityQuery {
        anchor: fightbox_world::StableSpatialKey(event.echo_anchor_key),
        source_position_city_enu_m: [
            event.ingress_position_enu.east_m,
            event.ingress_position_enu.north_m,
            event.ingress_position_enu.up_m,
        ],
        listener_cell: fightbox_world::echo_listener_cell_key(&authority.cell_id),
        listener_position_city_enu_m: [
            listener_position_enu.east_m,
            listener_position_enu.north_m,
            listener_position_enu.up_m,
        ],
    };
    let queried = authority
        .table
        .query(query)
        .map_err(|_| FbResult::FbInvalidState)?;
    if queried.taps.len() > MAX_MACRO_ECHO_TAPS {
        return Err(FbResult::FbInvalidState);
    }
    let mut plan = SteamMacroEchoPlan::OFF;
    for (index, tap) in queried.taps.iter().enumerate() {
        let air_db = atmosphere
            .stage_gain_db_at_distance(tap.total_path_m)
            .map_err(|_| FbResult::FbInvalidState)?;
        let material = tap.material_pressure;
        let material_by_band = [
            material[0],
            material[0],
            0.5 * (material[0] + material[1]),
            material[1],
            0.5 * (material[1] + material[2]),
            material[2],
            material[2],
            material[2],
        ];
        let distance_gain = 1.0 / tap.total_path_m.max(1.0);
        let mut spectral_pressure_gain = [0.0_f32; fightbox_api::spectral::SPECTRAL_BAND_COUNT];
        for band in 0..fightbox_api::spectral::SPECTRAL_BAND_COUNT {
            let air_pressure_gain = 10.0_f32.powf(air_db[band] / 20.0);
            let pressure_gain = material_by_band[band] * distance_gain * air_pressure_gain;
            if !pressure_gain.is_finite() {
                return Err(FbResult::FbInvalidState);
            }
            spectral_pressure_gain[band] = pressure_gain;
        }
        let delay_samples = tap.excess_path_m * sample_rate_hz as f32
            / fightbox_runtime::MACRO_SPEED_OF_SOUND_MPS as f32;
        if !delay_samples.is_finite() || delay_samples < 0.0 {
            return Err(FbResult::FbInvalidState);
        }
        plan.taps[index] = SteamMacroEchoTap {
            active: true,
            delay_samples,
            arrival_direction_enu: EnuVector3::new(
                tap.arrival_direction_city_enu[0],
                tap.arrival_direction_city_enu[1],
                tap.arrival_direction_city_enu[2],
            ),
            spectral_pressure_gain,
            path_key: tap.path_key.0,
        };
        plan.tap_count += 1;
    }
    Ok(plan)
}

/// Commits one ready token on its exact complete control frame. The direct
/// generation is the transaction boundary: a failed direct pass leaves the
/// reservation retryable, while a later optional-phase failure is already
/// committed and reported in `out_result`.
///
/// # Safety
/// `frame` is one readable current V2 complete frame and `out_result` is a
/// writable initialized current V3 record. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_control_frame_macro_v3(
    session: *mut FbSession,
    frame: *const FbControlFrameV2,
    token_id: u64,
    out_result: *mut FbMacroCommitResultV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if unsafe { validate_v3_output(out_result) }.is_err() || token_id == 0 {
            return FbResult::FbInvalidArgument;
        }
        let mut next_update = match unsafe { decode_control_update_v2(frame, session.source_count) }
        {
            Ok(update) => update,
            Err(result) => return result,
        };
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(reservation) = control.macro_bridge.reservation else {
            return FbResult::FbInvalidState;
        };
        let effective_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
        if session.route != SessionRoute::NeutralSpatial
            || !control.macro_bridge.enabled
            || reservation.token_id != token_id
            || reservation.state != MacroBridgeReservationState::Ready
            || !reservation.is_ready()
            || reservation.events.activation_frame() != Some(effective_frame)
            || control
                .macro_bridge
                .overlay
                .validate_complete_host_update(&next_update)
                .is_err()
        {
            return FbResult::FbInvalidState;
        }
        synchronize_adopted_cell_authority(control);
        let mut preview = match control.macro_ingress.preview_reserved(
            reservation.events,
            IngressArrivalContext {
                current_frame: effective_frame,
                sample_rate_hz: session.sample_rate_hz,
                listener_position_enu: next_update.listener.pose.position,
                atmosphere: &control.macro_atmosphere,
            },
        ) {
            Ok(preview) => preview,
            Err(_) => return FbResult::FbInvalidState,
        };
        let mut echo_plans = [SteamMacroEchoPlan::OFF; EventRole::COUNT];
        let mut echo_queried = [false; EventRole::COUNT];
        for event in reservation.events.iter() {
            let activation = preview
                .for_role(event.role)
                .expect("reserved preview preserves every event role");
            if activation.eligibility.authored_echo
                && event.echo_anchor_key != [0; 16]
                && matches!(
                    &activation.render_authority,
                    IngressRenderAuthority::DetailedLocal { .. }
                )
            {
                let Some(authority) = control.active_echo_authority.as_deref() else {
                    return FbResult::FbInvalidState;
                };
                echo_queried[event.role.index()] = true;
                echo_plans[event.role.index()] = match build_macro_echo_plan(
                    authority,
                    event,
                    next_update.listener.pose.position,
                    &control.macro_atmosphere,
                    session.sample_rate_hz,
                ) {
                    Ok(plan) => plan,
                    Err(result) => return result,
                };
            }
        }
        let mut provisional =
            SteamMacroIngressCommandBatch::from_activations(&preview, token_id, 1, effective_frame);
        for event in reservation.events.iter() {
            let Some(program_start_frame) = provisional
                .for_role(event.role)
                .map(|command| command.program_start_frame)
            else {
                return FbResult::FbInvalidState;
            };
            let Some(program_end_frame) = program_start_frame.checked_add(
                event
                    .tail_deadline_frame
                    .saturating_sub(event.ingress_activation_frame),
            ) else {
                return FbResult::FbInvalidState;
            };
            let echo_plan = echo_plans[event.role.index()];
            let echo_tail_deadline_frame = if echo_plan.is_enabled() {
                program_end_frame
                    .saturating_add(echo_plan.maximum_delay_frames())
                    .saturating_add(u64::from(session.sample_rate_hz).saturating_mul(2))
            } else {
                preview
                    .for_role(event.role)
                    .map_or(program_end_frame, |activation| {
                        activation.tail_deadline_frame
                    })
            };
            let tail_deadline_frame = preview
                .for_role(event.role)
                .map_or(echo_tail_deadline_frame, |activation| {
                    activation.tail_deadline_frame.max(echo_tail_deadline_frame)
                });
            if !preview.extend_tail_deadline(event.role, event.event_id, tail_deadline_frame)
                || !provisional.bind_asset_readiness(
                    event.role,
                    reservation.readiness_generation[event.role.index()],
                )
                || !provisional.bind_program_end_frame(event.role, program_end_frame)
                || !provisional.bind_echo_plan(event.role, echo_plan, tail_deadline_frame)
            {
                return FbResult::FbInvalidState;
            }
        }
        let mut proposed_overlay = control.macro_bridge.overlay;
        if proposed_overlay.activate(provisional).is_err()
            || control
                .macro_ingress
                .validate_reserved_commit(reservation.events, &preview, effective_frame)
                .is_err()
        {
            return FbResult::FbInvalidState;
        }
        // The mailbox was changed from EMPTY to RESERVED during token prepare.
        // Its consumer cannot observe or mutate that state, so after this
        // validation every post-direct macro operation is serialized and
        // infallible unless an internal invariant is broken.
        proposed_overlay.overlay_backend_update(&mut next_update);
        let runtime_activity =
            proposed_overlay.overlay_runtime_activity(std::array::from_fn(|index| {
                next_update.sources[index].active
            }));
        observe_spatial_callback_timings(control);
        let direct_before = control.runner.latest_direct_sequence();
        let result = commit_spatial_control_frame_phases_with_runtime_activity(
            &session.spatial_lifecycle,
            &mut control.runner,
            &mut control.update,
            next_update,
            Some(runtime_activity),
            &mut control.update_sequence,
            &mut control.propagation_writer,
            effective_frame,
            session.sample_rate_hz,
            &mut control.listener_published,
            &mut control.source_published,
            session.source_count,
            &mut control.batched_frame_advances,
        );
        let direct_generation = control.runner.latest_direct_sequence();
        let mut output = FbMacroCommitResultV3 {
            token_id,
            event_count: reservation.events.len() as u32,
            effective_frame,
            status: FbMacroTokenStatusV3::FbMacroTokenReadyV3 as u32,
            ..FbMacroCommitResultV3::default()
        };
        if direct_generation == direct_before {
            // Safety: the output header was validated before any state access.
            unsafe { out_result.write(output) };
            return result;
        }

        let mut commands = SteamMacroIngressCommandBatch::from_activations(
            &preview,
            token_id,
            direct_generation,
            effective_frame,
        );
        let mut committed_tail_deadline_frame = effective_frame;
        for event in reservation.events.iter() {
            let program_start_frame = commands
                .for_role(event.role)
                .map_or(0, |command| command.program_start_frame);
            let program_end_frame = program_start_frame.saturating_add(
                event
                    .tail_deadline_frame
                    .saturating_sub(event.ingress_activation_frame),
            );
            let tail_deadline_frame = preview
                .for_role(event.role)
                .map_or(program_end_frame, |activation| {
                    activation.tail_deadline_frame
                });
            committed_tail_deadline_frame = committed_tail_deadline_frame.max(tail_deadline_frame);
            let bound = commands.bind_asset_readiness(
                event.role,
                reservation.readiness_generation[event.role.index()],
            ) && commands.bind_program_end_frame(event.role, program_end_frame)
                && commands.bind_echo_plan(
                    event.role,
                    echo_plans[event.role.index()],
                    tail_deadline_frame,
                );
            debug_assert!(bound);
        }
        control
            .macro_ingress
            .commit_reserved(reservation.events, preview, effective_frame)
            .expect("serialized macro commit was proven before direct simulation");
        let mut committed_overlay = control.macro_bridge.overlay;
        committed_overlay
            .activate(commands)
            .expect("actual direct generation preserves the prevalidated overlay identity");
        control
            .macro_bridge
            .command_publisher
            .publish_reserved(commands)
            .expect("the pre-due mailbox reservation cannot be consumed by audio");
        control.macro_bridge.overlay = committed_overlay;
        for command in commands
            .iter()
            .filter(|command| command.echo_plan.is_enabled())
        {
            debug_assert!(control.active_echo_authority.is_some());
            if let Some(authority) = control.active_echo_authority.as_ref() {
                control.macro_bridge.echo_authority_pins[command.role.index()] =
                    Some(Arc::clone(authority));
            }
        }
        for event in reservation
            .events
            .iter()
            .filter(|event| echo_queried[event.role.index()])
        {
            let plan = echo_plans[event.role.index()];
            control.macro_bridge.echo_telemetry.committed_queries = control
                .macro_bridge
                .echo_telemetry
                .committed_queries
                .saturating_add(1);
            if plan.is_enabled() {
                control.macro_bridge.echo_telemetry.committed_plans = control
                    .macro_bridge
                    .echo_telemetry
                    .committed_plans
                    .saturating_add(1);
                control.macro_bridge.echo_telemetry.committed_taps = control
                    .macro_bridge
                    .echo_telemetry
                    .committed_taps
                    .saturating_add(u64::from(plan.tap_count));
            } else {
                control.macro_bridge.echo_telemetry.committed_silent_plans = control
                    .macro_bridge
                    .echo_telemetry
                    .committed_silent_plans
                    .saturating_add(1);
            }
        }
        let mut committed_reservation = reservation;
        committed_reservation.state = MacroBridgeReservationState::Committed;
        control.macro_bridge.reservation = Some(committed_reservation);
        control.macro_atmosphere_locked = true;
        output.status = FbMacroTokenStatusV3::FbMacroTokenCommittedV3 as u32;
        output.direct_generation = direct_generation;
        output.tail_deadline_frame = committed_tail_deadline_frame;
        // Safety: the output header was validated before any state access.
        unsafe { out_result.write(output) };
        result
    })
}

/// Discards a matching pre-commit token and returns its records to dormant
/// selection. Provider release remains Swift control-side.
///
/// # Safety
/// `session` is a live bridge-enabled neutral handle. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_discard_macro_token_v3(
    session: *mut FbSession,
    token_id: u64,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(reservation) = control.macro_bridge.reservation else {
            return FbResult::FbInvalidState;
        };
        if !control.macro_bridge.enabled
            || token_id == 0
            || reservation.token_id != token_id
            || reservation.state == MacroBridgeReservationState::Committed
        {
            return FbResult::FbInvalidState;
        }
        if control
            .macro_ingress
            .discard_reserved(reservation.events)
            .is_err()
        {
            return FbResult::FbInvalidState;
        }
        if !control.macro_bridge.command_publisher.discard_reservation() {
            return FbResult::FbInvalidState;
        }
        control.macro_bridge.reservation = None;
        FbResult::FbOk
    })
}

/// Polls one terminal fixed-copy audio acknowledgement. Polling never
/// releases Swift playback or control-owned ingress authority.
///
/// # Safety
/// `out_ack` is writable, aligned, and initialized with a current V3 header.
/// Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_poll_macro_ack_v3(
    session: *mut FbSession,
    out_ack: *mut FbMacroAudioAckV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if unsafe { validate_v3_output(out_ack) }.is_err() {
            return FbResult::FbInvalidArgument;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(mut reservation) = control.macro_bridge.reservation else {
            return FbResult::FbInvalidState;
        };
        if reservation.state != MacroBridgeReservationState::Committed {
            return FbResult::FbInvalidState;
        }
        if control
            .macro_bridge
            .pending_acknowledgements
            .iter()
            .all(Option::is_none)
        {
            if let Some(batch) = control.macro_bridge.acknowledgement_receiver.try_take() {
                for acknowledgement in batch.iter().copied() {
                    control.macro_bridge.pending_acknowledgements[acknowledgement.role.index()] =
                        Some(acknowledgement);
                }
            }
        }
        for role in EventRole::ALL {
            let role_index = role.index();
            let Some(acknowledgement) = control.macro_bridge.pending_acknowledgements[role_index]
            else {
                continue;
            };
            control.macro_bridge.pending_acknowledgements[role_index] = None;
            if acknowledgement.kind == SteamMacroIngressAcknowledgementKind::Activated {
                continue;
            }
            let Some(event) = reservation.events.iter().find(|event| event.role == role) else {
                continue;
            };
            let Some(active) = control.macro_bridge.overlay.active_command(role) else {
                continue;
            };
            let role_bit = 1_u8 << role_index;
            if reservation.acknowledged_mask & role_bit != 0
                || acknowledgement.activation_epoch != reservation.token_id
                || acknowledgement.direct_generation != active.direct_generation
                || acknowledgement.event_id != event.event_id
                || acknowledgement.asset_key != event.asset_key
                || acknowledgement.asset_readiness_generation
                    != reservation.readiness_generation[role_index]
            {
                continue;
            }
            reservation.acknowledged_mask |= role_bit;
            control.macro_bridge.reservation = Some(reservation);
            let output = FbMacroAudioAckV3 {
                token_id: reservation.token_id,
                event_id: event.event_id.0,
                role: macro_event_role_to_ffi(role),
                status: match acknowledgement.kind {
                    SteamMacroIngressAcknowledgementKind::Deactivated => {
                        FbMacroAudioAckStatusV3::FbMacroAudioCompletedV3 as u32
                    }
                    SteamMacroIngressAcknowledgementKind::TerminalRejected => {
                        FbMacroAudioAckStatusV3::FbMacroAudioTerminalRejectedV3 as u32
                    }
                    SteamMacroIngressAcknowledgementKind::Activated => unreachable!(),
                },
                asset_key: event.asset_key,
                source_index: fightbox_steam_audio::MacroIngressSlotMap::default()
                    .source_index(role) as u32,
                reserved_u32: 0,
                discontinuity_sequence: reservation.readiness_generation[role_index],
                direct_generation: acknowledgement.direct_generation,
                effective_frame: active.effective_frame,
                program_seek_frame: event.program_seek_frame,
                ..FbMacroAudioAckV3::default()
            };
            // Safety: the output header was validated before state access.
            unsafe { out_ack.write(output) };
            return FbResult::FbOk;
        }
        FbResult::FbInvalidState
    })
}

/// Finalizes a previously polled acknowledgement after Swift has released the
/// exact readiness token on its serialized control queue.
///
/// # Safety
/// `ack` is one readable current V3 scalar acknowledgement. Control thread
/// only and only after provider release.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_finalize_macro_ack_v3(
    session: *mut FbSession,
    ack: *const FbMacroAudioAckV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_const_ptr(ack) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller promises one readable aligned current record.
        let ack = unsafe { *ack };
        if ack.abi_version != FB_ABI_VERSION_V3
            || usize::try_from(ack.struct_size)
                .ok()
                .is_none_or(|size| size < size_of::<FbMacroAudioAckV3>())
            || ack.reserved_u32 != 0
            || ack.reserved.iter().any(|value| *value != 0)
            || ack.token_id == 0
            || ack.event_id == 0
            || !matches!(
                ack.status,
                value if value == FbMacroAudioAckStatusV3::FbMacroAudioCompletedV3 as u32
                    || value
                        == FbMacroAudioAckStatusV3::FbMacroAudioTerminalRejectedV3 as u32
            )
        {
            return FbResult::FbInvalidArgument;
        }
        let Some(role) = macro_event_role_from_ffi(ack.role) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(mut reservation) = control.macro_bridge.reservation else {
            return FbResult::FbInvalidState;
        };
        let Some(event) = reservation.events.iter().find(|event| event.role == role) else {
            return FbResult::FbInvalidState;
        };
        let role_bit = 1_u8 << role.index();
        let Some(active) = control.macro_bridge.overlay.active_command(role) else {
            return FbResult::FbInvalidState;
        };
        if reservation.state != MacroBridgeReservationState::Committed
            || reservation.token_id != ack.token_id
            || reservation.acknowledged_mask & role_bit == 0
            || reservation.finalized_mask & role_bit != 0
            || event.event_id.0 != ack.event_id
            || event.asset_key != ack.asset_key
            || event.program_seek_frame != ack.program_seek_frame
            || ack.source_index
                != fightbox_steam_audio::MacroIngressSlotMap::default().source_index(role) as u32
            || reservation.readiness_generation[role.index()] != ack.discontinuity_sequence
            || active.direct_generation != ack.direct_generation
            || active.effective_frame != ack.effective_frame
            || active.activation_epoch != ack.token_id
        {
            return FbResult::FbInvalidState;
        }
        let kind = if ack.status == FbMacroAudioAckStatusV3::FbMacroAudioCompletedV3 as u32 {
            SteamMacroIngressAcknowledgementKind::Deactivated
        } else {
            SteamMacroIngressAcknowledgementKind::TerminalRejected
        };
        // Both mutable halves are prevalidated before either can change. This
        // keeps finalization retryable: an ingress identity failure must not
        // clear the overlay command that qualifies a consumed Swift ACK.
        if !control.macro_ingress.release_matches(role, event.event_id) {
            return FbResult::FbInvalidState;
        }
        if control.macro_ingress.release(role, event.event_id).is_err() {
            return FbResult::FbInvalidState;
        }
        let acknowledged =
            control
                .macro_bridge
                .overlay
                .acknowledge(SteamMacroIngressAcknowledgement {
                    kind,
                    activation_epoch: ack.token_id,
                    direct_generation: ack.direct_generation,
                    audio_frame: ack.effective_frame,
                    event_id: event.event_id,
                    role,
                    asset_key: ack.asset_key,
                    asset_readiness_generation: ack.discontinuity_sequence,
                });
        debug_assert_eq!(acknowledged, Some(active));
        if acknowledged != Some(active) {
            return FbResult::FbInvalidState;
        }
        control.macro_bridge.echo_authority_pins[role.index()] = None;
        reservation.finalized_mask |= role_bit;
        let all_finalized = reservation
            .events
            .iter()
            .all(|event| reservation.finalized_mask & (1_u8 << event.role.index()) != 0);
        control.macro_bridge.reservation = (!all_finalized).then_some(reservation);
        FbResult::FbOk
    })
}

/// Consumes every currently due role-eligible macro event exactly once.
///
/// The returned records carry disjoint macro conditioning and final-local-leg
/// values. The host applies the macro fields once, seeks the canonical asset to
/// `program_seek_frame`, and gives only the local leg to the detailed renderer.
///
/// # Safety
///
/// `session` must be a live neutral handle and `out_batch` must be writable,
/// aligned, and initialized with a current V2 header. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_activate_macro_events_v2(
    session: *mut FbSession,
    current_frame: u64,
    out_batch: *mut FbMacroActivationBatchV2,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        if let Err(result) = unsafe { validate_macro_activation_batch_output_v2(out_batch) } {
            return result;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        synchronize_adopted_cell_authority(control);
        if control.macro_bridge.enabled && control.macro_bridge.reservation.is_some() {
            return FbResult::FbInvalidState;
        }
        control.macro_atmosphere_locked = true;
        let activated = control.macro_ingress.activate_due(IngressArrivalContext {
            current_frame,
            sample_rate_hz: session.sample_rate_hz,
            listener_position_enu: control.update.listener.pose.position,
            atmosphere: &control.macro_atmosphere,
        });
        let mut output = FbMacroActivationBatchV2::default();
        for (index, activation) in activated.iter().enumerate() {
            output.activations[index] = macro_activation_to_ffi(activation);
            output.activation_count += 1;
        }
        // Safety: the output prefix and alignment were validated above.
        unsafe { out_batch.write(output) };
        FbResult::FbOk
    })
}

/// Releases one active retained event role at a measured energy floor.
///
/// Deadline retirement remains automatic. An explicit release must identify
/// both the role and event so it cannot steal another member's reservation.
///
/// # Safety
///
/// `session` must be a live neutral handle. Control thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_release_macro_event_v2(
    session: *mut FbSession,
    role: u32,
    event_id: u64,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(role) = macro_event_role_from_ffi(role) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial || event_id == 0 {
            return FbResult::FbInvalidArgument;
        }
        let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        synchronize_adopted_cell_authority(control);
        if control.macro_bridge.enabled
            && control.macro_bridge.reservation.is_some_and(|reservation| {
                reservation.state == MacroBridgeReservationState::Committed
                    && reservation
                        .events
                        .iter()
                        .any(|event| event.role == role && event.event_id == MacroEventId(event_id))
            })
        {
            return FbResult::FbInvalidState;
        }
        match control.macro_ingress.release(role, MacroEventId(event_id)) {
            Ok(()) => FbResult::FbOk,
            Err(_) => FbResult::FbInvalidState,
        }
    })
}

fn publish_macro_audio_routes(audio: &mut MacroBridgeAudio) {
    audio.route_writer.publish(audio.route_snapshot);
}

fn queue_macro_audio_ack(
    audio: &mut MacroBridgeAudio,
    command: SteamMacroIngressCommand,
    kind: SteamMacroIngressAcknowledgementKind,
    audio_frame: u64,
) {
    let role_index = command.role.index();
    if audio.pending_acknowledgements[role_index].is_none() {
        audio.pending_acknowledgements[role_index] = Some(SteamMacroIngressAcknowledgement {
            kind,
            activation_epoch: command.activation_epoch,
            direct_generation: command.direct_generation,
            audio_frame,
            event_id: command.event_id,
            role: command.role,
            asset_key: command.asset_key,
            asset_readiness_generation: command.asset_readiness_generation,
        });
    }
    audio.roles[role_index].phase = MacroAudioRolePhase::AudioAcknowledged;
    audio.roles[role_index].staged_frame_count = 0;
    audio.route_snapshot.clear(command.role);
}

fn flush_one_macro_audio_ack(audio: &mut MacroBridgeAudio) {
    for role in EventRole::ALL {
        let Some(acknowledgement) = audio.pending_acknowledgements[role.index()] else {
            continue;
        };
        let mut batch = SteamMacroIngressAcknowledgementBatch::default();
        if !batch.try_push(acknowledgement) {
            return;
        }
        if audio.acknowledgement_publisher.try_publish(batch).is_ok() {
            audio.pending_acknowledgements[role.index()] = None;
        }
        return;
    }
}

/// Begins one allocation-free macro provider transaction for the session's
/// current callback frame. The output may be empty; bound macro slots must
/// still remain silent unless a returned interval names them.
///
/// # Safety
/// `out_batch` is writable, aligned, and initialized with a current V3 header.
/// Audio thread only, immediately before `fb_session_render_spatial_v2`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_macro_render_begin_v3(
    session: *mut FbSession,
    out_batch: *mut FbMacroProgramRequestBatchV3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if unsafe { validate_v3_output(out_batch) }.is_err() {
            return FbResult::FbInvalidArgument;
        }
        let Some(render) = (unsafe { &mut *session.spatial_render.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let audio = &mut render.macro_audio;
        if !audio.enabled || audio.staged_block_start_frame.is_some() {
            return FbResult::FbInvalidState;
        }
        flush_one_macro_audio_ack(audio);
        let block_start_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
        let observed_generation = render.graph.observe_spatial_propagation_sequence();
        if audio.pending_commands.is_none() {
            audio.pending_commands = audio.command_receiver.try_take();
        }
        if let Some(commands) = audio.pending_commands {
            let future = commands
                .iter()
                .any(|command| command.direct_generation > observed_generation);
            if !future {
                audio.pending_commands = None;
                let mut invalid = commands.is_empty()
                    || !commands.is_asset_ready()
                    || commands.iter().any(|command| {
                        command.activation_epoch == 0
                            || command.direct_generation != observed_generation
                            || command.effective_frame > block_start_frame
                            || command.program_start_frame < block_start_frame
                            || audio.roles[command.role.index()].phase
                                == MacroAudioRolePhase::Active
                    });
                let mut transfers =
                    [fightbox_api::spectral::SpectralTransfer::NEUTRAL; EventRole::COUNT];
                let slots = fightbox_steam_audio::MacroIngressSlotMap::default();
                if !invalid {
                    for command in commands.iter().copied() {
                        match command.compose_runtime_spectral_transfer(
                            fightbox_api::spectral::SpectralTransfer::NEUTRAL,
                        ) {
                            Ok(transfer) => transfers[command.role.index()] = transfer,
                            Err(_) => invalid = true,
                        }
                    }
                }
                if !invalid {
                    for command in commands.iter().copied() {
                        if render
                            .graph
                            .set_source_spectral_transfer(
                                slots.source_index(command.role),
                                transfers[command.role.index()],
                            )
                            .is_err()
                        {
                            invalid = true;
                            break;
                        }
                    }
                }
                if invalid {
                    for command in commands.iter().copied() {
                        queue_macro_audio_ack(
                            audio,
                            command,
                            SteamMacroIngressAcknowledgementKind::TerminalRejected,
                            block_start_frame,
                        );
                    }
                    publish_macro_audio_routes(audio);
                } else {
                    for command in commands.iter().copied() {
                        audio.roles[command.role.index()] = MacroAudioRole {
                            command: Some(command),
                            next_program_frame: command.program_start_frame,
                            staged_frame_count: 0,
                            phase: MacroAudioRolePhase::Active,
                        };
                        audio.route_snapshot.set_active(command);
                    }
                    publish_macro_audio_routes(audio);
                }
            }
        }

        let block_end = match block_start_frame.checked_add(session.block_size as u64) {
            Some(end) => end,
            None => return FbResult::FbInvalidState,
        };
        let mut output = FbMacroProgramRequestBatchV3 {
            block_start_frame,
            ..FbMacroProgramRequestBatchV3::default()
        };
        let slots = fightbox_steam_audio::MacroIngressSlotMap::default();
        let mut terminal_rejection = false;
        for role in EventRole::ALL {
            let role_index = role.index();
            let state = audio.roles[role_index];
            if state.phase != MacroAudioRolePhase::Active {
                continue;
            }
            let Some(command) = state.command else {
                return FbResult::FbInvalidState;
            };
            output.token_id = command.activation_epoch;
            if state.next_program_frame < block_start_frame {
                queue_macro_audio_ack(
                    audio,
                    command,
                    SteamMacroIngressAcknowledgementKind::TerminalRejected,
                    block_start_frame,
                );
                terminal_rejection = true;
                continue;
            }
            if state.next_program_frame >= block_end
                || state.next_program_frame >= command.program_end_frame
            {
                continue;
            }
            let request_end = block_end.min(command.program_end_frame);
            let frame_count = match u32::try_from(request_end - state.next_program_frame) {
                Ok(count) => count,
                Err(_) => return FbResult::FbInvalidState,
            };
            let destination_frame_offset =
                match u32::try_from(state.next_program_frame - block_start_frame) {
                    Ok(offset) => offset,
                    Err(_) => return FbResult::FbInvalidState,
                };
            let asset_frame_start = match command
                .program_seek_frame
                .checked_add(state.next_program_frame - command.program_start_frame)
            {
                Some(frame) => frame,
                None => return FbResult::FbInvalidState,
            };
            let request_index = output.request_count as usize;
            output.requests[request_index] = FbMacroProgramRequestV3 {
                abi_version: FB_ABI_VERSION_V3,
                struct_size: size_of::<FbMacroProgramRequestV3>() as u32,
                token_id: command.activation_epoch,
                event_id: command.event_id.0,
                asset_key: command.asset_key,
                role: macro_event_role_to_ffi(role),
                mode: match command.mode {
                    fightbox_steam_audio::SteamMacroIngressMode::DetailedLocal => {
                        FbMacroProgramModeV3::FbMacroProgramDetailedLocalV3 as u32
                    }
                    fightbox_steam_audio::SteamMacroIngressMode::MacroFallback { .. } => {
                        FbMacroProgramModeV3::FbMacroProgramFallbackPointV3 as u32
                    }
                },
                source_index: slots.source_index(role) as u32,
                reserved_u32: 0,
                program_seek_frame: command.program_seek_frame,
                discontinuity_sequence: command.asset_readiness_generation,
                asset_frame_start,
                frame_count,
                destination_frame_offset,
                block_start_frame,
                reserved: [0; 2],
            };
            audio.roles[role_index].staged_frame_count = frame_count;
            output.request_count += 1;
        }
        if terminal_rejection {
            publish_macro_audio_routes(audio);
            for role in EventRole::ALL {
                if audio.roles[role.index()].phase == MacroAudioRolePhase::AudioAcknowledged {
                    let _ = render.graph.set_source_spectral_transfer(
                        slots.source_index(role),
                        fightbox_api::spectral::SpectralTransfer::NEUTRAL,
                    );
                }
            }
        }
        output.status = if terminal_rejection {
            FbMacroTokenStatusV3::FbMacroTokenTerminalRejectedV3 as u32
        } else if EventRole::ALL
            .into_iter()
            .any(|role| audio.roles[role.index()].phase == MacroAudioRolePhase::Active)
        {
            FbMacroTokenStatusV3::FbMacroTokenAudioActiveV3 as u32
        } else {
            FbMacroTokenStatusV3::FbMacroTokenNoneV3 as u32
        };
        audio.staged_block_start_frame = Some(block_start_frame);
        // Safety: the output header was validated before mutating audio state.
        unsafe { out_batch.write(output) };
        FbResult::FbOk
    })
}

/// Completes or discards the previously begun provider interval transaction.
/// Discard clears only staged frame counts; it emits no acknowledgement and
/// leaves each active role retryable at the same engine callback frame.
///
/// # Safety
/// `session` is a live bridge-enabled neutral handle. Audio thread only.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_macro_render_end_v3(
    session: *mut FbSession,
    disposition: u32,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(render) = (unsafe { &mut *session.spatial_render.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        let Some(staged_block_start_frame) = render.macro_audio.staged_block_start_frame.take()
        else {
            return FbResult::FbInvalidState;
        };
        if disposition == FbMacroRenderDispositionV3::FbMacroRenderDiscardV3 as u32 {
            for role in EventRole::ALL {
                render.macro_audio.roles[role.index()].staged_frame_count = 0;
            }
            return FbResult::FbOk;
        }
        if disposition != FbMacroRenderDispositionV3::FbMacroRenderCommitV3 as u32 {
            render.macro_audio.staged_block_start_frame = Some(staged_block_start_frame);
            return FbResult::FbInvalidArgument;
        }
        let expected_next = match staged_block_start_frame.checked_add(session.block_size as u64) {
            Some(frame) => frame,
            None => return FbResult::FbInvalidState,
        };
        if session.spatial_block_start_frame.load(Ordering::Acquire) != expected_next
            || render.metadata.block_start_frame != staged_block_start_frame
            || render.metadata.validity != SpatialOutputValidity::Valid
        {
            for role in EventRole::ALL {
                render.macro_audio.roles[role.index()].staged_frame_count = 0;
            }
            return FbResult::FbInvalidState;
        }
        let audio = &mut render.macro_audio;
        let mut route_changed = false;
        for role in EventRole::ALL {
            let role_index = role.index();
            let staged_frame_count = audio.roles[role_index].staged_frame_count;
            audio.roles[role_index].staged_frame_count = 0;
            if audio.roles[role_index].phase != MacroAudioRolePhase::Active {
                continue;
            }
            let Some(command) = audio.roles[role_index].command else {
                return FbResult::FbInvalidState;
            };
            audio.roles[role_index].next_program_frame = audio.roles[role_index]
                .next_program_frame
                .saturating_add(u64::from(staged_frame_count));
            if audio.roles[role_index].next_program_frame >= command.program_end_frame
                && expected_next >= command.tail_deadline_frame
            {
                queue_macro_audio_ack(
                    audio,
                    command,
                    SteamMacroIngressAcknowledgementKind::Deactivated,
                    expected_next,
                );
                route_changed = true;
            }
        }
        if route_changed {
            publish_macro_audio_routes(audio);
            let slots = fightbox_steam_audio::MacroIngressSlotMap::default();
            for role in EventRole::ALL {
                if audio.roles[role.index()].phase == MacroAudioRolePhase::AudioAcknowledged {
                    let _ = render.graph.set_source_spectral_transfer(
                        slots.source_index(role),
                        fightbox_api::spectral::SpectralTransfer::NEUTRAL,
                    );
                }
            }
        }
        flush_one_macro_audio_ack(audio);
        FbResult::FbOk
    })
}

/// Renders one neutral spatial callback block.
///
/// The complete argument graph is validated before an output byte or the
/// session block clock can change. Runtime and Steam render into session-owned
/// packed scratch; only a successful RuntimeGraph call is scattered into the
/// caller's potentially strided banks. Legacy-route sessions reject this entry
/// point.
///
/// # Safety
///
/// `session` must be live. `block` and every active record/buffer follow the
/// V2 header, capacity, alignment, readability, writability, and non-overlap
/// contracts declared by their public structures.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_render_spatial_v2(
    session: *mut FbSession,
    block: *const FbSpatialRenderBlockV2,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::NeutralSpatial {
            return FbResult::FbInvalidState;
        }
        // Configuration and explicit preparation are control-thread barriers.
        // Reject incomplete or unprepared sessions before even reading the
        // callback block. Starting is privately owned by another first-call
        // claimant and fails closed as well.
        if matches!(
            session.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::Collecting
                | SpatialShellLifecycle::BoundUnprepared
                | SpatialShellLifecycle::Starting
        ) {
            return FbResult::FbInvalidState;
        }
        let Some(spatial) = (unsafe { &*session.spatial.get() }).as_ref() else {
            return FbResult::FbInvalidState;
        };
        if spatial.configured_count != session.source_count {
            return FbResult::FbInvalidState;
        }
        // Start the preinitialized audio-thread clock immediately after the
        // ready barrier, without borrowing the render graph. This covers the
        // current-prefix copy, finite scans, assembly, RuntimeGraph, mapping,
        // and scatter while allowing a concurrent update to revoke preparation
        // safely during validation. Rejected calls discard this sample.
        let Some(realtime_clock) = session.spatial_realtime_clock.as_ref() else {
            return FbResult::FbInvalidState;
        };
        let started = realtime_clock.start();
        // Safety: validate the universal header before copying the current
        // render-block prefix.
        let block = match unsafe { copy_spatial_render_block_v2(block) } {
            Ok(block) => block,
            Err(result) => return result,
        };
        // Safety: configuration is frozen before the audio thread begins.
        let configured_channels = spatial.configured_channels();
        // Safety: the public C contract supplies storage for every declared
        // active range. The validator checks all representable invariants
        // before constructing any slice.
        let validated = match unsafe {
            validate_spatial_render_block_v2(
                &block,
                session.source_count,
                &configured_channels,
                session.block_size,
            )
        } {
            Ok(validated) => validated,
            Err(result) => return result,
        };

        let block_start_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
        let Some(next_block_start_frame) = block_start_frame.checked_add(session.block_size as u64)
        else {
            return FbResult::FbInvalidState;
        };
        let now_ns = frame_time_ns(block_start_frame, session.sample_rate_hz);

        let empty: &[f32] = &[];
        let mut source_blocks = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [empty, empty],
        }; MAX_ACTIVE_SOURCES];
        for (slot, program) in validated.programs[..validated.program_count]
            .iter()
            .enumerate()
        {
            // Safety: the V2 validator proved each active plane readable,
            // finite, non-overlapping, and exactly one configured block long.
            let first = unsafe { slice::from_raw_parts(program.planes[0], session.block_size) };
            let second = if program.channel_count == 2 {
                // Safety: same proof for configured plane one.
                unsafe { slice::from_raw_parts(program.planes[1], session.block_size) }
            } else {
                empty
            };
            source_blocks[slot] = SpatialProgramBlock {
                source_index: program.source_index,
                program_plane_count: program.channel_count,
                program_planes: [first, second],
            };
        }

        // Invalid block graphs above do not consume preparation. Claim the
        // first advancing callback only immediately before RuntimeGraph.
        let first_render_epoch = match session.spatial_lifecycle.claim_first_render() {
            Ok(epoch) => epoch,
            Err(result) => return result,
        };
        // Starting or Running now excludes control-thread preparation before
        // the callback takes its unique audio-thread render-graph borrow.
        let Some(render) = (unsafe { &mut *session.spatial_render.get() }).as_mut() else {
            if let Some(epoch) = first_render_epoch {
                session.spatial_lifecycle.finish_first_render_failure(epoch);
            }
            return FbResult::FbInvalidState;
        };
        let runtime_result = render.graph.process_spatial_block(SpatialProcessBlock {
            now_ns,
            block_start_frame,
            sources: &source_blocks[..validated.program_count],
            presentation_bank: &mut render.presentation_bank,
            environmental_bank: &mut render.environmental_bank,
            metadata: &mut render.metadata,
        });
        if let Err(error) = runtime_result {
            if let Some(epoch) = first_render_epoch {
                session.spatial_lifecycle.finish_first_render_failure(epoch);
            }
            return spatial_render_error_to_ffi(error);
        }
        if first_render_epoch.is_some() {
            // RuntimeGraph advanced. Running becomes visible before any later
            // metadata mapping or caller scatter can diagnose an invariant.
            session.spatial_lifecycle.finish_first_render_success();
        }
        // RuntimeGraph has now advanced its source clocks even if a later
        // impossible metadata-mapping invariant is diagnosed. Keep the public
        // frame clock in the same committed state before scattering outputs.
        session
            .spatial_block_start_frame
            .store(next_block_start_frame, Ordering::Release);

        let mapped = match map_spatial_output_metadata_v2(&render.metadata) {
            Ok(mapped) => mapped,
            Err(result) => return result,
        };
        // Safety: validation proved every destination range writable and
        // pairwise disjoint. Mapping completed before the first caller write.
        unsafe {
            scatter_spatial_output_v2(
                &validated,
                &render.presentation_bank,
                &render.environmental_bank,
                &mapped,
                session.block_size,
            )
        };
        let elapsed = realtime_clock.elapsed_ns(started);
        render.timing_writer.record(elapsed.max(1));
        FbResult::FbOk
    })
}

/// Publishes a listener pose and advances the control-side simulation cadence.
///
/// Thread safety: control thread only. Calls must not overlap other update or
/// telemetry calls. This may run Steam Audio simulation and must never be
/// called from the audio callback. It may run concurrently with render.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_listener(
    session: *mut FbSession,
    pose: *const FbPose,
    linear_velocity_mps: *const FbVec3,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_const_ptr(pose) || !valid_const_ptr(linear_velocity_mps) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: both input pointers were checked and are borrowed for this call.
        let pose = match pose_from_ffi(unsafe { *pose }) {
            Some(value) => value,
            None => return FbResult::FbInvalidArgument,
        };
        // Safety: pointer was checked above.
        let velocity = match vector_from_ffi(unsafe { *linear_velocity_mps }) {
            Some(value) => value,
            None => return FbResult::FbInvalidArgument,
        };
        let listener = ListenerState {
            pose,
            linear_velocity_mps: velocity,
        };
        if session.route == SessionRoute::NeutralSpatial {
            // Safety: the API requires serialized control-thread access.
            let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
                return FbResult::FbInvalidState;
            };
            if control.macro_bridge.enabled
                && control.macro_bridge.reservation.is_some_and(|reservation| {
                    reservation.state != MacroBridgeReservationState::Committed
                })
            {
                return FbResult::FbInvalidState;
            }
            // Invalidate preparation before publishing any possibly newer
            // control truth. The packed epoch also makes a concurrent first
            // callback's failure rollback unambiguous.
            session.spatial_lifecycle.note_control_update();
            control.update.listener = listener;
            let cadence_before = control.update_sequence;
            let result = advance_spatial_simulation(session, control);
            if control.update_sequence != cadence_before {
                control.granular_listener_advances =
                    control.granular_listener_advances.saturating_add(1);
            }
            if result == FbResult::FbOk {
                control.listener_published = true;
            }
            return result;
        }

        // Safety: the API requires serialized control-thread access.
        let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        control.update.listener = listener;
        control.orientation_writer.publish(ListenerOrientation {
            forward: pose.forward,
            up: pose.up,
        });
        advance_simulation(session, control)
    })
}

/// Publishes one source's motion and advances the control-side simulation cadence.
///
/// Thread safety: control thread only, serialized with listener updates and
/// telemetry. `source_index` is stable and zero-based.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_source(
    session: *mut FbSession,
    source_index: u32,
    update: *const FbSourceUpdate,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        let Ok(index) = usize::try_from(source_index) else {
            return FbResult::FbInvalidArgument;
        };
        if index >= session.source_count || !valid_const_ptr(update) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: `update` was checked and is borrowed for this call.
        let update = unsafe { *update };
        let Some(pose) = pose_from_ffi(update.pose) else {
            return FbResult::FbInvalidArgument;
        };
        let Some(velocity) = vector_from_ffi(update.linear_velocity_mps) else {
            return FbResult::FbInvalidArgument;
        };
        let active = update.active != 0;
        let source_motion = SourceMotion {
            active,
            pose,
            linear_velocity_mps: velocity,
        };
        if session.route == SessionRoute::NeutralSpatial {
            // Safety: the API requires serialized control-thread access.
            let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
                return FbResult::FbInvalidState;
            };
            if control.macro_bridge.enabled {
                let reserved_source = EventRole::ALL.into_iter().any(|role| {
                    fightbox_steam_audio::MacroIngressSlotMap::default().source_index(role) == index
                });
                if reserved_source
                    || control.macro_bridge.reservation.is_some_and(|reservation| {
                        reservation.state != MacroBridgeReservationState::Committed
                    })
                {
                    return FbResult::FbInvalidState;
                }
            }
            // See the listener path above. Even a backend failure may have
            // committed newer direct truth before a later phase failed, so
            // conservatively revoke a not-yet-consumed preparation first.
            session.spatial_lifecycle.note_control_update();
            control.update.sources[index] = source_motion;
            if control.macro_bridge.enabled {
                control
                    .macro_bridge
                    .overlay
                    .overlay_backend_update(&mut control.update);
            }
            let cadence_before = control.update_sequence;
            let result = advance_spatial_simulation(session, control);
            if control.update_sequence != cadence_before {
                control.granular_source_advances =
                    control.granular_source_advances.saturating_add(1);
            }
            if result == FbResult::FbOk {
                control.source_published[index] = true;
            }
            return result;
        }

        // Safety: the API requires serialized control-thread access.
        let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
            return FbResult::FbInvalidState;
        };
        control.update.sources[index] = source_motion;
        control
            .active_sources_writer
            .publish(std::array::from_fn(|source_index| {
                control.update.sources[source_index].active
            }));
        advance_simulation(session, control)
    })
}

/// Publishes one complete listener/source control frame and advances simulation once.
///
/// The frame carries exactly one source record for every stable source index.
/// Its current prefixes are copied and semantically validated in full before
/// either route's control truth, publications, or lifecycle state can change.
/// A valid frame uses the same synchronous direct/path/reflection scheduler as
/// the granular calls. Successful direct simulation consumes exactly one
/// cadence tick for the whole frame; direct failure consumes none, while a
/// later pathing or reflection failure retains the already-consumed tick. Both
/// legacy-final-stereo and neutral-spatial sessions accept it.
/// `FbInvalidArgument` is a total no-op. `FbBackendError` is not transactional:
/// neutral stages the accepted update and revokes pre-render preparation before
/// direct simulation, then publishes correlated propagation after direct
/// succeeds; legacy publishes its frozen orientation and activity snapshots
/// before direct simulation.
/// Neutral publication remains correlated by the direct-generation token.
/// Legacy deliberately preserves its frozen independent orientation, active,
/// and Steam propagation snapshots, including their bounded adjacent-snapshot
/// callback skew; this additive entry point does not reopen that fallback ABI.
///
/// Thread safety: control thread only, serialized with granular updates,
/// preparation, and telemetry. This may run Steam Audio simulation and must
/// never be called from the audio callback.
///
/// # Safety
///
/// `session` must be a live handle. `frame` must name at least its readable
/// eight-byte V2 header and, when that header advertises the current size, the
/// complete current prefix. Its source pointer must name one contiguous
/// readable allocation spanning `(source_count - 1) * stride + 52` bytes for
/// the declared strided source records.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_update_control_frame_v2(
    session: *mut FbSession,
    frame: *const FbControlFrameV2,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };

        // Construction-state failure wins before caller-owned frame memory is
        // read. Once a route has a control half, all frame bytes are copied and
        // validated before that control half is mutably borrowed.
        let has_control = if session.route == SessionRoute::NeutralSpatial {
            // Safety: the API requires serialized control-thread access.
            unsafe { (&*session.spatial_control.get()).is_some() }
        } else {
            // Safety: the API requires serialized control-thread access.
            unsafe { (&*session.control.get()).is_some() }
        };
        if !has_control {
            return FbResult::FbInvalidState;
        }

        // Safety: the C contract supplies the universal header and every
        // readable strided source prefix for this serialized call.
        let mut next_update = match unsafe { decode_control_update_v2(frame, session.source_count) }
        {
            Ok(update) => update,
            Err(result) => return result,
        };

        if session.route == SessionRoute::NeutralSpatial {
            // Safety: availability was checked above and the API requires
            // serialized control-thread access.
            let control = unsafe { (&mut *session.spatial_control.get()).as_mut() }
                .expect("neutral control availability was checked");
            observe_spatial_callback_timings(control);
            let block_start_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
            if control.macro_bridge.enabled {
                if control
                    .macro_bridge
                    .overlay
                    .validate_complete_host_update(&next_update)
                    .is_err()
                    || control.macro_bridge.reservation.is_some_and(|reservation| {
                        reservation.state != MacroBridgeReservationState::Committed
                    })
                {
                    return FbResult::FbInvalidState;
                }
                control
                    .macro_bridge
                    .overlay
                    .overlay_backend_update(&mut next_update);
                let runtime_activity =
                    control
                        .macro_bridge
                        .overlay
                        .overlay_runtime_activity(std::array::from_fn(|index| {
                            next_update.sources[index].active
                        }));
                return commit_spatial_control_frame_phases_with_runtime_activity(
                    &session.spatial_lifecycle,
                    &mut control.runner,
                    &mut control.update,
                    next_update,
                    Some(runtime_activity),
                    &mut control.update_sequence,
                    &mut control.propagation_writer,
                    block_start_frame,
                    session.sample_rate_hz,
                    &mut control.listener_published,
                    &mut control.source_published,
                    session.source_count,
                    &mut control.batched_frame_advances,
                );
            }
            return commit_spatial_control_frame_phases(
                &session.spatial_lifecycle,
                &mut control.runner,
                &mut control.update,
                next_update,
                &mut control.update_sequence,
                &mut control.propagation_writer,
                block_start_frame,
                session.sample_rate_hz,
                &mut control.listener_published,
                &mut control.source_published,
                session.source_count,
                &mut control.batched_frame_advances,
            );
        }

        // Safety: availability was checked above and the API requires
        // serialized control-thread access.
        let control = unsafe { (&mut *session.control.get()).as_mut() }
            .expect("legacy control availability was checked");
        // Preserve the legacy renderer's independent frozen publications. The
        // batch coheres validation and simulation cadence, not callback-side
        // snapshot identity on the bit-identical fallback route.
        publish_legacy_control_frame(
            &mut control.update,
            &mut control.orientation_writer,
            &mut control.active_sources_writer,
            next_update,
        );
        advance_simulation(session, control)
    })
}

/// Renders exactly one configured block.
///
/// `source_mono` contains `source_count * block_size_frames` finite samples in
/// source-major order. `out_interleaved_stereo` contains
/// `block_size_frames * 2` writable samples and must not overlap the input.
///
/// Thread safety: one audio thread only. The function is allocation-free and
/// lock-free after construction. It may run concurrently with control updates.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_render_block(
    session: *mut FbSession,
    source_mono: *const f32,
    source_sample_count: usize,
    out_interleaved_stereo: *mut f32,
    out_sample_count: usize,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if session.route != SessionRoute::LegacyFinalStereo {
            return FbResult::FbInvalidState;
        }
        let expected_input = match session.source_count.checked_mul(session.block_size) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        let expected_output = match session.block_size.checked_mul(2) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        if source_sample_count != expected_input
            || out_sample_count != expected_output
            || !valid_slice_ptr(source_mono, source_sample_count)
            || !valid_mut_slice_ptr(out_interleaved_stereo, out_sample_count)
            || ranges_overlap(
                source_mono.cast::<u8>(),
                source_sample_count.saturating_mul(size_of::<f32>()),
                out_interleaved_stereo.cast::<u8>(),
                out_sample_count.saturating_mul(size_of::<f32>()),
            )
        {
            return FbResult::FbInvalidArgument;
        }
        // Safety: pointer, length, alignment, and non-overlap were validated.
        let input = unsafe { slice::from_raw_parts(source_mono, source_sample_count) };
        // Safety: pointer, length, alignment, and non-overlap were validated.
        let output = unsafe { slice::from_raw_parts_mut(out_interleaved_stereo, out_sample_count) };
        if input.iter().any(|sample| !sample.is_finite()) {
            output.fill(0.0);
            return FbResult::FbInvalidArgument;
        }
        // Safety: the API requires serialized audio-thread access.
        let Some(render) = (unsafe { &mut *session.render.get() }).as_mut() else {
            output.fill(0.0);
            return FbResult::FbInvalidState;
        };

        render.left.fill(0.0);
        render.right.fill(0.0);
        let empty = &[];
        let mut sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: empty,
        }; MAX_ACTIVE_SOURCES];
        let mut active_count = 0;
        let active_sources = render.active_sources_reader.read();
        for index in 0..session.source_count {
            if active_sources[index] {
                let start = index * session.block_size;
                sources[active_count] = BackendSourceBlock {
                    source_index: index,
                    input_mono: &input[start..start + session.block_size],
                };
                active_count += 1;
            }
        }
        let started = render.realtime_clock.start();
        let result = render.graph.render_block(PropagationRenderBlock {
            listener_orientation: render.orientation_reader.read(),
            sources: &sources[..active_count],
            output_left: &mut render.left,
            output_right: &mut render.right,
        });
        let elapsed = render.realtime_clock.elapsed_ns(started);
        session
            .last_render_ns
            .store(elapsed.max(1), Ordering::Release);
        if result.is_err()
            || render
                .left
                .iter()
                .chain(&render.right)
                .any(|sample| !sample.is_finite())
        {
            output.fill(0.0);
            return FbResult::FbBackendError;
        }
        for (frame, pair) in output.chunks_exact_mut(2).enumerate() {
            pair[0] = render.left[frame];
            pair[1] = render.right[frame];
        }
        FbResult::FbOk
    })
}

/// Copies a NUL-terminated delivered-quality/timing JSON snapshot.
///
/// Thread safety: control thread only, serialized with updates. `out_required`
/// always receives the required byte count including the NUL terminator.
/// Passing a null buffer with zero capacity is the supported size query.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_telemetry_json(
    session: *mut FbSession,
    buffer: *mut c_char,
    buffer_capacity: usize,
    out_required: *mut usize,
) -> FbResult {
    ffi_boundary(|| {
        let Some(session) = (unsafe { session_ref(session) }) else {
            return FbResult::FbInvalidArgument;
        };
        if !valid_mut_ptr(out_required)
            || (buffer_capacity != 0 && !valid_mut_slice_ptr(buffer, buffer_capacity))
            || (buffer_capacity == 0 && !buffer.is_null())
        {
            return FbResult::FbInvalidArgument;
        }
        let json = if session.route == SessionRoute::NeutralSpatial {
            // Safety: the API requires serialized control-thread access.
            let Some(control) = (unsafe { &mut *session.spatial_control.get() }).as_mut() else {
                return FbResult::FbInvalidState;
            };
            observe_spatial_callback_timings(control);
            synchronize_adopted_cell_authority(control);
            spatial_telemetry_json(
                control.runner.quality_governor_telemetry(),
                control.memory,
                &control.callback_timing_run,
                control.callback_timing_run_max_observation,
                control.timing_reader.dropped_observations(),
                control.preparation,
                session.spatial_lifecycle.public_name(),
                SpatialControlScheduleTelemetry {
                    cadence_advances: control.update_sequence,
                    batched_frame_advances: control.batched_frame_advances,
                    granular_listener_advances: control.granular_listener_advances,
                    granular_source_advances: control.granular_source_advances,
                },
                &control.macro_ingress.telemetry(),
                control.macro_atmosphere,
                control.macro_atmosphere_locked,
                control.active_echo_authority.as_deref(),
                control.pending_cell_authority.is_some(),
                control.macro_bridge.echo_telemetry,
                control
                    .macro_bridge
                    .echo_authority_pins
                    .iter()
                    .filter(|pin| pin.is_some())
                    .count(),
            )
        } else {
            // Safety: the API requires serialized control-thread access.
            let Some(control) = (unsafe { &mut *session.control.get() }).as_mut() else {
                return FbResult::FbInvalidState;
            };
            observe_latest_render_timing(session, control);
            telemetry_json(
                control.runner.quality_governor_telemetry(),
                session.ffi_render_buffers_bytes,
            )
        };
        let required = match json.len().checked_add(1) {
            Some(value) => value,
            None => return FbResult::FbInvalidState,
        };
        // Safety: `out_required` was validated above.
        unsafe { out_required.write(required) };
        if buffer_capacity < required {
            return FbResult::FbBufferTooSmall;
        }
        // Safety: capacity is sufficient and buffer was validated.
        unsafe {
            ptr::copy_nonoverlapping(json.as_ptr(), buffer.cast::<u8>(), json.len());
            buffer.add(json.len()).write(0);
        }
        FbResult::FbOk
    })
}

/// Destroys a session and releases all Rust and Steam Audio resources.
///
/// Thread safety: call only after the control and audio threads are stopped
/// and joined. A handle must be destroyed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fb_session_destroy(session: *mut FbSession) -> FbResult {
    ffi_boundary(|| {
        if !valid_session_ptr(session) {
            return FbResult::FbInvalidArgument;
        }
        // Safety: the caller transfers the unique allocation back exactly once.
        drop(unsafe { Box::from_raw(session.cast::<SessionInner>()) });
        FbResult::FbOk
    })
}

impl SessionInner {
    fn configure_spatial_source(&self, source_index: usize, shape: SpatialSourceShape) -> FbResult {
        if self.spatial_lifecycle.lifecycle() != SpatialShellLifecycle::Collecting {
            return FbResult::FbInvalidState;
        }
        let build_inputs = {
            // Safety: source configuration is serialized on the control thread
            // and finishes before the audio thread may access spatial state.
            let Some(spatial) = (unsafe { &mut *self.spatial.get() }).as_mut() else {
                return FbResult::FbInvalidState;
            };
            if let Err(result) = spatial.stage_source_shape(source_index, shape) {
                return result;
            }
            if spatial.configured_count != self.source_count {
                return FbResult::FbOk;
            }
            let Some(build_inputs) = spatial.build_inputs.take() else {
                spatial.rollback_source_shape(source_index);
                return FbResult::FbInvalidState;
            };
            build_inputs
        };

        let (source_shapes, macro_bridge_requested, macro_diffuse_profile) = {
            // Safety: this is the same serialized construction phase.
            let spatial = unsafe { &*self.spatial.get() };
            let spatial = spatial
                .as_ref()
                .expect("neutral session retains its source-shape shell");
            (
                spatial.source_shapes,
                spatial.macro_bridge_requested,
                spatial.macro_diffuse_profile,
            )
        };
        let binding = build_spatial_binding(
            build_inputs,
            &source_shapes,
            self.source_count,
            self.block_size,
            macro_bridge_requested,
            macro_diffuse_profile,
        );
        let (control, render) = match binding {
            Ok(binding) => binding,
            Err((result, build_inputs)) => {
                // Safety: a failed construction never exposes either half to
                // the audio thread; restore the final shape as retryable.
                let spatial = unsafe { &mut *self.spatial.get() };
                let spatial = spatial
                    .as_mut()
                    .expect("neutral session retains its source-shape shell");
                spatial.build_inputs = Some(build_inputs);
                spatial.rollback_source_shape(source_index);
                return result;
            }
        };

        // Safety: these cells are empty and control-thread-owned until this
        // construction barrier completes. The public contract starts audio
        // only after the final configure call returns.
        unsafe {
            *self.spatial_control.get() = Some(control);
            *self.spatial_render.get() = Some(render);
        }
        // Safety: same serialized construction phase; both bound halves are
        // fully initialized before readiness becomes observable.
        let spatial = unsafe { &mut *self.spatial.get() };
        spatial
            .as_mut()
            .expect("neutral session retains its source-shape shell")
            .mark_bound(self.source_count);
        self.spatial_lifecycle.mark_bound_after_configuration();
        FbResult::FbOk
    }

    fn create_spatial_shell(
        config: FbSessionConfigV2,
        package_path: &Path,
        bake_path: &Path,
    ) -> Result<Self, FbResult> {
        let source_count = config.source_count as usize;
        let block_size = config.block_size_frames as usize;
        let quality_tier =
            quality_tier_from_ffi(config.quality_tier).ok_or(FbResult::FbInvalidArgument)?;
        let loaded =
            fightbox_world::read_package(package_path).map_err(|_| FbResult::FbInvalidPackage)?;
        let mesh = scene_mesh(&loaded)?;
        let baked = load_bake(bake_path)?;
        verify_bake_identity(&loaded, bake_path, &baked)?;
        let cell_authority = cell_authority_seed(&loaded, package_path, &baked)?;
        let build_inputs = SpatialBuildInputs {
            mesh,
            baked,
            cell_authority,
            audio: AudioConfig {
                sample_rate_hz: config.sample_rate_hz as i32,
                frame_size: config.block_size_frames as i32,
            },
            simulation: quality_tier.simulation_defaults(),
            quality_tier,
            default_source_level_db: config.default_source_level_db,
            environmental_order: config.environmental_order as usize,
        };
        let spatial_realtime_clock =
            RealtimeClock::new().map_err(|_| FbResult::FbBackendUnavailable)?;
        Ok(Self {
            control: UnsafeCell::new(None),
            render: UnsafeCell::new(None),
            spatial_control: UnsafeCell::new(None),
            spatial_render: UnsafeCell::new(None),
            source_count,
            sample_rate_hz: config.sample_rate_hz,
            block_size,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: (block_size as u64)
                .saturating_mul(
                    (MAX_SPATIAL_PRESENTATION_FEEDS + MAX_SPATIAL_ENVIRONMENT_PLANES) as u64,
                )
                .saturating_mul(size_of::<f32>() as u64),
            route: SessionRoute::NeutralSpatial,
            spatial: UnsafeCell::new(Some(SpatialShellState::with_build_inputs(Some(
                build_inputs,
            )))),
            spatial_lifecycle: SpatialLifecycleState::new(SpatialShellLifecycle::Collecting),
            spatial_realtime_clock: Some(spatial_realtime_clock),
            spatial_block_start_frame: AtomicU64::new(0),
        })
    }

    fn create(
        config: FbSessionConfig,
        package_path: &Path,
        bake_path: &Path,
    ) -> Result<Self, FbResult> {
        let source_count =
            usize::try_from(config.source_count).map_err(|_| FbResult::FbInvalidArgument)?;
        let block_size =
            usize::try_from(config.block_size_frames).map_err(|_| FbResult::FbInvalidArgument)?;
        let quality_tier =
            quality_tier_from_ffi(config.quality_tier).ok_or(FbResult::FbInvalidArgument)?;
        if config.sample_rate_hz == 0
            || block_size == 0
            || source_count == 0
            || source_count > quality_tier.active_source_cap()
            || !config.default_source_level_db.is_finite()
            || i32::try_from(config.sample_rate_hz).is_err()
            || i32::try_from(config.block_size_frames).is_err()
        {
            return Err(FbResult::FbInvalidArgument);
        }
        let loaded =
            fightbox_world::read_package(package_path).map_err(|_| FbResult::FbInvalidPackage)?;
        let mesh = scene_mesh(&loaded)?;
        let baked = load_bake(bake_path)?;
        verify_bake_identity(&loaded, bake_path, &baked)?;
        let descriptors = (0..source_count)
            .map(|_| {
                MultiSourceDescriptor::at(EnuVector3::default()).with_reference_level(
                    ReferenceLevel::CreativeDb {
                        db: config.default_source_level_db,
                    },
                )
            })
            .collect::<Vec<_>>();
        let audio = AudioConfig {
            sample_rate_hz: config.sample_rate_hz as i32,
            frame_size: config.block_size_frames as i32,
        };
        let simulation = quality_tier.simulation_defaults();
        let (mut runner, graph) = build_multi_source_session_for_tier(
            &mesh,
            &baked,
            audio,
            simulation,
            &descriptors,
            quality_tier,
        )
        .map_err(|error| {
            if matches!(error, fightbox_steam_audio::BackendError::SdkUnavailable(_)) {
                FbResult::FbBackendUnavailable
            } else {
                FbResult::FbBackendError
            }
        })?;
        let update = default_simulation_update();
        runner.update_inputs(&update);
        runner
            .run_direct()
            .and_then(|_| runner.run_pathing())
            .and_then(|_| runner.run_reflections())
            .map_err(|_| FbResult::FbBackendError)?;
        let realtime_clock = RealtimeClock::new().map_err(|_| FbResult::FbBackendUnavailable)?;
        let listener = update.listener.pose;
        let (orientation_writer, orientation_reader) =
            SnapshotPublication::new(ListenerOrientation {
                forward: listener.forward,
                up: listener.up,
            });
        let (active_sources_writer, active_sources_reader) =
            SnapshotPublication::new([false; MAX_ACTIVE_SOURCES]);
        Ok(Self {
            control: UnsafeCell::new(Some(ControlState {
                runner,
                update,
                update_sequence: 0,
                orientation_writer,
                active_sources_writer,
            })),
            render: UnsafeCell::new(Some(RenderState {
                graph,
                left: vec![0.0; block_size],
                right: vec![0.0; block_size],
                orientation_reader,
                active_sources_reader,
                realtime_clock,
            })),
            spatial_control: UnsafeCell::new(None),
            spatial_render: UnsafeCell::new(None),
            source_count,
            sample_rate_hz: config.sample_rate_hz,
            block_size,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: (block_size as u64)
                .saturating_mul(2)
                .saturating_mul(size_of::<f32>() as u64),
            route: SessionRoute::LegacyFinalStereo,
            spatial: UnsafeCell::new(None),
            spatial_lifecycle: SpatialLifecycleState::new(SpatialShellLifecycle::Collecting),
            spatial_realtime_clock: None,
            spatial_block_start_frame: AtomicU64::new(0),
        })
    }
}

// The construction error returns ownership of the complete loaded world so a
// rejected final source shape remains retryable without re-reading the pack.
#[allow(clippy::result_large_err)]
fn build_spatial_binding(
    build_inputs: SpatialBuildInputs,
    source_shapes: &[SpatialSourceShape; MAX_ACTIVE_SOURCES],
    source_count: usize,
    block_size: usize,
    macro_bridge_requested: bool,
    macro_diffuse_profile: fightbox_api::diffuse::DiffuseFieldProfile,
) -> Result<(SpatialControlState, SpatialRenderState), (FbResult, SpatialBuildInputs)> {
    let result = (|| {
        let reference_level = ReferenceLevel::CreativeDb {
            db: build_inputs.default_source_level_db,
        };
        if macro_bridge_requested && source_count != MAX_ACTIVE_SOURCES {
            return Err(FbResult::FbInvalidState);
        }
        let macro_slots = fightbox_steam_audio::MacroIngressSlotMap::default();
        let mut descriptors = Vec::with_capacity(source_count);
        let mut program_channel_counts = Vec::with_capacity(source_count);
        for (source_index, shape) in source_shapes[..source_count].iter().enumerate() {
            let macro_role = macro_bridge_requested.then(|| {
                EventRole::ALL
                    .into_iter()
                    .find(|role| macro_slots.source_index(*role) == source_index)
            });
            let descriptor = if let Some(Some(role)) = macro_role {
                if shape.channel_count != 1
                    || shape.source_geometry != FbSourceGeometryV2::FbSourceGeometryPointV2 as u32
                {
                    return Err(FbResult::FbInvalidState);
                }
                macro_slots
                    .descriptor(role, EnuVector3::default())
                    .with_reference_level(reference_level)
            } else {
                MultiSourceDescriptor::at(EnuVector3::default())
                    .with_reference_level(reference_level)
                    .with_extent(extent_from_spatial_shape(*shape))
            };
            let descriptor = build_inputs
                .cell_authority
                .metadata_city_offset_enu
                .map_or(descriptor, |offset| {
                    descriptor.with_metadata_city_offset(offset)
                });
            descriptors.push(descriptor);
            program_channel_counts.push(usize::from(shape.channel_count));
        }

        let mono_expanded_requested = source_shapes[..source_count].iter().any(|shape| {
            shape.presentation_provenance == SpatialPresentationProvenance::MonoExpanded
        });
        let (mut runner, spatial_backend) = build_spatial_multi_source_session_for_tier(
            &build_inputs.mesh,
            &build_inputs.baked,
            build_inputs.audio,
            build_inputs.simulation,
            &descriptors,
            &program_channel_counts,
            build_inputs.environmental_order,
            build_inputs.quality_tier,
        )
        .map_err(|error| {
            if mono_expanded_requested
                || matches!(error, fightbox_steam_audio::BackendError::SdkUnavailable(_))
            {
                FbResult::FbBackendUnavailable
            } else {
                FbResult::FbBackendError
            }
        })?;
        let neutral_graph_memory = spatial_backend.persistent_memory();

        let update = default_simulation_update();
        let construction_governor_before = runner.quality_governor_telemetry();
        // Seed one exact direct/path/reflection truth through the dedicated
        // non-accounting control seam. Construction still returns a bound,
        // unprepared public session: only `fb_session_prepare_spatial_v2` may
        // prepare Runtime's backend after the caller publishes actual truth.
        runner
            .prepare_simulation_for_realtime(&update)
            .map_err(|_| FbResult::FbBackendError)?;
        if let (Some(before), Some(after)) = (
            construction_governor_before,
            runner.quality_governor_telemetry(),
        ) {
            debug_assert_eq!(after.ladder_position, before.ladder_position);
            debug_assert_eq!(after.reason, before.reason);
            debug_assert_eq!(after.p50_ns, before.p50_ns);
            debug_assert_eq!(after.p95_ns, before.p95_ns);
            debug_assert_eq!(after.p99_ns, before.p99_ns);
            debug_assert_eq!(after.p99_9_ns, before.p99_9_ns);
            debug_assert_eq!(
                after.callback_deadline_misses,
                before.callback_deadline_misses
            );
            debug_assert_eq!(after.simulation_lateness_ns, before.simulation_lateness_ns);
        }

        // The construction seed publishes one successful direct generation.
        // Runtime's initial activity snapshot must carry that exact token or
        // the first callback would correctly reject the mismatched pair.
        let initial_direct_sequence = runner.latest_direct_sequence();
        let initial_world_offset_enu = build_inputs.cell_authority.metadata_city_offset_enu;
        let initial_echo_authority = build_inputs.cell_authority.echo_authority.clone();
        let initial_cell_authority = build_inputs
            .cell_authority
            .bind(runner.world_diagnostics().generation);
        let macro_ingress = Box::new(MacroLocalIngress::new(
            initial_cell_authority.cell.clone(),
            Some(initial_cell_authority),
        ));
        let (command_publisher, macro_command_receiver) = steam_macro_ingress_activation_channel();
        let (macro_acknowledgement_publisher, acknowledgement_receiver) =
            steam_macro_ingress_acknowledgement_channel();
        let (macro_route_writer, macro_route_reader) = macro_ingress_render_snapshot_channel();
        let (spatial_backend, macro_wrapper_payload_bytes): (
            Box<dyn SpatialBackendRenderGraph>,
            u64,
        ) = if macro_bridge_requested {
            let wrapper = MacroIngressSpatialRenderGraph::new(
                Box::new(spatial_backend),
                macro_route_reader,
                macro_slots,
                build_inputs.audio.sample_rate_hz as u32,
                block_size,
                macro_diffuse_profile,
            )
            .map_err(|_| FbResult::FbBackendError)?;
            let payload_bytes = wrapper.persistent_payload_bytes();
            (Box::new(wrapper), payload_bytes)
        } else {
            (Box::new(spatial_backend), 0)
        };
        let initial_snapshot =
            propagation_snapshot_from_update(&update, initial_direct_sequence, 0);
        let (propagation_writer, propagation_reader) = SnapshotPublication::new(initial_snapshot);
        let runtime_config = EngineConfig {
            sample_rate_hz: build_inputs.audio.sample_rate_hz as u32,
            block_size_frames: build_inputs.audio.frame_size as u32,
            max_active_sources: source_count as u8,
            ..EngineConfig::default()
        };
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            runtime_config,
            propagation_reader,
            &program_channel_counts,
            spatial_backend,
        )
        .map_err(|_| FbResult::FbBackendError)?;
        graph.set_listener_state(update.listener);
        for (source_index, shape) in source_shapes[..source_count].iter().copied().enumerate() {
            let profile = source_profile_for_spatial_shape(source_index, shape, reference_level)?;
            graph
                .set_source(source_index, &profile, SceneCalibration::default())
                .map_err(|_| FbResult::FbBackendError)?;
        }
        let runtime_graph_memory = graph.persistent_memory();
        let (timing_writer, timing_reader) = CallbackTimingPublication::new();

        let presentation_sample_count = MAX_SPATIAL_PRESENTATION_FEEDS
            .checked_mul(block_size)
            .ok_or(FbResult::FbBackendError)?;
        let environmental_sample_count = MAX_SPATIAL_ENVIRONMENT_PLANES
            .checked_mul(block_size)
            .ok_or(FbResult::FbBackendError)?;
        let presentation_bank = vec![0.0; presentation_sample_count];
        let environmental_bank = vec![0.0; environmental_sample_count];
        let memory = SpatialBindingMemoryTelemetry {
            neutral_graph: neutral_graph_memory,
            runtime_graph: runtime_graph_memory,
            ffi_presentation_bank_payload_bytes: (presentation_bank.capacity() as u64)
                .saturating_mul(size_of::<f32>() as u64),
            ffi_environmental_bank_payload_bytes: (environmental_bank.capacity() as u64)
                .saturating_mul(size_of::<f32>() as u64),
            propagation_snapshot_publication_payload_bytes:
                SnapshotPublication::shared_payload_bytes::<PropagationSnapshot>(),
            callback_timing_publication_payload_bytes:
                CallbackTimingPublication::shared_payload_bytes(),
            macro_route_publication_payload_bytes: SnapshotPublication::shared_payload_bytes::<
                MacroIngressRenderSnapshot,
            >(),
            macro_command_mailbox_payload_bytes: steam_macro_ingress_command_channel_payload_bytes(
            ),
            macro_acknowledgement_mailbox_payload_bytes:
                steam_macro_ingress_acknowledgement_channel_payload_bytes(),
            macro_render_graph_payload_bytes: macro_wrapper_payload_bytes,
        };
        Ok((
            SpatialControlState {
                runner,
                active_world_offset_enu: initial_world_offset_enu,
                prepared_cell_reserved: Arc::new(AtomicBool::new(false)),
                macro_ingress,
                macro_bridge: MacroBridgeControl {
                    enabled: macro_bridge_requested,
                    next_token_id: 1,
                    reservation: None,
                    overlay: MacroIngressControlOverlay::default(),
                    command_publisher,
                    acknowledgement_receiver,
                    pending_acknowledgements: [None; EventRole::COUNT],
                    echo_authority_pins: std::array::from_fn(|_| None),
                    echo_telemetry: MacroEchoBridgeTelemetry::default(),
                },
                macro_atmosphere: FrozenAtmosphere::freeze(None),
                macro_atmosphere_locked: false,
                active_echo_authority: initial_echo_authority,
                pending_cell_authority: None,
                pending_echo_authority: None,
                update,
                update_sequence: 0,
                batched_frame_advances: 0,
                granular_listener_advances: 0,
                granular_source_advances: 0,
                listener_published: false,
                source_published: [false; MAX_ACTIVE_SOURCES],
                propagation_writer,
                timing_reader,
                callback_timing_run: RunTimingHistogram::default(),
                callback_timing_run_max_observation: None,
                preparation: SpatialPreparationTelemetry::default(),
                memory,
            },
            SpatialRenderState {
                graph,
                macro_audio: MacroBridgeAudio {
                    enabled: macro_bridge_requested,
                    command_receiver: macro_command_receiver,
                    acknowledgement_publisher: macro_acknowledgement_publisher,
                    pending_commands: None,
                    route_writer: macro_route_writer,
                    route_snapshot: MacroIngressRenderSnapshot::default(),
                    roles: [MacroAudioRole::default(); EventRole::COUNT],
                    pending_acknowledgements: [None; EventRole::COUNT],
                    staged_block_start_frame: None,
                },
                presentation_bank,
                environmental_bank,
                metadata: SpatialOutputMetadata::default(),
                timing_writer,
            },
        ))
    })();

    result.map_err(|result| (result, build_inputs))
}

fn extent_from_spatial_shape(shape: SpatialSourceShape) -> ExtentDescriptor {
    match shape.source_geometry {
        value if value == FbSourceGeometryV2::FbSourceGeometryPointV2 as u32 => {
            ExtentDescriptor::Point
        }
        value if value == FbSourceGeometryV2::FbSourceGeometryMultiPointV2 as u32 => {
            ExtentDescriptor::MultiPoint {
                count: shape.multipoint_count,
            }
        }
        value if value == FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32 => {
            ExtentDescriptor::LineSegment {
                length_m: shape.extent_m,
            }
        }
        value if value == FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32 => {
            ExtentDescriptor::StereoImage {
                width_m: shape.extent_m,
            }
        }
        _ => unreachable!("validated V2 source geometry"),
    }
}

fn source_profile_for_spatial_shape(
    source_index: usize,
    shape: SpatialSourceShape,
    reference_level: ReferenceLevel,
) -> Result<SourceProfile, FbResult> {
    let measurement_provenance = AssetMeasurementProvenance::new("fightbox-ffi-v2-creative-db/v1")
        .map_err(|_| FbResult::FbBackendError)?;
    let asset_analysis = AssetAnalysis::new(0.0, 0.0, measurement_provenance)
        .map_err(|_| FbResult::FbBackendError)?;
    Ok(SourceProfile {
        id: SourceId::new(format!("ffi-v2-source-{source_index}")),
        pose: default_pose(),
        reference_level,
        asset_analysis,
        extent: extent_from_spatial_shape(shape),
        directivity: Directivity::default(),
        max_speed_mps: f32::MAX,
    })
}

fn propagation_snapshot_from_update(
    update: &SimulationUpdate,
    sequence: u64,
    simulated_at_ns: u64,
) -> PropagationSnapshot {
    PropagationSnapshot {
        sequence,
        simulated_at_ns,
        sources: std::array::from_fn(|source_index| SourcePropagation {
            active: update.sources[source_index].active,
            // The neutral Steam graph owns all physical propagation delay and
            // gain. Runtime carries only valid neutral truth plus active state.
            target_delay_samples: 0.0,
            left_gain: 1.0,
            right_gain: 1.0,
        }),
    }
}

struct MappedSpatialOutputV2 {
    feeds: [FbPresentationFeedMetadataV2; MAX_SPATIAL_PRESENTATION_FEEDS],
    block: FbSpatialBlockMetadataV2,
}

fn map_spatial_output_metadata_v2(
    metadata: &SpatialOutputMetadata,
) -> Result<MappedSpatialOutputV2, FbResult> {
    let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
    let mut component_mask = 0_u32;
    let mut active_feed_count = 0_usize;
    for (destination, source) in feeds.iter_mut().zip(&metadata.presentation_feeds) {
        if !source.valid {
            continue;
        }
        destination.valid = 1;
        destination.source_index =
            u32::try_from(source.source_index).map_err(|_| FbResult::FbBackendError)?;
        destination.component = presentation_component_to_ffi(source.component);
        destination.placement = match source.placement {
            SpatialFeedPlacement::Pose => FbPresentationPlacementV2::FbPresentationPoseV2 as u32,
            SpatialFeedPlacement::Direction => {
                FbPresentationPlacementV2::FbPresentationDirectionV2 as u32
            }
        };
        destination.pose = pose_to_ffi(source.pose_enu);
        destination.direction_enu = vector_to_ffi(source.direction_enu);
        destination.processing_latency_frames = source.latency_frames;
        component_mask |= destination.component;
        active_feed_count += 1;
    }
    if active_feed_count != metadata.active_presentation_feed_count {
        return Err(FbResult::FbBackendError);
    }

    let validity = match metadata.validity {
        SpatialOutputValidity::Invalid => FbSpatialOutputValidityV2::FbSpatialInvalidV2,
        SpatialOutputValidity::Valid => FbSpatialOutputValidityV2::FbSpatialValidV2,
        SpatialOutputValidity::SilentDiscontinuity => {
            FbSpatialOutputValidityV2::FbSpatialSilentDiscontinuityV2
        }
    };
    let mut flags = 0_u32;
    if metadata.validity == SpatialOutputValidity::Valid {
        flags |= FB_SPATIAL_BLOCK_VALID_V2;
    }
    if metadata.validity == SpatialOutputValidity::SilentDiscontinuity {
        flags |= FB_SPATIAL_BLOCK_DISCONTINUITY_V2;
    }
    if metadata.source_safety_gain_applied {
        flags |= FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2;
    }
    if !metadata.output_limiter_applied {
        flags |= FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2;
    }
    if metadata.world_space_unrotated {
        flags |= FB_SPATIAL_WORLD_UNROTATED_V2;
    }
    if !metadata.final_hrtf_applied {
        flags |= FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2;
    }
    if metadata.source_drive_applied {
        flags |= FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2;
    }
    if !metadata.monitor_gain_applied {
        flags |= FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2;
    }

    let environmental_order = metadata.active_environmental_order.order();
    let environmental_channel_count = metadata.active_environmental_order.channel_count();
    if environmental_channel_count != metadata.active_environmental_plane_count {
        return Err(FbResult::FbBackendError);
    }
    let environmental_channel_order = match metadata.environmental_channel_order {
        SpatialAmbisonicChannelOrder::Acn => {
            FbEnvironmentalChannelOrderV2::FbEnvironmentalAcnV2 as u32
        }
    };
    let environmental_normalization = match metadata.environmental_normalization {
        SpatialAmbisonicNormalization::N3d => {
            FbEnvironmentalNormalizationV2::FbEnvironmentalN3dV2 as u32
        }
    };
    let environmental_basis = match metadata.environmental_basis {
        SpatialEnvironmentalBasis::RightHandedEnu => {
            FbEnvironmentalBasisV2::FbEnvironmentalRightHandedEnuV2 as u32
        }
        SpatialEnvironmentalBasis::RightHandedXRightYUpZBack => {
            FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2 as u32
        }
    };

    let active_presentation_feed_count =
        u32::try_from(active_feed_count).map_err(|_| FbResult::FbBackendError)?;
    let environmental_order =
        u32::try_from(environmental_order.unwrap_or(0)).map_err(|_| FbResult::FbBackendError)?;
    let environmental_channel_count =
        u32::try_from(environmental_channel_count).map_err(|_| FbResult::FbBackendError)?;
    let block = FbSpatialBlockMetadataV2 {
        sample_rate_hz: metadata.sample_rate_hz,
        block_size_frames: metadata.block_size_frames,
        block_start_frame: metadata.block_start_frame,
        generation: metadata.generation,
        discontinuity_sequence: metadata.discontinuity_sequence,
        validity: validity as u32,
        active_presentation_feed_count,
        environmental_order,
        environmental_channel_count,
        environmental_latency_frames: metadata.environmental_latency_frames,
        component_mask,
        flags,
        environmental_channel_order,
        environmental_normalization,
        environmental_basis,
        ..FbSpatialBlockMetadataV2::default()
    };
    Ok(MappedSpatialOutputV2 { feeds, block })
}

fn presentation_component_to_ffi(component: SpatialPresentationComponent) -> u32 {
    match component {
        SpatialPresentationComponent::DirectCenter => FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2,
        SpatialPresentationComponent::WidthPositive => FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2,
        SpatialPresentationComponent::WidthNegative => FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2,
        SpatialPresentationComponent::DiscreteEcho => FB_PRESENTATION_COMPONENT_DISCRETE_ECHO_V2,
    }
}

unsafe fn scatter_spatial_output_v2(
    destination: &ValidatedSpatialRenderBlockV2,
    presentation_bank: &[f32],
    environmental_bank: &[f32],
    metadata: &MappedSpatialOutputV2,
    block_size: usize,
) {
    for plane in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
        let source_offset = plane * block_size;
        let destination_offset = plane * destination.direct_stride_samples;
        // Safety: the validator proved this exact active destination plane is
        // writable and disjoint; both source and destination hold block_size.
        unsafe {
            ptr::copy_nonoverlapping(
                presentation_bank.as_ptr().add(source_offset),
                destination.direct_samples.add(destination_offset),
                block_size,
            )
        };
    }
    for plane in 0..MAX_SPATIAL_ENVIRONMENT_PLANES {
        let source_offset = plane * block_size;
        let destination_offset = plane * destination.environmental_stride_samples;
        // Safety: same validated plane contract for the environmental bank.
        unsafe {
            ptr::copy_nonoverlapping(
                environmental_bank.as_ptr().add(source_offset),
                destination.environmental_samples.add(destination_offset),
                block_size,
            )
        };
    }
    for (index, feed) in metadata.feeds.iter().copied().enumerate() {
        let offset = index * destination.feed_metadata_stride_bytes;
        let pointer = destination
            .feed_metadata
            .cast::<u8>()
            .wrapping_add(offset)
            .cast::<FbPresentationFeedMetadataV2>();
        // Safety: validation proved every strided metadata record writable,
        // aligned, and disjoint from all other active ranges.
        unsafe { pointer.write(feed) };
    }
    // Safety: validation checked the universal header, current writable size,
    // alignment, and non-overlap of the block metadata record.
    unsafe { destination.block_metadata.write(metadata.block) };
}

fn spatial_render_error_to_ffi(error: SpatialRenderError) -> FbResult {
    match error {
        SpatialRenderError::SpatialBackendUnavailable => FbResult::FbBackendUnavailable,
        _ => FbResult::FbInvalidArgument,
    }
}

fn spatial_backend_prepare_error_to_ffi(_error: SpatialBackendRenderError) -> FbResult {
    // Creation already proved the SDK and bound graph exist. Any preparation
    // rejection on this live session is therefore a backend-state failure,
    // including an internally inactive graph.
    FbResult::FbBackendError
}

fn frame_time_ns(frame: u64, sample_rate_hz: u32) -> u64 {
    let nanoseconds = (u128::from(frame) * 1_000_000_000_u128) / u128::from(sample_rate_hz);
    nanoseconds.min(u128::from(u64::MAX)) as u64
}

fn vector_to_ffi(value: EnuVector3) -> FbVec3 {
    FbVec3 {
        east_m: value.east_m,
        north_m: value.north_m,
        up_m: value.up_m,
    }
}

fn macro_event_role_from_ffi(value: u32) -> Option<EventRole> {
    match value {
        value if value == FbMacroEventRoleV2::FbMacroCinematicImpulseV2 as u32 => {
            Some(EventRole::CinematicImpulse)
        }
        value if value == FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32 => {
            Some(EventRole::StandardImpulse)
        }
        value if value == FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32 => {
            Some(EventRole::BallisticCrack)
        }
        value if value == FbMacroEventRoleV2::FbMacroBallisticBlastV2 as u32 => {
            Some(EventRole::BallisticBlast)
        }
        _ => None,
    }
}

fn macro_event_role_to_ffi(value: EventRole) -> u32 {
    match value {
        EventRole::CinematicImpulse => FbMacroEventRoleV2::FbMacroCinematicImpulseV2 as u32,
        EventRole::StandardImpulse => FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
        EventRole::BallisticCrack => FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32,
        EventRole::BallisticBlast => FbMacroEventRoleV2::FbMacroBallisticBlastV2 as u32,
    }
}

fn macro_asset_transport_from_ffi(value: u32) -> Option<MacroAssetTransport> {
    match value {
        value if value == FbMacroAssetTransportV2::FbMacroAssetSeekableV2 as u32 => {
            Some(MacroAssetTransport::Seekable)
        }
        value if value == FbMacroAssetTransportV2::FbMacroAssetPreGeneratedV2 as u32 => {
            Some(MacroAssetTransport::PreGenerated)
        }
        value if value == FbMacroAssetTransportV2::FbMacroAssetDeterministicGeneratorV2 as u32 => {
            Some(MacroAssetTransport::DeterministicGenerator)
        }
        value if value == FbMacroAssetTransportV2::FbMacroAssetNonSeekableLiveV2 as u32 => {
            Some(MacroAssetTransport::NonSeekableLive)
        }
        _ => None,
    }
}

fn macro_fallback_reason_to_ffi(value: IngressFallbackReason) -> u32 {
    match value {
        IngressFallbackReason::MissingActiveCell => {
            FbMacroFallbackReasonV2::FbMacroFallbackMissingActiveCellV2 as u32
        }
        IngressFallbackReason::StaleActiveCell => {
            FbMacroFallbackReasonV2::FbMacroFallbackStaleActiveCellV2 as u32
        }
        IngressFallbackReason::MissingPackageAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackMissingPackageAuthorityV2 as u32
        }
        IngressFallbackReason::StalePackageAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackStalePackageAuthorityV2 as u32
        }
        IngressFallbackReason::MissingProbeBakeAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackMissingProbeBakeAuthorityV2 as u32
        }
        IngressFallbackReason::StaleProbeBakeAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackStaleProbeBakeAuthorityV2 as u32
        }
        IngressFallbackReason::MissingEchoAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackMissingEchoAuthorityV2 as u32
        }
        IngressFallbackReason::StaleEchoAuthority => {
            FbMacroFallbackReasonV2::FbMacroFallbackStaleEchoAuthorityV2 as u32
        }
        IngressFallbackReason::InvalidLocalLeg => {
            FbMacroFallbackReasonV2::FbMacroFallbackInvalidLocalLegV2 as u32
        }
    }
}

fn macro_activation_to_ffi(activation: &LocalIngressActivation) -> FbMacroActivationV2 {
    let eligibility = activation.eligibility;
    let eligibility_bits = u32::from(eligibility.detailed_direct)
        * FB_MACRO_ELIGIBILITY_DETAILED_DIRECT_V2
        | u32::from(eligibility.statistical_ground) * FB_MACRO_ELIGIBILITY_STATISTICAL_GROUND_V2
        | u32::from(eligibility.baked_reflections) * FB_MACRO_ELIGIBILITY_BAKED_REFLECTIONS_V2
        | u32::from(eligibility.authored_echo) * FB_MACRO_ELIGIBILITY_AUTHORED_ECHO_V2
        | u32::from(eligibility.shared_diffuse) * FB_MACRO_ELIGIBILITY_SHARED_DIFFUSE_V2;
    let (render_authority, fallback_reason, shared_diffuse_fallback, world_generation) =
        match &activation.render_authority {
            IngressRenderAuthority::DetailedLocal { authority } => (
                FbMacroRenderAuthorityV2::FbMacroRenderAuthorityDetailedLocalV2 as u32,
                FbMacroFallbackReasonV2::FbMacroFallbackNoneV2 as u32,
                0,
                authority.world_generation,
            ),
            IngressRenderAuthority::MacroFallback {
                observed_authority,
                reason,
                shared_diffuse,
            } => (
                FbMacroRenderAuthorityV2::FbMacroRenderAuthorityFallbackV2 as u32,
                macro_fallback_reason_to_ffi(*reason),
                u32::from(*shared_diffuse),
                observed_authority
                    .as_ref()
                    .map_or(0, |authority| authority.world_generation),
            ),
        };
    FbMacroActivationV2 {
        active: 1,
        role: macro_event_role_to_ffi(activation.role),
        render_authority,
        fallback_reason,
        eligibility_bits,
        shared_diffuse_fallback,
        event_id: activation.event_id.0,
        atomic_group_id: activation.family.atomic_group_id(),
        asset_key: activation.asset_key,
        emission_frame: activation.emission_frame,
        program_seek_frame: activation.program_seek_frame,
        macro_arrival_frame: activation.macro_conditioning.arrival_frame(),
        fallback_ear_arrival_frame: activation.fallback_ear_arrival_frame().unwrap_or(0),
        tail_deadline_frame: activation.tail_deadline_frame,
        authority_world_generation: world_generation,
        macro_distance_gain: activation.macro_conditioning.distance_gain(),
        macro_atmosphere_gain_db: *activation.macro_conditioning.atmosphere_gain_db(),
        ingress_proxy_enu: vector_to_ffi(activation.local_leg.ingress_proxy_enu),
        remote_direction_enu: vector_to_ffi(activation.local_leg.remote_direction_enu),
        local_distance_m: activation.local_leg.distance_m,
        local_delay_frames: activation.local_leg.delay_frames,
        local_distance_gain: activation.local_leg.distance_gain,
        local_atmosphere_gain_db: activation.local_leg.atmosphere_gain_db,
        ..FbMacroActivationV2::default()
    }
}

fn pose_to_ffi(value: Pose) -> FbPose {
    FbPose {
        position: vector_to_ffi(value.position),
        forward: vector_to_ffi(value.forward),
        up: vector_to_ffi(value.up),
    }
}

fn default_pose() -> Pose {
    Pose {
        position: EnuVector3::default(),
        forward: EnuVector3::new(0.0, 1.0, 0.0),
        up: EnuVector3::new(0.0, 0.0, 1.0),
    }
}

/// Copies and validates one frozen V2 complete frame into engine-owned state.
///
/// # Safety
/// `frame` and every declared strided source prefix follow the V2 readable
/// memory contract for this call.
unsafe fn decode_control_update_v2(
    frame: *const FbControlFrameV2,
    source_count: usize,
) -> Result<SimulationUpdate, FbResult> {
    // Safety: forwarded from this helper's caller contract.
    let frame = unsafe { copy_control_frame_v2(frame) }?;
    // Safety: the copied outer record carries caller-provided source pointers
    // that remain readable for this call.
    let frame = unsafe { validate_control_frame_v2(&frame, source_count) }?;
    let listener_pose = pose_from_ffi(frame.listener_pose).ok_or(FbResult::FbInvalidArgument)?;
    let listener_velocity =
        vector_from_ffi(frame.listener_linear_velocity_mps).ok_or(FbResult::FbInvalidArgument)?;
    let mut update = default_simulation_update();
    update.listener = ListenerState {
        pose: listener_pose,
        linear_velocity_mps: listener_velocity,
    };
    for (source_index, source) in frame.source_updates[..source_count]
        .iter()
        .copied()
        .enumerate()
    {
        update.sources[source_index] = SourceMotion {
            active: source.active != 0,
            pose: pose_from_ffi(source.pose).ok_or(FbResult::FbInvalidArgument)?,
            linear_velocity_mps: vector_from_ffi(source.linear_velocity_mps)
                .ok_or(FbResult::FbInvalidArgument)?,
        };
    }
    Ok(update)
}

fn default_simulation_update() -> SimulationUpdate {
    SimulationUpdate {
        listener: ListenerState {
            pose: default_pose(),
            linear_velocity_mps: EnuVector3::default(),
        },
        sources: [SourceMotion::default(); MAX_ACTIVE_SOURCES],
    }
}

fn translate_update_between_cell_frames(
    update: &SimulationUpdate,
    from_offset: Option<EnuVector3>,
    to_offset: Option<EnuVector3>,
) -> SimulationUpdate {
    let from = from_offset.unwrap_or_default();
    let to = to_offset.unwrap_or_default();
    let delta = EnuVector3::new(
        from.east_m - to.east_m,
        from.north_m - to.north_m,
        from.up_m - to.up_m,
    );
    let mut translated = *update;
    translated.listener.pose.position.east_m += delta.east_m;
    translated.listener.pose.position.north_m += delta.north_m;
    translated.listener.pose.position.up_m += delta.up_m;
    for source in &mut translated.sources {
        source.pose.position.east_m += delta.east_m;
        source.pose.position.north_m += delta.north_m;
        source.pose.position.up_m += delta.up_m;
    }
    translated
}

fn advance_simulation(session: &SessionInner, control: &mut ControlState) -> FbResult {
    observe_latest_render_timing(session, control);
    advance_legacy_simulation_phases(
        &mut control.runner,
        &control.update,
        &mut control.update_sequence,
    )
}

fn advance_legacy_simulation_phases<R: SimulationRunner>(
    runner: &mut R,
    update: &SimulationUpdate,
    update_sequence: &mut u64,
) -> FbResult {
    runner.update_inputs(update);
    if runner.run_direct().is_err() {
        return FbResult::FbBackendError;
    }
    *update_sequence = update_sequence.wrapping_add(1);
    if update_sequence.is_multiple_of(4) && runner.run_pathing().is_err() {
        return FbResult::FbBackendError;
    }
    if update_sequence.is_multiple_of(12) && runner.run_reflections().is_err() {
        return FbResult::FbBackendError;
    }
    FbResult::FbOk
}

fn publish_legacy_control_frame(
    update: &mut SimulationUpdate,
    orientation_writer: &mut SnapshotWriter<ListenerOrientation>,
    active_sources_writer: &mut SnapshotWriter<[bool; MAX_ACTIVE_SOURCES]>,
    next_update: SimulationUpdate,
) {
    *update = next_update;
    orientation_writer.publish(ListenerOrientation {
        forward: next_update.listener.pose.forward,
        up: next_update.listener.pose.up,
    });
    active_sources_writer.publish(std::array::from_fn(|source_index| {
        next_update.sources[source_index].active
    }));
}

trait SpatialSimulationPhases {
    fn update_spatial_inputs(&mut self, update: &SimulationUpdate);
    fn spatial_direct_succeeded(&mut self) -> bool;
    fn latest_spatial_direct_sequence(&self) -> u64;
    fn spatial_pathing_succeeded(&mut self) -> bool;
    fn spatial_reflections_succeeded(&mut self) -> bool;
}

impl SpatialSimulationPhases for SteamAudioSpatialSimulationRunner {
    fn update_spatial_inputs(&mut self, update: &SimulationUpdate) {
        SimulationRunner::update_inputs(self, update);
    }

    fn spatial_direct_succeeded(&mut self) -> bool {
        SimulationRunner::run_direct(self).is_ok()
    }

    fn latest_spatial_direct_sequence(&self) -> u64 {
        self.latest_direct_sequence()
    }

    fn spatial_pathing_succeeded(&mut self) -> bool {
        SimulationRunner::run_pathing(self).is_ok()
    }

    fn spatial_reflections_succeeded(&mut self) -> bool {
        SimulationRunner::run_reflections(self).is_ok()
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn advance_spatial_simulation_phases<R: SpatialSimulationPhases>(
    runner: &mut R,
    update: &SimulationUpdate,
    update_sequence: &mut u64,
    propagation_writer: &mut SnapshotWriter<PropagationSnapshot>,
    block_start_frame: u64,
    sample_rate_hz: u32,
) -> FbResult {
    advance_spatial_simulation_phases_with_runtime_activity(
        runner,
        update,
        None,
        update_sequence,
        propagation_writer,
        block_start_frame,
        sample_rate_hz,
    )
}

fn advance_spatial_simulation_phases_with_runtime_activity<R: SpatialSimulationPhases>(
    runner: &mut R,
    update: &SimulationUpdate,
    runtime_activity: Option<[bool; MAX_ACTIVE_SOURCES]>,
    update_sequence: &mut u64,
    propagation_writer: &mut SnapshotWriter<PropagationSnapshot>,
    block_start_frame: u64,
    sample_rate_hz: u32,
) -> FbResult {
    runner.update_spatial_inputs(update);
    if !runner.spatial_direct_succeeded() {
        return FbResult::FbBackendError;
    }

    // Steam direct simulation has already published its backend-active set.
    // Runtime may additionally require wrapper-owned fallback programs; both
    // views carry this exact direct-generation token.
    let propagation_sequence = runner.latest_spatial_direct_sequence();
    let mut snapshot = propagation_snapshot_from_update(
        update,
        propagation_sequence,
        frame_time_ns(block_start_frame, sample_rate_hz),
    );
    if let Some(runtime_activity) = runtime_activity {
        for (source, active) in snapshot.sources.iter_mut().zip(runtime_activity) {
            source.active = active;
        }
    }
    propagation_writer.publish(snapshot);

    *update_sequence = update_sequence.wrapping_add(1);
    if update_sequence.is_multiple_of(4) && !runner.spatial_pathing_succeeded() {
        return FbResult::FbBackendError;
    }
    if update_sequence.is_multiple_of(12) && !runner.spatial_reflections_succeeded() {
        return FbResult::FbBackendError;
    }
    FbResult::FbOk
}

fn advance_spatial_simulation(
    session: &SessionInner,
    control: &mut SpatialControlState,
) -> FbResult {
    observe_spatial_callback_timings(control);
    synchronize_adopted_cell_authority(control);
    let block_start_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
    let runtime_activity = control.macro_bridge.enabled.then(|| {
        control
            .macro_bridge
            .overlay
            .overlay_runtime_activity(std::array::from_fn(|index| {
                control.update.sources[index].active
            }))
    });
    advance_spatial_simulation_phases_with_runtime_activity(
        &mut control.runner,
        &control.update,
        runtime_activity,
        &mut control.update_sequence,
        &mut control.propagation_writer,
        block_start_frame,
        session.sample_rate_hz,
    )
}

fn synchronize_adopted_cell_authority(control: &mut SpatialControlState) {
    let Some(authority) = control.pending_cell_authority.as_ref() else {
        return;
    };
    if matches!(
        control.runner.cell_stream_state(),
        SpatialCellStreamState::Publishing | SpatialCellStreamState::Prepared
    ) {
        return;
    }
    let authority = authority.clone();
    control
        .macro_ingress
        .publish_active_cell(authority.cell.clone(), Some(authority));
    control.active_echo_authority = control.pending_echo_authority.take();
    control.pending_cell_authority = None;
}

#[allow(clippy::too_many_arguments)]
fn commit_spatial_control_frame_phases<R: SpatialSimulationPhases>(
    lifecycle: &SpatialLifecycleState,
    runner: &mut R,
    update: &mut SimulationUpdate,
    next_update: SimulationUpdate,
    update_sequence: &mut u64,
    propagation_writer: &mut SnapshotWriter<PropagationSnapshot>,
    block_start_frame: u64,
    sample_rate_hz: u32,
    listener_published: &mut bool,
    source_published: &mut [bool; MAX_ACTIVE_SOURCES],
    source_count: usize,
    batched_frame_advances: &mut u64,
) -> FbResult {
    commit_spatial_control_frame_phases_with_runtime_activity(
        lifecycle,
        runner,
        update,
        next_update,
        None,
        update_sequence,
        propagation_writer,
        block_start_frame,
        sample_rate_hz,
        listener_published,
        source_published,
        source_count,
        batched_frame_advances,
    )
}

#[allow(clippy::too_many_arguments)]
fn commit_spatial_control_frame_phases_with_runtime_activity<R: SpatialSimulationPhases>(
    lifecycle: &SpatialLifecycleState,
    runner: &mut R,
    update: &mut SimulationUpdate,
    next_update: SimulationUpdate,
    runtime_activity: Option<[bool; MAX_ACTIVE_SOURCES]>,
    update_sequence: &mut u64,
    propagation_writer: &mut SnapshotWriter<PropagationSnapshot>,
    block_start_frame: u64,
    sample_rate_hz: u32,
    listener_published: &mut bool,
    source_published: &mut [bool; MAX_ACTIVE_SOURCES],
    source_count: usize,
    batched_frame_advances: &mut u64,
) -> FbResult {
    // This is the batch transaction boundary: invalid inputs never reach it;
    // admitted input increments the packed epoch exactly once before any
    // backend pass can partially commit.
    lifecycle.note_control_update();
    *update = next_update;
    let cadence_before = *update_sequence;
    let result = advance_spatial_simulation_phases_with_runtime_activity(
        runner,
        update,
        runtime_activity,
        update_sequence,
        propagation_writer,
        block_start_frame,
        sample_rate_hz,
    );
    if *update_sequence != cadence_before {
        *batched_frame_advances = batched_frame_advances.saturating_add(1);
    }
    if result == FbResult::FbOk {
        *listener_published = true;
        source_published[..source_count].fill(true);
    }
    result
}

fn prepare_spatial_session_for_realtime(
    session: &SessionInner,
    control: &mut SpatialControlState,
    render: &mut SpatialRenderState,
) -> Result<(), FbResult> {
    // This consolidated Steam seam forces current direct, path, and reflection
    // truth without advancing the ordinary cadence scheduler or governor
    // timing evidence. Preparation likewise does not drain callback timings.
    control
        .runner
        .prepare_simulation_for_realtime(&control.update)
        .map_err(|_| FbResult::FbBackendError)?;
    let direct_sequence = control.runner.latest_direct_sequence();
    let block_start_frame = session.spatial_block_start_frame.load(Ordering::Acquire);
    control
        .propagation_writer
        .publish(propagation_snapshot_from_update(
            &control.update,
            direct_sequence,
            frame_time_ns(block_start_frame, session.sample_rate_hz),
        ));
    render
        .graph
        .prepare_spatial_backend_for_realtime()
        .map_err(spatial_backend_prepare_error_to_ffi)
}

fn observe_spatial_callback_timings(control: &mut SpatialControlState) {
    let runner = &mut control.runner;
    let callback_timing_run = &mut control.callback_timing_run;
    let callback_timing_run_max_observation = &mut control.callback_timing_run_max_observation;
    control.timing_reader.drain(|elapsed_ns| {
        let observation = callback_timing_run.len();
        let is_new_maximum = callback_timing_run
            .max_ns()
            .is_none_or(|maximum| elapsed_ns > maximum);
        callback_timing_run.record(elapsed_ns);
        if is_new_maximum {
            *callback_timing_run_max_observation = Some(observation);
        }
        runner.observe_render_timing(elapsed_ns);
    });
}

fn observe_latest_render_timing(session: &SessionInner, control: &mut ControlState) {
    let elapsed = session.last_render_ns.swap(0, Ordering::AcqRel);
    if elapsed != 0 {
        control.runner.observe_render_timing(elapsed);
    }
}

fn scene_mesh(
    loaded: &fightbox_world::LoadedPackage,
) -> Result<fightbox_steam_audio::SceneMesh, FbResult> {
    let triangles = loaded
        .mesh
        .triangles
        .iter()
        .map(|triangle| {
            Ok([
                i32::try_from(triangle[0]).map_err(|_| FbResult::FbInvalidPackage)?,
                i32::try_from(triangle[1]).map_err(|_| FbResult::FbInvalidPackage)?,
                i32::try_from(triangle[2]).map_err(|_| FbResult::FbInvalidPackage)?,
            ])
        })
        .collect::<Result<Vec<_>, FbResult>>()?;
    let material_indices = loaded
        .mesh
        .material_ids
        .iter()
        .map(|index| i32::try_from(*index).map_err(|_| FbResult::FbInvalidPackage))
        .collect::<Result<Vec<_>, _>>()?;
    let materials = loaded
        .materials
        .iter()
        .map(|(_, material)| AcousticMaterial {
            absorption: material.absorption,
            scattering: material.scattering,
            transmission: material.transmission,
        })
        .collect();
    Ok(fightbox_steam_audio::SceneMesh {
        vertices_enu_m: loaded
            .mesh
            .vertices_enu_m
            .iter()
            .map(|vertex| {
                fightbox_steam_audio::EnuVector3::new(vertex.east_m, vertex.north_m, vertex.up_m)
            })
            .collect(),
        triangles,
        material_indices,
        materials,
    })
}

fn load_bake(path: &Path) -> Result<BakedProbeBatch, FbResult> {
    let bytes = std::fs::read(path.join("probe-batch.bin")).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FbResult::FbInvalidBake
        } else {
            FbResult::FbIoError
        }
    })?;
    let metadata_text = std::fs::read_to_string(path.join("probe-batch-metadata.json"))
        .map_err(|_| FbResult::FbInvalidBake)?;
    let wire: ProbeMetadataWire =
        serde_json::from_str(&metadata_text).map_err(|_| FbResult::FbInvalidBake)?;
    if wire.schema_version != PROBE_BATCH_METADATA_SCHEMA
        || wire.steam_audio_version != STEAM_AUDIO_VERSION
        || wire.upstream_commit != STEAM_AUDIO_UPSTREAM_COMMIT
    {
        return Err(FbResult::FbInvalidBake);
    }
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: wire.probe_count,
            path_data_size_bytes: wire.path_data_size_bytes,
            serialized_size_bytes: wire.serialized_size_bytes,
            content_sha256: wire.content_sha256,
            bake_progress_callback_count: wire.bake_progress_callback_count,
            final_bake_progress_millionths: wire.final_bake_progress_millionths,
        },
        bytes,
    };
    baked.validate().map_err(|_| FbResult::FbInvalidBake)?;
    Ok(baked)
}

fn verify_bake_identity(
    loaded: &fightbox_world::LoadedPackage,
    bake_path: &Path,
    baked: &BakedProbeBatch,
) -> Result<(), FbResult> {
    let bytes = std::fs::read(bake_path.join("city-bake-manifest.json"))
        .map_err(|_| FbResult::FbInvalidBake)?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| FbResult::FbInvalidBake)?;
    for (field, expected) in [
        (
            "mesh_content_sha256",
            loaded.manifest.mesh_content_sha256.as_str(),
        ),
        (
            "materials_content_sha256",
            loaded.manifest.materials_content_sha256.as_str(),
        ),
        ("probe_batch_sha256", baked.metadata.content_sha256.as_str()),
    ] {
        if value.get(field).and_then(serde_json::Value::as_str) != Some(expected) {
            return Err(FbResult::FbInvalidBake);
        }
    }
    Ok(())
}

fn cell_authority_seed(
    loaded: &fightbox_world::LoadedPackage,
    package_path: &Path,
    baked: &BakedProbeBatch,
) -> Result<CellAuthoritySeed, FbResult> {
    let (cell, metadata_city_offset_enu) = loaded.manifest.world.as_ref().map_or_else(
        || {
            (
                CellIdentity::new(
                    "legacy-package",
                    format!("legacy:{}", loaded.manifest.mesh_content_sha256),
                ),
                None,
            )
        },
        |world| {
            (
                CellIdentity::new(world.city.id.clone(), world.cell.id.clone()),
                Some(EnuVector3::new(
                    world.cell.local_to_city_enu_m[0] as f32,
                    world.cell.local_to_city_enu_m[1] as f32,
                    world.cell.local_to_city_enu_m[2] as f32,
                )),
            )
        },
    );
    let package_content_sha256 = fightbox_world::package_manifest_content_hash(package_path)
        .map_err(|_| FbResult::FbInvalidPackage)?;
    let echo_authority = fightbox_world::load_package_echo_authority(package_path, loaded)
        .map_err(|_| FbResult::FbInvalidPackage)?
        .map(Arc::new);
    Ok(CellAuthoritySeed {
        cell,
        metadata_city_offset_enu,
        package_content_sha256,
        probe_bake_content_sha256: baked.metadata.content_sha256.clone(),
        echo_authority,
    })
}

fn telemetry_json(
    telemetry: Option<QualityGovernorTelemetry>,
    ffi_render_buffers_bytes: u64,
) -> String {
    let Some(value) = telemetry else {
        return String::from(r#"{"available":false}"#);
    };
    let mut sources = String::new();
    for (index, source) in value.sources[..usize::from(value.source_count)]
        .iter()
        .enumerate()
    {
        if index != 0 {
            sources.push(',');
        }
        sources.push_str(&format!(
            concat!(
                r#"{{"source_index":{},"quality":"{}","predicted_audibility_db":{},"#,
                r#""physically_calibrated":{},"below_hearing_threshold":{},"#,
                r#""transport_advances":{}}}"#
            ),
            source.source_index,
            source_quality_name(source.quality),
            source.predicted_audibility_db,
            source.physically_calibrated,
            source.below_hearing_threshold,
            source.transport_advances,
        ));
    }
    let tracked_at_create_bytes = value
        .memory
        .tracked_at_create_bytes
        .saturating_add(ffi_render_buffers_bytes);
    let tracked_current_bytes = value
        .memory
        .tracked_current_bytes
        .saturating_add(ffi_render_buffers_bytes);
    let tracked_peak_bytes = value
        .memory
        .tracked_peak_bytes
        .saturating_add(ffi_render_buffers_bytes);
    format!(
        concat!(
            r#"{{"available":true,"quality_tier":"{}","tier_source_cap":{},"sequence":{},"ladder_position":{},"reason":"{}","#,
            r#""timing_ns":{{"p50":{},"p95":{},"p99":{},"p99_9":{},"deadline_misses":{}}},"#,
            r#""simulation_lateness_ns":[{},{},{}],"delivered_quality":{{"#,
            r#""reflections":{{"level":"{}","rays":{},"diffuse_samples":{},"bounces":{},"#,
            r#""ir_duration_s":{},"cadence_divisor":{}}},"pathing":"{}","#,
            r#""ambisonic_order":{},"reverb":"{}","reflection_output_gain":{},"sources":[{}]}},"#,
            r#""memory":{{"scope":"configuration_known_payloads_and_capacities_not_process_total","#,
            r#""tracked_at_create_bytes":{},"tracked_current_bytes":{},"tracked_peak_bytes":{},"categories":{{"#,
            r#""snapshot_ring_payload_bytes":{},"reflection_ir_payload_capacity_bytes":{},"#,
            r#""audio_buffer_payload_bytes":{},"engine_render_scratch_bytes":{},"#,
            r#""ffi_render_buffers_bytes":{},"propagation_delay_line_bytes":{},"retained_bake_bytes":{}}},"#,
            r#""untracked":[{{"category":"steam_audio_sdk_internal","status":"{}","#,
            r#""includes":"allocator_overhead,effect_workspaces,hrtf,scene,probe_and_simulator_storage"}}]}}}}"#
        ),
        quality_tier_name(value.quality_tier),
        value.tier_source_cap,
        value.sequence,
        value.ladder_position,
        transition_reason_name(value.reason),
        value.p50_ns,
        value.p95_ns,
        value.p99_ns,
        value.p99_9_ns,
        value.callback_deadline_misses,
        value.simulation_lateness_ns[0],
        value.simulation_lateness_ns[1],
        value.simulation_lateness_ns[2],
        reflection_quality_name(value.reflections.level),
        value.reflections.rays,
        value.reflections.diffuse_samples,
        value.reflections.bounces,
        value.reflections.ir_duration_s,
        value.reflections.cadence_divisor,
        path_quality_name(value.pathing),
        value.ambisonic_order,
        reverb_name(value.reverb),
        value.reflection_output_gain,
        sources,
        tracked_at_create_bytes,
        tracked_current_bytes,
        tracked_peak_bytes,
        value.memory.snapshot_ring_payload_bytes,
        value.memory.reflection_ir_payload_capacity_bytes,
        value.memory.audio_buffer_payload_bytes,
        value.memory.render_scratch_bytes,
        ffi_render_buffers_bytes,
        value.memory.propagation_delay_line_bytes,
        value.memory.retained_bake_bytes,
        memory_tracking_status_name(value.memory.steam_audio_sdk_internal),
    )
}

#[allow(clippy::too_many_arguments)]
fn spatial_telemetry_json(
    telemetry: Option<QualityGovernorTelemetry>,
    memory: SpatialBindingMemoryTelemetry,
    callback_timing_run: &RunTimingHistogram,
    callback_timing_run_max_observation: Option<u64>,
    dropped_timing_observations: u64,
    preparation: SpatialPreparationTelemetry,
    preparation_status: &str,
    control_schedule: SpatialControlScheduleTelemetry,
    macro_ingress: &MacroIngressTelemetry,
    macro_atmosphere: FrozenAtmosphere,
    macro_atmosphere_locked: bool,
    active_echo_authority: Option<&fightbox_world::PackageEchoAuthority>,
    echo_authority_adoption_pending: bool,
    macro_echo: MacroEchoBridgeTelemetry,
    macro_echo_pinned_role_count: usize,
) -> String {
    let Some(telemetry) = telemetry else {
        return String::from(r#"{"available":false}"#);
    };

    // Keep the frozen V1 serializer byte-for-byte untouched. V2 starts from
    // the same governor payload, then adds only its route-specific accounting
    // and timing-publication evidence.
    let mut root: serde_json::Value = serde_json::from_str(&telemetry_json(Some(telemetry), 0))
        .expect("the internal telemetry serializer emits valid JSON");
    let external_payload_bytes = memory.external_payload_bytes();
    let ffi_bank_payload_bytes = memory.ffi_bank_payload_bytes();
    let render_binding_payload_bytes = memory
        .neutral_graph
        .total_tracked_payload_bytes
        .saturating_add(external_payload_bytes);

    let memory_object = root
        .get_mut("memory")
        .and_then(serde_json::Value::as_object_mut)
        .expect("available telemetry has a memory object");
    memory_object.insert(
        "tracked_at_create_bytes".into(),
        telemetry
            .memory
            .tracked_at_create_bytes
            .saturating_add(external_payload_bytes)
            .into(),
    );
    memory_object.insert(
        "tracked_current_bytes".into(),
        telemetry
            .memory
            .tracked_current_bytes
            .saturating_add(external_payload_bytes)
            .into(),
    );
    memory_object.insert(
        "tracked_peak_bytes".into(),
        telemetry
            .memory
            .tracked_peak_bytes
            .saturating_add(external_payload_bytes)
            .into(),
    );
    memory_object.insert(
        "v2_render_binding_payload_bytes".into(),
        render_binding_payload_bytes.into(),
    );
    memory_object.insert(
        "v2_external_payload_bytes".into(),
        external_payload_bytes.into(),
    );

    let categories = memory_object
        .get_mut("categories")
        .and_then(serde_json::Value::as_object_mut)
        .expect("available telemetry has memory categories");
    categories.insert(
        "ffi_render_buffers_bytes".into(),
        ffi_bank_payload_bytes.into(),
    );
    categories.insert(
        "neutral_spatial_graph".into(),
        serde_json::json!({
            "included_in_governor_totals": true,
            "source_capacity": memory.neutral_graph.source_capacity,
            "configured_source_count": memory.neutral_graph.configured_source_count,
            "configured_stereo_source_count": memory.neutral_graph.configured_stereo_source_count,
            "stereo_indirect_suppressed_source_count": memory
                .neutral_graph
                .stereo_indirect_suppressed_source_count,
            "program_delay_audio_history_payload_bytes": memory
                .neutral_graph
                .program_delay_audio_history_payload_bytes,
            "program_delay_geometry_history_payload_bytes": memory
                .neutral_graph
                .program_delay_geometry_history_payload_bytes,
            "additional_program_channel_payload_bytes": memory
                .neutral_graph
                .additional_program_channel_payload_bytes,
            "steam_audio_buffer_payload_bytes": memory
                .neutral_graph
                .steam_audio_buffer_payload_bytes,
            "delayed_program_scratch_payload_bytes": memory
                .neutral_graph
                .delayed_program_scratch_payload_bytes,
            "outer_vec_payload_bytes": memory.neutral_graph.outer_vec_payload_bytes,
            "rust_scratch_payload_bytes": memory.neutral_graph.rust_scratch_payload_bytes,
            "total_tracked_payload_bytes": memory.neutral_graph.total_tracked_payload_bytes,
            "steam_audio_sdk_internal": memory_tracking_status_name(
                memory.neutral_graph.steam_audio_sdk_internal,
            ),
        }),
    );
    categories.insert(
        "runtime_graph".into(),
        serde_json::json!({
            "source_node_capacity": memory.runtime_graph.source_node_capacity,
            "propagation_delay_payload_bytes": memory
                .runtime_graph
                .propagation_delay_payload_bytes,
            "block_scratch_payload_bytes": memory.runtime_graph.block_scratch_payload_bytes,
            "spatial_scratch_payload_bytes": memory.runtime_graph.spatial_scratch_payload_bytes,
            "total_payload_bytes": memory.runtime_graph.total_payload_bytes,
        }),
    );
    categories.insert(
        "ffi_spatial_banks".into(),
        serde_json::json!({
            "presentation_plane_capacity": MAX_SPATIAL_PRESENTATION_FEEDS,
            "environmental_plane_capacity": MAX_SPATIAL_ENVIRONMENT_PLANES,
            "presentation_bank_payload_bytes": memory.ffi_presentation_bank_payload_bytes,
            "environmental_bank_payload_bytes": memory.ffi_environmental_bank_payload_bytes,
            "total_payload_bytes": ffi_bank_payload_bytes,
        }),
    );
    categories.insert(
        "propagation_snapshot_publication".into(),
        serde_json::json!({
            "included_in_v2_external_payload_bytes": true,
            "shared_payload_bytes": memory.propagation_snapshot_publication_payload_bytes,
            "slot_count": 3,
        }),
    );
    categories.insert(
        "callback_timing_publication".into(),
        serde_json::json!({
            "included_in_v2_external_payload_bytes": true,
            "shared_payload_bytes": memory.callback_timing_publication_payload_bytes,
            "record_capacity": 4096,
        }),
    );
    categories.insert(
        "macro_production_bridge".into(),
        serde_json::json!({
            "included_in_v2_external_payload_bytes": true,
            "route_publication_payload_bytes": memory.macro_route_publication_payload_bytes,
            "command_mailbox_payload_bytes": memory.macro_command_mailbox_payload_bytes,
            "acknowledgement_mailbox_payload_bytes": memory
                .macro_acknowledgement_mailbox_payload_bytes,
            "render_graph_payload_bytes": memory.macro_render_graph_payload_bytes,
            "total_payload_bytes": memory
                .macro_route_publication_payload_bytes
                .saturating_add(memory.macro_command_mailbox_payload_bytes)
                .saturating_add(memory.macro_acknowledgement_mailbox_payload_bytes)
                .saturating_add(memory.macro_render_graph_payload_bytes),
        }),
    );

    let untracked = memory_object
        .get_mut("untracked")
        .and_then(serde_json::Value::as_array_mut)
        .expect("available telemetry has an untracked-memory array");
    untracked.push(serde_json::json!({
        "category": "v2_publication_and_runtime_overhead",
        "status": "untracked",
        "includes": "arc_control_blocks,allocator_metadata,struct_padding,reader_private_state,runtime_output_safety_publication",
    }));

    let timing = root
        .get_mut("timing_ns")
        .and_then(serde_json::Value::as_object_mut)
        .expect("available telemetry has a timing object");
    timing.insert(
        "callback_local_run".into(),
        serde_json::json!({
            "scope": "ready_barrier_through_output_scatter",
            "observations": callback_timing_run.len(),
            "coverage_complete": dropped_timing_observations == 0,
            "min": callback_timing_run.min_ns(),
            "p50": callback_timing_run.percentile_ns(50.0),
            "p95": callback_timing_run.percentile_ns(95.0),
            "p99": callback_timing_run.percentile_ns(99.0),
            "p99_9": callback_timing_run.percentile_ns(99.9),
            "max": callback_timing_run.max_ns(),
            "max_observation_index": callback_timing_run_max_observation,
        }),
    );
    timing.insert("observations".into(), callback_timing_run.len().into());
    timing.insert(
        "dropped_observations".into(),
        dropped_timing_observations.into(),
    );
    root.as_object_mut()
        .expect("available telemetry is a JSON object")
        .insert(
            "preparation".into(),
            serde_json::json!({
                "status": preparation_status,
                "attempts": preparation.attempts,
                "successes": preparation.successes,
                "failures": preparation.failures,
                "latest_duration_ns": preparation.latest_duration_ns,
            }),
        );
    root.as_object_mut()
        .expect("available telemetry is a JSON object")
        .insert(
            "control_schedule".into(),
            serde_json::json!({
                "owner": "ffi_caller_control_thread",
                "execution": "synchronous",
                "cadence_basis": "successful_direct_control_advance",
                "cadence_advances": control_schedule.cadence_advances,
                "batched_frame_advances": control_schedule.batched_frame_advances,
                "granular_listener_advances": control_schedule.granular_listener_advances,
                "granular_source_advances": control_schedule.granular_source_advances,
                "pathing_every_n_advances": 4,
                "base_reflections_every_n_advances": 12,
                "worker_busy_feedback": "not_applicable",
                "interval_lateness_policy": "diagnostic_only",
                "pass_overrun_policy": "actionable",
            }),
        );
    root.as_object_mut()
        .expect("available telemetry is a JSON object")
        .insert(
            "echo_authority".into(),
            serde_json::json!({
                "loaded": active_echo_authority.is_some(),
                "adoption_pending": echo_authority_adoption_pending,
                "content_sha256": active_echo_authority
                    .map(|authority| authority.content_sha256.as_str()),
                "cell_id": active_echo_authority.map(|authority| authority.cell_id.as_str()),
                "serialized_size_bytes": active_echo_authority
                    .map(|authority| authority.serialized_size_bytes),
                "resident_size_bytes": active_echo_authority
                    .map(|authority| authority.resident_size_bytes),
                "anchor_count": active_echo_authority
                    .map(|authority| authority.table.anchors().len()),
                "listener_node_count": active_echo_authority
                    .map(|authority| authority.table.listener_nodes().len()),
                "tile_count": active_echo_authority
                    .map(|authority| authority.table.tiles().len()),
            }),
        );
    root.as_object_mut()
        .expect("available telemetry is a JSON object")
        .insert(
            "macro_echo_binding".into(),
            serde_json::json!({
                "committed_queries": macro_echo.committed_queries,
                "committed_plans": macro_echo.committed_plans,
                "committed_silent_plans": macro_echo.committed_silent_plans,
                "committed_taps": macro_echo.committed_taps,
                "pinned_role_count": macro_echo_pinned_role_count,
            }),
        );
    root.as_object_mut()
        .expect("available telemetry is a JSON object")
        .insert(
            "macro_ingress".into(),
            serde_json::json!({
                "selected_cell": {
                    "city": macro_ingress.selected_cell.city,
                    "cell": macro_ingress.selected_cell.cell,
                },
                "published_cell": macro_ingress.published_cell.as_ref().map(|cell| {
                    serde_json::json!({"city": cell.city, "cell": cell.cell})
                }),
                "queued_event_count": macro_ingress.queued_event_count,
                "detailed_activation_count": macro_ingress.detailed_activation_count,
                "fallback_activation_count": macro_ingress.fallback_activation_count,
                "last_fallback_event": macro_ingress.last_fallback_event.map(|event| event.0),
                "last_fallback_reason": macro_ingress
                    .last_fallback_reason
                    .map(ingress_fallback_reason_name),
                "active_tail_events": macro_ingress
                    .active_tail_events
                    .map(|event| event.map(|event| event.0)),
                "atmosphere": {
                    "locked": macro_atmosphere_locked,
                    "provenance": macro_atmosphere.provenance().stable_label(),
                    "temperature_c": macro_atmosphere.observation().temperature_c,
                    "relative_humidity_percent": macro_atmosphere
                        .observation()
                        .relative_humidity_percent,
                    "pressure_kpa": macro_atmosphere.observation().pressure_kpa,
                    "absorption_db_per_meter": macro_atmosphere.absorption_db_per_meter(),
                },
            }),
        );
    serde_json::to_string(&root).expect("JSON value serialization cannot fail")
}

fn ingress_fallback_reason_name(value: fightbox_runtime::IngressFallbackReason) -> &'static str {
    use fightbox_runtime::IngressFallbackReason;
    match value {
        IngressFallbackReason::MissingActiveCell => "missing_active_cell",
        IngressFallbackReason::StaleActiveCell => "stale_active_cell",
        IngressFallbackReason::MissingPackageAuthority => "missing_package_authority",
        IngressFallbackReason::StalePackageAuthority => "stale_package_authority",
        IngressFallbackReason::MissingProbeBakeAuthority => "missing_probe_bake_authority",
        IngressFallbackReason::StaleProbeBakeAuthority => "stale_probe_bake_authority",
        IngressFallbackReason::MissingEchoAuthority => "missing_echo_authority",
        IngressFallbackReason::StaleEchoAuthority => "stale_echo_authority",
        IngressFallbackReason::InvalidLocalLeg => "invalid_local_leg",
    }
}

fn quality_tier_from_ffi(value: u32) -> Option<QualityTier> {
    match value {
        value if value == FbQualityTier::FbQualityDesktop as u32 => Some(QualityTier::Desktop),
        value if value == FbQualityTier::FbQualityMobile as u32 => Some(QualityTier::Mobile),
        _ => None,
    }
}

fn quality_tier_name(value: QualityTier) -> &'static str {
    match value {
        QualityTier::Desktop => "desktop",
        QualityTier::Mobile => "mobile",
    }
}

fn memory_tracking_status_name(value: MemoryTrackingStatus) -> &'static str {
    match value {
        MemoryTrackingStatus::Tracked => "tracked",
        MemoryTrackingStatus::Untracked => "untracked",
    }
}

fn transition_reason_name(value: GovernorTransitionReason) -> &'static str {
    match value {
        GovernorTransitionReason::Initial => "initial",
        GovernorTransitionReason::RenderP99OverBudget => "render_p99_over_budget",
        GovernorTransitionReason::RenderP999OverCeiling => "render_p99_9_over_ceiling",
        GovernorTransitionReason::RenderDeadlineMiss => "render_deadline_miss",
        GovernorTransitionReason::RenderDemotionIneffective => "render_demotion_ineffective",
        GovernorTransitionReason::RenderDemotionLocked => "render_demotion_locked",
        GovernorTransitionReason::SimulationLate => "simulation_late",
        GovernorTransitionReason::SustainedHeadroom => "sustained_headroom",
        GovernorTransitionReason::AtMinimumQuality => "at_minimum_quality",
        GovernorTransitionReason::AtFullQuality => "at_full_quality",
    }
}

fn reflection_quality_name(value: ReflectionQualityLevel) -> &'static str {
    match value {
        ReflectionQualityLevel::Full => "full",
        ReflectionQualityLevel::Reduced => "reduced",
        ReflectionQualityLevel::Intermediate => "intermediate",
        ReflectionQualityLevel::Minimum => "minimum",
    }
}

fn path_quality_name(value: PathQualityLevel) -> &'static str {
    match value {
        PathQualityLevel::Full => "full",
        PathQualityLevel::NoValidation => "no_validation",
        PathQualityLevel::PrimaryOnly => "primary_only",
    }
}

fn reverb_name(value: ReverbStrategy) -> &'static str {
    match value {
        ReverbStrategy::SdkMixerConvolution => "sdk_mixer_convolution",
        ReverbStrategy::Hybrid => "hybrid",
        ReverbStrategy::Baked => "baked",
        ReverbStrategy::ListenerCentric => "listener_centric",
        ReverbStrategy::ShortIrLowerOrder => "short_ir_lower_order",
    }
}

fn source_quality_name(value: SourceQualityLevel) -> &'static str {
    match value {
        SourceQualityLevel::Full => "full",
        SourceQualityLevel::DirectOnly => "direct_only",
        SourceQualityLevel::Virtualized => "virtualized",
    }
}

fn vector_from_ffi(value: FbVec3) -> Option<EnuVector3> {
    let value = EnuVector3::new(value.east_m, value.north_m, value.up_m);
    value.is_finite().then_some(value)
}

fn pose_from_ffi(value: FbPose) -> Option<Pose> {
    let pose = Pose {
        position: vector_from_ffi(value.position)?,
        forward: vector_from_ffi(value.forward)?,
        up: vector_from_ffi(value.up)?,
    };
    let forward_length = length_squared(pose.forward);
    let up_length = length_squared(pose.up);
    let cross_length = length_squared(cross(pose.forward, pose.up));
    (forward_length > 1.0e-8 && up_length > 1.0e-8 && cross_length > 1.0e-8).then_some(pose)
}

fn length_squared(value: EnuVector3) -> f32 {
    value.east_m * value.east_m + value.north_m * value.north_m + value.up_m * value.up_m
}

fn cross(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.north_m * right.up_m - left.up_m * right.north_m,
        left.up_m * right.east_m - left.east_m * right.up_m,
        left.east_m * right.north_m - left.north_m * right.east_m,
    )
}

fn ffi_boundary(operation: impl FnOnce() -> FbResult) -> FbResult {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(FbResult::FbPanic)
}

unsafe fn path_from_c(pointer: *const c_char) -> Result<PathBuf, FbResult> {
    // Safety: the caller guarantees a readable NUL-terminated C string.
    let bytes = unsafe { CStr::from_ptr(pointer) };
    let text = bytes.to_str().map_err(|_| FbResult::FbInvalidArgument)?;
    if text.is_empty() {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(PathBuf::from(text))
}

unsafe fn session_ref<'a>(session: *mut FbSession) -> Option<&'a SessionInner> {
    if !valid_session_ptr(session) {
        return None;
    }
    // Safety: caller owns a live handle for the duration of the operation.
    Some(unsafe { &*session.cast::<SessionInner>() })
}

fn valid_session_ptr(pointer: *mut FbSession) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<SessionInner>())
}

fn valid_const_ptr<T>(pointer: *const T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<T>())
}

fn valid_mut_ptr<T>(pointer: *mut T) -> bool {
    !pointer.is_null() && pointer.addr().is_multiple_of(align_of::<T>())
}

/// Validates the universal `{abi_version, struct_size}` prefix of a writable
/// current V3 output before writing its full current layout.
///
/// # Safety
/// `pointer` must be readable for the universal two-u32 header when non-null.
unsafe fn validate_v3_output<T>(pointer: *mut T) -> Result<(), FbResult> {
    if !valid_mut_ptr(pointer) {
        return Err(FbResult::FbInvalidArgument);
    }
    let header = pointer.cast::<u32>();
    // Safety: forwarded from this helper's caller contract.
    let abi_version = unsafe { header.read() };
    // Safety: the universal header contains a second readable u32.
    let struct_size = unsafe { header.add(1).read() };
    if abi_version != FB_ABI_VERSION_V3
        || usize::try_from(struct_size)
            .ok()
            .is_none_or(|size| size < size_of::<T>())
    {
        return Err(FbResult::FbInvalidArgument);
    }
    Ok(())
}

fn valid_slice_ptr<T>(pointer: *const T, length: usize) -> bool {
    length == 0 || valid_const_ptr(pointer)
}

fn valid_mut_slice_ptr<T>(pointer: *mut T, length: usize) -> bool {
    length == 0 || valid_mut_ptr(pointer)
}

fn ranges_overlap(left: *const u8, left_len: usize, right: *const u8, right_len: usize) -> bool {
    let left_start = left.addr();
    let right_start = right.addr();
    let Some(left_end) = left_start.checked_add(left_len) else {
        return true;
    };
    let Some(right_end) = right_start.checked_add(right_len) else {
        return true;
    };
    left_start < right_end && right_start < left_end
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_runtime::MAX_TIMING_RECORDS;
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::{Arc, Barrier};

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    struct CountingAllocator;

    // Safety: every operation delegates to the process System allocator with
    // the original GlobalAlloc contract unchanged.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // Safety: forwarded unchanged under GlobalAlloc's contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // Safety: the pointer/layout came from the delegated allocator.
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // Safety: forwarded unchanged under GlobalAlloc's contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // Safety: forwarded unchanged under GlobalAlloc's contract.
            unsafe { System.realloc(pointer, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

    fn count_allocations<T>(operation: impl FnOnce() -> T) -> (usize, T) {
        ALLOCATION_COUNT.with(|count| count.set(0));
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
        let value = operation();
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        (ALLOCATION_COUNT.with(Cell::get), value)
    }

    struct FailingSpatialBackend;

    impl fightbox_runtime::backend::SpatialBackendRenderGraph for FailingSpatialBackend {
        fn prepare_for_realtime(
            &mut self,
        ) -> Result<(), fightbox_runtime::backend::SpatialBackendRenderError> {
            Ok(())
        }

        fn render_spatial_block(
            &mut self,
            _block: fightbox_runtime::backend::SpatialPropagationRenderBlock<'_>,
        ) -> Result<(), fightbox_runtime::backend::SpatialBackendRenderError> {
            Err(fightbox_runtime::backend::SpatialBackendRenderError::InactiveGraph)
        }
    }

    struct PreparationProbeBackend {
        failures_remaining: Arc<AtomicU64>,
    }

    impl fightbox_runtime::backend::SpatialBackendRenderGraph for PreparationProbeBackend {
        fn prepare_for_realtime(
            &mut self,
        ) -> Result<(), fightbox_runtime::backend::SpatialBackendRenderError> {
            let mut remaining = self.failures_remaining.load(Ordering::Acquire);
            while remaining != 0 {
                match self.failures_remaining.compare_exchange(
                    remaining,
                    remaining - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return Err(
                            fightbox_runtime::backend::SpatialBackendRenderError::InactiveGraph,
                        );
                    }
                    Err(actual) => remaining = actual,
                }
            }
            Ok(())
        }

        fn render_spatial_block(
            &mut self,
            _block: fightbox_runtime::backend::SpatialPropagationRenderBlock<'_>,
        ) -> Result<(), fightbox_runtime::backend::SpatialBackendRenderError> {
            Err(fightbox_runtime::backend::SpatialBackendRenderError::InactiveGraph)
        }
    }

    #[derive(Default)]
    struct DirectThenPathFailure {
        observed_active: bool,
        direct_calls: usize,
        direct_sequence: u64,
        pathing_calls: usize,
        reflection_calls: usize,
    }

    impl SpatialSimulationPhases for DirectThenPathFailure {
        fn update_spatial_inputs(&mut self, update: &SimulationUpdate) {
            self.observed_active = update.sources[0].active;
        }

        fn spatial_direct_succeeded(&mut self) -> bool {
            self.direct_calls += 1;
            self.direct_sequence = self.direct_sequence.wrapping_add(1);
            true
        }

        fn latest_spatial_direct_sequence(&self) -> u64 {
            self.direct_sequence
        }

        fn spatial_pathing_succeeded(&mut self) -> bool {
            self.pathing_calls += 1;
            false
        }

        fn spatial_reflections_succeeded(&mut self) -> bool {
            self.reflection_calls += 1;
            true
        }
    }

    #[derive(Default)]
    struct DirectThenReflectionFailure {
        observed_active: bool,
        direct_calls: usize,
        direct_sequence: u64,
        pathing_calls: usize,
        reflection_calls: usize,
    }

    impl SpatialSimulationPhases for DirectThenReflectionFailure {
        fn update_spatial_inputs(&mut self, update: &SimulationUpdate) {
            self.observed_active = update.sources[0].active;
        }

        fn spatial_direct_succeeded(&mut self) -> bool {
            self.direct_calls += 1;
            self.direct_sequence = self.direct_sequence.wrapping_add(1);
            true
        }

        fn latest_spatial_direct_sequence(&self) -> u64 {
            self.direct_sequence
        }

        fn spatial_pathing_succeeded(&mut self) -> bool {
            self.pathing_calls += 1;
            true
        }

        fn spatial_reflections_succeeded(&mut self) -> bool {
            self.reflection_calls += 1;
            false
        }
    }

    struct DirectFailureOnce {
        observed_active: bool,
        direct_calls: usize,
        direct_sequence: u64,
        failures_remaining: usize,
    }

    impl SpatialSimulationPhases for DirectFailureOnce {
        fn update_spatial_inputs(&mut self, update: &SimulationUpdate) {
            self.observed_active = update.sources[0].active;
        }

        fn spatial_direct_succeeded(&mut self) -> bool {
            self.direct_calls += 1;
            if self.failures_remaining != 0 {
                self.failures_remaining -= 1;
                return false;
            }
            self.direct_sequence = self.direct_sequence.wrapping_add(1);
            true
        }

        fn latest_spatial_direct_sequence(&self) -> u64 {
            self.direct_sequence
        }

        fn spatial_pathing_succeeded(&mut self) -> bool {
            panic!("pathing is not due during the direct-failure retry proof")
        }

        fn spatial_reflections_succeeded(&mut self) -> bool {
            panic!("reflections are not due during the direct-failure retry proof")
        }
    }

    #[derive(Default)]
    struct LegacyDirectFailure {
        observed_active: bool,
        direct_calls: usize,
    }

    impl SimulationRunner for LegacyDirectFailure {
        fn update_inputs(&mut self, update: &SimulationUpdate) {
            self.observed_active = update.sources[0].active;
        }

        fn run_direct(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
            self.direct_calls += 1;
            Err(fightbox_runtime::backend::SimulationError::KernelFailure)
        }

        fn run_pathing(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
            panic!("pathing must not run after direct failure")
        }

        fn run_reflections(&mut self) -> Result<(), fightbox_runtime::backend::SimulationError> {
            panic!("reflections must not run after direct failure")
        }
    }

    #[repr(C, align(8))]
    struct ShortV2Header {
        abi_version: u32,
        struct_size: u32,
    }

    #[test]
    fn cell_frame_translation_preserves_city_pose_and_velocity() {
        let mut update = default_simulation_update();
        update.listener.pose.position = EnuVector3::new(7.125, -3.25, 1.5);
        update.listener.linear_velocity_mps = EnuVector3::new(1.0, 2.0, 3.0);
        update.sources[0].pose.position = EnuVector3::new(-4.5, 8.75, 2.0);
        update.sources[0].linear_velocity_mps = EnuVector3::new(-1.0, 4.0, 0.5);
        let from = EnuVector3::new(1_000.0, 2_000.0, 10.0);
        let to = EnuVector3::new(1_500.0, 1_250.0, -5.0);
        let translated = translate_update_between_cell_frames(&update, Some(from), Some(to));
        assert_eq!(
            translated.listener.pose.position,
            EnuVector3::new(-492.875, 746.75, 16.5)
        );
        assert_eq!(
            translated.sources[0].pose.position,
            EnuVector3::new(-504.5, 758.75, 17.0)
        );
        assert_eq!(
            translated.listener.linear_velocity_mps,
            update.listener.linear_velocity_mps
        );
        assert_eq!(
            translated.sources[0].linear_velocity_mps,
            update.sources[0].linear_velocity_mps
        );
    }

    #[test]
    fn prepared_cell_reservation_is_single_owner_and_drop_releases_it() {
        let state = Arc::new(AtomicBool::new(false));
        let reservation = PreparedCellReservation::acquire(&state).expect("first reservation");
        assert_eq!(
            PreparedCellReservation::acquire(&state).err(),
            Some(FbResult::FbInvalidState)
        );
        drop(reservation);
        let retry = PreparedCellReservation::acquire(&state).expect("released reservation");
        assert!(state.load(Ordering::Acquire));
        drop(retry);
        assert!(!state.load(Ordering::Acquire));
    }

    #[test]
    fn legacy_v1_c_layout_and_discriminants_are_frozen() {
        assert_eq!(size_of::<FbResult>(), 4);
        assert_eq!(FbResult::FbOk as u32, 0);
        assert_eq!(FbResult::FbInvalidArgument as u32, 1);
        assert_eq!(FbResult::FbInvalidState as u32, 2);
        assert_eq!(FbResult::FbIoError as u32, 3);
        assert_eq!(FbResult::FbInvalidPackage as u32, 4);
        assert_eq!(FbResult::FbInvalidBake as u32, 5);
        assert_eq!(FbResult::FbBackendUnavailable as u32, 6);
        assert_eq!(FbResult::FbBackendError as u32, 7);
        assert_eq!(FbResult::FbBufferTooSmall as u32, 8);
        assert_eq!(FbResult::FbPanic as u32, 9);

        assert_eq!(size_of::<FbQualityTier>(), 4);
        assert_eq!(FbQualityTier::FbQualityDesktop as u32, 0);
        assert_eq!(FbQualityTier::FbQualityMobile as u32, 1);

        assert_eq!(size_of::<FbVec3>(), 12);
        assert_eq!(align_of::<FbVec3>(), 4);
        assert_eq!(std::mem::offset_of!(FbVec3, east_m), 0);
        assert_eq!(std::mem::offset_of!(FbVec3, north_m), 4);
        assert_eq!(std::mem::offset_of!(FbVec3, up_m), 8);

        assert_eq!(size_of::<FbPose>(), 36);
        assert_eq!(align_of::<FbPose>(), 4);
        assert_eq!(std::mem::offset_of!(FbPose, position), 0);
        assert_eq!(std::mem::offset_of!(FbPose, forward), 12);
        assert_eq!(std::mem::offset_of!(FbPose, up), 24);

        assert_eq!(size_of::<FbSessionConfig>(), 20);
        assert_eq!(align_of::<FbSessionConfig>(), 4);
        assert_eq!(std::mem::offset_of!(FbSessionConfig, sample_rate_hz), 0);
        assert_eq!(std::mem::offset_of!(FbSessionConfig, block_size_frames), 4);
        assert_eq!(std::mem::offset_of!(FbSessionConfig, source_count), 8);
        assert_eq!(
            std::mem::offset_of!(FbSessionConfig, default_source_level_db),
            12
        );
        assert_eq!(std::mem::offset_of!(FbSessionConfig, quality_tier), 16);

        assert_eq!(size_of::<FbSourceUpdate>(), 52);
        assert_eq!(align_of::<FbSourceUpdate>(), 4);
        assert_eq!(std::mem::offset_of!(FbSourceUpdate, active), 0);
        assert_eq!(std::mem::offset_of!(FbSourceUpdate, pose), 4);
        assert_eq!(
            std::mem::offset_of!(FbSourceUpdate, linear_velocity_mps),
            40
        );
    }

    #[test]
    fn legacy_v1_exported_function_signatures_are_frozen() {
        let _: unsafe extern "C" fn(
            *const FbSessionConfig,
            *const c_char,
            *const c_char,
            *mut *mut FbSession,
        ) -> FbResult = fb_session_create;
        let _: unsafe extern "C" fn(*mut FbSession, *const FbPose, *const FbVec3) -> FbResult =
            fb_session_update_listener;
        let _: unsafe extern "C" fn(*mut FbSession, u32, *const FbSourceUpdate) -> FbResult =
            fb_session_update_source;
        let _: unsafe extern "C" fn(
            *mut FbSession,
            *const f32,
            usize,
            *mut f32,
            usize,
        ) -> FbResult = fb_session_render_block;
        let _: unsafe extern "C" fn(*mut FbSession, *mut c_char, usize, *mut usize) -> FbResult =
            fb_session_telemetry_json;
        let _: unsafe extern "C" fn(*mut FbSession) -> FbResult = fb_session_destroy;
    }

    #[test]
    fn additive_v2_exported_function_signatures_are_frozen() {
        let _: unsafe extern "C" fn(
            *const FbSessionConfigV2,
            *const c_char,
            *const c_char,
            *mut *mut FbSession,
        ) -> FbResult = fb_session_create_v2;
        let _: unsafe extern "C" fn(*mut FbSession, *const FbSourceProgramConfigV2) -> FbResult =
            fb_session_configure_source_v2;
        let _: unsafe extern "C" fn(*mut FbSession) -> FbResult = fb_session_prepare_spatial_v2;
        let _: unsafe extern "C" fn(*mut FbSession, *const FbControlFrameV2) -> FbResult =
            fb_session_update_control_frame_v2;
        let _: unsafe extern "C" fn(*mut FbSession, *const FbSpatialRenderBlockV2) -> FbResult =
            fb_session_render_spatial_v2;
    }

    #[test]
    fn legacy_v1_null_argument_behavior_is_frozen() {
        let mut session = ptr::without_provenance_mut::<FbSession>(1);
        let sample = 0.0_f32;
        let mut output = 0.0_f32;
        let mut required = 7_usize;

        // Safety: every function must reject the null session/config before it
        // dereferences the remaining pointers. The non-null scalar buffers are
        // valid for their declared one-sample lengths.
        unsafe {
            assert_eq!(
                fb_session_create(ptr::null(), ptr::null(), ptr::null(), &mut session),
                FbResult::FbInvalidArgument
            );
            assert!(session.is_null());
            assert_eq!(
                fb_session_update_listener(ptr::null_mut(), ptr::null(), ptr::null()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_update_source(ptr::null_mut(), 0, ptr::null()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_render_block(ptr::null_mut(), &sample, 1, &mut output, 1,),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_telemetry_json(ptr::null_mut(), ptr::null_mut(), 0, &mut required),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_destroy(ptr::null_mut()),
                FbResult::FbInvalidArgument
            );
        }
        assert_eq!(required, 7);
    }

    fn fake_session_with_route_and_source_count(
        route: SessionRoute,
        source_count: usize,
    ) -> *mut FbSession {
        let session = SessionInner {
            control: UnsafeCell::new(None),
            render: UnsafeCell::new(None),
            spatial_control: UnsafeCell::new(None),
            spatial_render: UnsafeCell::new(None),
            source_count,
            sample_rate_hz: 48_000,
            block_size: 4,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: 2 * 4 * size_of::<f32>() as u64,
            route,
            spatial: UnsafeCell::new(
                (route == SessionRoute::NeutralSpatial).then(SpatialShellState::new),
            ),
            spatial_lifecycle: SpatialLifecycleState::new(SpatialShellLifecycle::Collecting),
            spatial_realtime_clock: (route == SessionRoute::NeutralSpatial)
                .then(|| RealtimeClock::new().unwrap()),
            spatial_block_start_frame: AtomicU64::new(0),
        };
        Box::into_raw(Box::new(session)).cast()
    }

    fn fake_session_with_route(route: SessionRoute) -> *mut FbSession {
        fake_session_with_route_and_source_count(route, 1)
    }

    fn fake_session() -> *mut FbSession {
        fake_session_with_route(SessionRoute::LegacyFinalStereo)
    }

    fn fake_bound_failing_spatial_session() -> *mut FbSession {
        const BLOCK_SIZE: usize = 4;
        let (timing_writer, _timing_reader) = CallbackTimingPublication::new();
        let shape = SpatialSourceShape {
            channel_count: 1,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryPointV2 as u32,
            multipoint_count: 0,
            extent_m: 0.0,
            presentation_provenance: SpatialPresentationProvenance::NativeMono,
        };
        let update = SimulationUpdate {
            sources: std::array::from_fn(|source_index| SourceMotion {
                active: source_index == 0,
                ..SourceMotion::default()
            }),
            ..default_simulation_update()
        };
        let (_writer, reader) =
            SnapshotPublication::new(propagation_snapshot_from_update(&update, 1, 0));
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            EngineConfig {
                block_size_frames: BLOCK_SIZE as u32,
                max_active_sources: 1,
                ..EngineConfig::default()
            },
            reader,
            &[1],
            Box::new(FailingSpatialBackend),
        )
        .unwrap();
        graph
            .set_source(
                0,
                &source_profile_for_spatial_shape(0, shape, ReferenceLevel::CreativeDb { db: 0.0 })
                    .unwrap(),
                SceneCalibration::default(),
            )
            .unwrap();

        let (_macro_command_publisher, macro_command_receiver) =
            steam_macro_ingress_activation_channel();
        let (macro_acknowledgement_publisher, _macro_acknowledgement_receiver) =
            steam_macro_ingress_acknowledgement_channel();
        let (macro_route_writer, _macro_route_reader) = macro_ingress_render_snapshot_channel();
        let mut shell = SpatialShellState::new();
        shell.source_shapes[0] = shape;
        shell.configured[0] = true;
        shell.configured_count = 1;
        let session = SessionInner {
            control: UnsafeCell::new(None),
            render: UnsafeCell::new(None),
            spatial_control: UnsafeCell::new(None),
            spatial_render: UnsafeCell::new(Some(SpatialRenderState {
                graph,
                macro_audio: MacroBridgeAudio {
                    enabled: false,
                    command_receiver: macro_command_receiver,
                    acknowledgement_publisher: macro_acknowledgement_publisher,
                    pending_commands: None,
                    route_writer: macro_route_writer,
                    route_snapshot: MacroIngressRenderSnapshot::default(),
                    roles: [MacroAudioRole::default(); EventRole::COUNT],
                    pending_acknowledgements: [None; EventRole::COUNT],
                    staged_block_start_frame: None,
                },
                presentation_bank: vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE],
                environmental_bank: vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE],
                metadata: SpatialOutputMetadata::default(),
                timing_writer,
            })),
            source_count: 1,
            sample_rate_hz: 48_000,
            block_size: BLOCK_SIZE,
            last_render_ns: AtomicU64::new(0),
            ffi_render_buffers_bytes: ((MAX_SPATIAL_PRESENTATION_FEEDS
                + MAX_SPATIAL_ENVIRONMENT_PLANES)
                * BLOCK_SIZE
                * size_of::<f32>()) as u64,
            route: SessionRoute::NeutralSpatial,
            spatial: UnsafeCell::new(Some(shell)),
            spatial_lifecycle: SpatialLifecycleState::new(
                SpatialShellLifecycle::PreparedNotStarted,
            ),
            spatial_realtime_clock: Some(RealtimeClock::new().unwrap()),
            spatial_block_start_frame: AtomicU64::new(0),
        };
        Box::into_raw(Box::new(session)).cast()
    }

    fn chicago_fixture_c_paths() -> (std::ffi::CString, std::ffi::CString) {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let package =
            repository.join("platforms/ios/FightboxApp/Resources/chicago-block-a.fightbox");
        let bake = repository.join("platforms/ios/FightboxApp/Resources/chicago-block-baked");
        (
            std::ffi::CString::new(package.to_str().expect("UTF-8 fixture path")).unwrap(),
            std::ffi::CString::new(bake.to_str().expect("UTF-8 fixture path")).unwrap(),
        )
    }

    fn default_ffi_pose_at(east_m: f32, north_m: f32, up_m: f32) -> FbPose {
        FbPose {
            position: FbVec3 {
                east_m,
                north_m,
                up_m,
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
        }
    }

    fn control_frame_for(
        listener_pose: FbPose,
        listener_linear_velocity_mps: FbVec3,
        source_updates: &[FbSourceUpdate],
    ) -> FbControlFrameV2 {
        FbControlFrameV2 {
            source_updates: source_updates.as_ptr(),
            source_count: source_updates.len() as u32,
            listener_pose,
            listener_linear_velocity_mps,
            ..FbControlFrameV2::default()
        }
    }

    fn source_update_at(active: bool, east_m: f32) -> FbSourceUpdate {
        FbSourceUpdate {
            active: u8::from(active),
            pose: default_ffi_pose_at(east_m, 2.0, 0.0),
            linear_velocity_mps: FbVec3::default(),
        }
    }

    fn create_configured_neutral_point_session(
        block_size_frames: u32,
        source_count: u32,
    ) -> *mut FbSession {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames,
            source_count,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        // Safety: every input and the output slot live through the call.
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        for source_index in 0..source_count {
            let source_config = FbSourceProgramConfigV2 {
                source_index,
                ..FbSourceProgramConfigV2::default()
            };
            assert_eq!(
                unsafe { fb_session_configure_source_v2(session, &source_config) },
                FbResult::FbOk
            );
        }
        session
    }

    fn install_test_echo_authority(session: *mut FbSession) -> fightbox_world::StableSpatialKey {
        use fightbox_world::{
            EchoAuthorityBindings, EchoAuthorityTable, EchoPathKind, EchoPathRecord,
            EchoPathVertex, ListenerNode, PackageEchoAuthority, PlanTile, PlanTileContent,
            Sha256Digest, StableGeometryKey, StablePathKey, StableSpatialKey, StaticSourceAnchor,
        };
        let inner = unsafe { session_ref(session) }.unwrap();
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        let selected = control.macro_ingress.telemetry().selected_cell;
        let generation = control.runner.world_diagnostics().generation;
        let anchor = StableSpatialKey::derive("test-echo-anchor", b"macro-ingress");
        let listener_cell = fightbox_world::echo_listener_cell_key(&selected.cell);
        let node_keys = [
            StableSpatialKey::derive("test-echo-node", b"a"),
            StableSpatialKey::derive("test-echo-node", b"b"),
            StableSpatialKey::derive("test-echo-node", b"c"),
        ];
        let positions = [[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [0.0, 10.0, 0.0]];
        let listeners = node_keys
            .into_iter()
            .zip(positions)
            .map(|(key, position_city_enu_m)| ListenerNode {
                key,
                listener_cell,
                position_city_enu_m,
            })
            .collect();
        let vertices = positions.map(|listener| EchoPathVertex {
            total_path_m: 180.0,
            excess_path_m: 103.0,
            final_interaction_city_enu_m: [20.0, 12.0, 4.0],
            arrival_vector_city_enu_m: [listener[0] - 20.0, listener[1] - 12.0, listener[2] - 4.0],
            material_pressure: [0.82, 0.74, 0.58],
            predicted_received_pressure: [0.0045, 0.0040, 0.0031],
        });
        let table = EchoAuthorityTable::new(
            EchoAuthorityBindings {
                package_manifest_hash: Sha256Digest([1; 32]),
                mesh_hash: Sha256Digest([2; 32]),
                material_hash: Sha256Digest([3; 32]),
                fixture_request_hash: Sha256Digest([4; 32]),
                anchor_set_hash: Sha256Digest([5; 32]),
                probe_layout_hash: Sha256Digest([6; 32]),
                coordinate_frame: StableSpatialKey::derive("test-echo-frame", b"enu"),
                baker_revision: 1,
                sound_contract_revision: 1,
            },
            vec![StaticSourceAnchor {
                key: anchor,
                position_city_enu_m: [600.0, 0.0, 0.0],
            }],
            listeners,
            vec![PlanTile {
                key: StableSpatialKey::derive("test-echo-tile", b"one"),
                source_anchor: anchor,
                listener_cell,
                listener_vertices: node_keys,
                direct_occluded: false,
                content: PlanTileContent::Paths(vec![EchoPathRecord {
                    path_key: StablePathKey::derive("test-echo-path", b"one"),
                    geometry_key: StableGeometryKey::derive("test-echo-geometry", b"wall"),
                    material_key: StableGeometryKey::derive("test-echo-material", b"concrete"),
                    kind: EchoPathKind::Specular,
                    vertices,
                }]),
            }],
        )
        .unwrap();
        let encoded = table.encode();
        let package = Arc::new(PackageEchoAuthority {
            table,
            content_sha256: fightbox_world::Sha256Digest::from_bytes(&encoded).to_hex(),
            serialized_size_bytes: encoded.len() as u64,
            resident_size_bytes: encoded.len() as u64,
            cell_id: selected.cell.clone(),
        });
        let artifact = |fill: char| {
            CellArtifactIdentity::new(selected.clone(), generation, fill.to_string().repeat(64))
        };
        control.macro_ingress.publish_active_cell(
            selected.clone(),
            Some(LocalCellAuthority {
                cell: selected.clone(),
                world_generation: generation,
                package: Some(artifact('1')),
                probe_bake: Some(artifact('2')),
                echo_authority: Some(artifact('3')),
            }),
        );
        control.active_echo_authority = Some(package);
        anchor
    }

    fn create_configured_macro_bridge_session(block_size_frames: u32) -> *mut FbSession {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames,
            source_count: MAX_ACTIVE_SOURCES as u32,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        let bridge_config = FbMacroProductionBridgeConfigV3 {
            diffuse_wet_gain: 0.22,
            diffuse_rt60_s: 1.1,
            diffuse_high_frequency_damping: 0.58,
            ..FbMacroProductionBridgeConfigV3::default()
        };
        assert_eq!(
            unsafe { fb_session_enable_macro_production_bridge_v3(session, &bridge_config) },
            FbResult::FbOk
        );
        for source_index in 0..MAX_ACTIVE_SOURCES as u32 {
            let source_config = FbSourceProgramConfigV2 {
                source_index,
                channel_count: 1,
                source_geometry: FbSourceGeometryV2::FbSourceGeometryPointV2 as u32,
                ..FbSourceProgramConfigV2::default()
            };
            assert_eq!(
                unsafe { fb_session_configure_source_v2(session, &source_config) },
                FbResult::FbOk
            );
        }
        session
    }

    fn create_v1_legacy_session(block_size_frames: u32, source_count: u32) -> *mut FbSession {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfig {
            block_size_frames,
            source_count,
            ..FbSessionConfig::default()
        };
        let mut session = ptr::null_mut();
        // Safety: every input and the output slot live through the call.
        assert_eq!(
            unsafe { fb_session_create(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        session
    }

    fn create_bound_active_point_session(block_size_frames: u32) -> *mut FbSession {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames,
            source_count: 1,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        // Safety: every input and the output slot live through the call.
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        let source_config = FbSourceProgramConfigV2::default();
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_config) },
            FbResult::FbOk
        );
        let listener = default_ffi_pose_at(0.0, 0.0, 0.0);
        let velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        let source = FbSourceUpdate {
            active: 1,
            pose: default_ffi_pose_at(1.0, 2.0, 0.0),
            linear_velocity_mps: velocity,
        };
        assert_eq!(
            unsafe { fb_session_update_source(session, 0, &source) },
            FbResult::FbOk
        );
        session
    }

    fn create_ready_active_point_session(block_size_frames: u32) -> *mut FbSession {
        let session = create_bound_active_point_session(block_size_frames);
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        session
    }

    #[test]
    fn macro_v3_prepare_readiness_and_discard_preserve_v2_dormant_behavior() {
        let session = create_configured_macro_bridge_session(128);
        let request = FbMacroEventRequestV2 {
            event_id: 201,
            atomic_group_id: 91,
            role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetPreGeneratedV2 as u32,
            asset_key: 9_201,
            emission_frame: 100,
            program_seek_frame: 27,
            retained_frames_after_activation: 48_000,
            emitter_position_enu: FbVec3 {
                east_m: 601.0,
                north_m: 0.0,
                up_m: 0.0,
            },
            local_horizon_m: 600.0,
            ..FbMacroEventRequestV2::default()
        };
        assert_eq!(
            unsafe { fb_session_admit_macro_event_group_v2(session, &request, 1) },
            FbResult::FbOk
        );

        let binding = FbMacroEchoAnchorBindingV3 {
            event_id: 201,
            anchor_key: [9; 16],
            ..FbMacroEchoAnchorBindingV3::default()
        };
        assert_eq!(
            unsafe { fb_session_bind_macro_echo_anchor_v3(session, &binding) },
            FbResult::FbOk
        );

        let mut wrong_header = FbMacroPrepareBatchV3::default();
        wrong_header.abi_version = 2;
        assert_eq!(
            unsafe { fb_session_prepare_macro_token_v3(session, 300, &mut wrong_header) },
            FbResult::FbInvalidArgument
        );
        let mut prepared = FbMacroPrepareBatchV3::default();
        assert_eq!(
            unsafe { fb_session_prepare_macro_token_v3(session, 300, &mut prepared) },
            FbResult::FbOk
        );
        assert_ne!(prepared.token_id, 0);
        assert_eq!(prepared.event_count, 1);
        assert_eq!(
            prepared.status,
            FbMacroTokenStatusV3::FbMacroTokenReservedV3 as u32
        );
        assert_eq!(prepared.events[0].event_id, 201);
        assert_eq!(prepared.events[0].asset_key, 9_201);
        assert_eq!(prepared.events[0].program_seek_frame, 27);
        {
            let inner = unsafe { session_ref(session) }.unwrap();
            let control = unsafe { &mut *inner.spatial_control.get() }
                .as_mut()
                .unwrap();
            assert_eq!(
                control.macro_bridge.reservation.unwrap().events.events[0].echo_anchor_key,
                [9; 16]
            );
        }
        assert_eq!(
            unsafe { fb_session_bind_macro_echo_anchor_v3(session, &binding) },
            FbResult::FbInvalidState
        );

        let mut v2_while_reserved = FbMacroActivationBatchV2::default();
        assert_eq!(
            unsafe { fb_session_activate_macro_events_v2(session, 300, &mut v2_while_reserved) },
            FbResult::FbInvalidState
        );
        let mut ready = FbMacroReadyAssetV3 {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: size_of::<FbMacroReadyAssetV3>() as u32,
            token_id: prepared.token_id,
            event_id: 201,
            role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
            source_index: 13,
            asset_key: 9_201,
            program_seek_frame: 27,
            discontinuity_sequence: 7,
            reserved: [0; 2],
        };
        assert_eq!(
            unsafe { fb_session_stage_macro_ready_v3(session, &ready, 1) },
            FbResult::FbInvalidArgument
        );
        ready.discontinuity_sequence = 8;
        assert_eq!(
            unsafe { fb_session_stage_macro_ready_v3(session, &ready, 1) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_discard_macro_token_v3(session, prepared.token_id) },
            FbResult::FbOk
        );

        let mut activated = FbMacroActivationBatchV2::default();
        assert_eq!(
            unsafe { fb_session_activate_macro_events_v2(session, 300, &mut activated) },
            FbResult::FbOk
        );
        assert_eq!(activated.activation_count, 1);
        assert_eq!(activated.activations[0].event_id, 201);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn macro_v3_shared_diffuse_fallback_commits_to_runtime_only_role() {
        let session = create_configured_macro_bridge_session(128);
        let request = FbMacroEventRequestV2 {
            event_id: 251,
            atomic_group_id: 251,
            role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetPreGeneratedV2 as u32,
            asset_key: 9_251,
            emission_frame: 0,
            program_seek_frame: 3,
            retained_frames_after_activation: 128,
            emitter_position_enu: FbVec3 {
                east_m: 1.001,
                north_m: 0.0,
                up_m: 0.0,
            },
            local_horizon_m: 1.0,
            ..FbMacroEventRequestV2::default()
        };
        assert_eq!(
            unsafe { fb_session_admit_macro_event_group_v2(session, &request, 1) },
            FbResult::FbOk
        );
        let mut prepared = FbMacroPrepareBatchV3::default();
        assert_eq!(
            unsafe { fb_session_prepare_macro_token_v3(session, 0, &mut prepared) },
            FbResult::FbOk
        );
        let ready = FbMacroReadyAssetV3 {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: size_of::<FbMacroReadyAssetV3>() as u32,
            token_id: prepared.token_id,
            event_id: 251,
            role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
            source_index: 13,
            asset_key: 9_251,
            program_seek_frame: 3,
            discontinuity_sequence: 8,
            reserved: [0; 2],
        };
        assert_eq!(
            unsafe { fb_session_stage_macro_ready_v3(session, &ready, 1) },
            FbResult::FbOk
        );
        let source_updates: [FbSourceUpdate; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|_| source_update_at(false, 0.0));
        let frame = control_frame_for(
            default_ffi_pose_at(0.0, 0.0, 0.0),
            FbVec3::default(),
            &source_updates,
        );
        let mut committed = FbMacroCommitResultV3::default();
        assert_eq!(
            unsafe {
                fb_session_update_control_frame_macro_v3(
                    session,
                    &frame,
                    prepared.token_id,
                    &mut committed,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(
            committed.status,
            FbMacroTokenStatusV3::FbMacroTokenCommittedV3 as u32
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        let command = control
            .macro_bridge
            .overlay
            .active_command(EventRole::StandardImpulse)
            .unwrap();
        assert_eq!(
            command.mode,
            fightbox_steam_audio::SteamMacroIngressMode::MacroFallback {
                shared_diffuse: true
            }
        );
        assert!(command.detailed_source_motion().is_none());
        assert!(
            control
                .macro_bridge
                .overlay
                .overlay_runtime_activity([false; MAX_ACTIVE_SOURCES])[13]
        );
        assert_eq!(
            unsafe { fb_session_discard_macro_token_v3(session, prepared.token_id) },
            FbResult::FbInvalidState
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn macro_v3_authored_anchor_queries_fixed_echo_plan_and_pins_authority() {
        let session = create_configured_macro_bridge_session(128);
        let anchor = install_test_echo_authority(session);
        let request = FbMacroEventRequestV2 {
            event_id: 252,
            atomic_group_id: 252,
            role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetSeekableV2 as u32,
            asset_key: 25_200,
            emission_frame: 0,
            retained_frames_after_activation: 256,
            emitter_position_enu: FbVec3 {
                east_m: 600.0,
                north_m: 0.0,
                up_m: 0.0,
            },
            local_horizon_m: 600.0,
            ..FbMacroEventRequestV2::default()
        };
        assert_eq!(
            unsafe { fb_session_admit_macro_event_group_v2(session, &request, 1) },
            FbResult::FbOk
        );
        let binding = FbMacroEchoAnchorBindingV3 {
            event_id: 252,
            anchor_key: anchor.0,
            ..FbMacroEchoAnchorBindingV3::default()
        };
        assert_eq!(
            unsafe { fb_session_bind_macro_echo_anchor_v3(session, &binding) },
            FbResult::FbOk
        );
        let mut prepared = FbMacroPrepareBatchV3::default();
        assert_eq!(
            unsafe { fb_session_prepare_macro_token_v3(session, 0, &mut prepared) },
            FbResult::FbOk
        );
        let ready = FbMacroReadyAssetV3 {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: size_of::<FbMacroReadyAssetV3>() as u32,
            token_id: prepared.token_id,
            event_id: prepared.events[0].event_id,
            role: prepared.events[0].role,
            source_index: fightbox_steam_audio::MacroIngressSlotMap::default()
                .source_index(EventRole::StandardImpulse) as u32,
            asset_key: prepared.events[0].asset_key,
            program_seek_frame: prepared.events[0].program_seek_frame,
            discontinuity_sequence: 12,
            ..FbMacroReadyAssetV3::default()
        };
        assert_eq!(
            unsafe { fb_session_stage_macro_ready_v3(session, &ready, 1) },
            FbResult::FbOk
        );
        let source_updates: [FbSourceUpdate; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|_| source_update_at(false, 0.0));
        let frame = control_frame_for(
            default_ffi_pose_at(0.0, 0.0, 0.0),
            FbVec3::default(),
            &source_updates,
        );
        let mut committed = FbMacroCommitResultV3::default();
        assert_eq!(
            unsafe {
                fb_session_update_control_frame_macro_v3(
                    session,
                    &frame,
                    prepared.token_id,
                    &mut committed,
                )
            },
            FbResult::FbOk
        );
        let telemetry = read_session_telemetry(session);
        assert_eq!(telemetry["macro_echo_binding"]["committed_queries"], 1);
        assert_eq!(telemetry["macro_echo_binding"]["committed_plans"], 1);
        assert_eq!(telemetry["macro_echo_binding"]["committed_taps"], 1);
        assert_eq!(telemetry["macro_echo_binding"]["pinned_role_count"], 1);
        let ack = {
            let inner = unsafe { session_ref(session) }.unwrap();
            let control = unsafe { &mut *inner.spatial_control.get() }
                .as_mut()
                .unwrap();
            let command = control
                .macro_bridge
                .overlay
                .active_command(EventRole::StandardImpulse)
                .unwrap();
            assert_eq!(command.echo_plan.tap_count, 1);
            assert_ne!(command.echo_plan.taps[0].path_key, [0; 16]);
            assert!(command.echo_plan.taps[0].delay_samples > 14_000.0);
            assert!(command.tail_deadline_frame > command.program_end_frame + 100_000);
            assert_eq!(committed.tail_deadline_frame, command.tail_deadline_frame);
            let pin = control.macro_bridge.echo_authority_pins[EventRole::StandardImpulse.index()]
                .as_ref()
                .unwrap();
            assert_eq!(pin.table.anchors().len(), 1);
            control
                .macro_bridge
                .reservation
                .as_mut()
                .unwrap()
                .acknowledged_mask |= 1_u8 << EventRole::StandardImpulse.index();
            FbMacroAudioAckV3 {
                token_id: command.activation_epoch,
                event_id: command.event_id.0,
                role: FbMacroEventRoleV2::FbMacroStandardImpulseV2 as u32,
                status: FbMacroAudioAckStatusV3::FbMacroAudioCompletedV3 as u32,
                asset_key: command.asset_key,
                source_index: 13,
                discontinuity_sequence: command.asset_readiness_generation,
                direct_generation: command.direct_generation,
                effective_frame: command.effective_frame,
                program_seek_frame: command.program_seek_frame,
                ..FbMacroAudioAckV3::default()
            }
        };
        assert_eq!(
            unsafe { fb_session_finalize_macro_ack_v3(session, &ack) },
            FbResult::FbOk
        );
        {
            let inner = unsafe { session_ref(session) }.unwrap();
            let control = unsafe { &mut *inner.spatial_control.get() }
                .as_mut()
                .unwrap();
            assert!(control.macro_bridge.reservation.is_none());
            assert!(
                control.macro_bridge.echo_authority_pins[EventRole::StandardImpulse.index()]
                    .is_none()
            );
        }
        let telemetry = read_session_telemetry(session);
        assert_eq!(telemetry["macro_echo_binding"]["pinned_role_count"], 0);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn macro_v3_exact_commit_callback_and_ack_finalize_are_one_transaction() {
        const BLOCK: usize = 128;
        let session = create_configured_macro_bridge_session(BLOCK as u32);
        let source_updates: [FbSourceUpdate; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|_| source_update_at(false, 0.0));
        let frame = control_frame_for(
            default_ffi_pose_at(0.0, 0.0, 0.0),
            FbVec3::default(),
            &source_updates,
        );
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &frame) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        let request = FbMacroEventRequestV2 {
            event_id: 301,
            atomic_group_id: 301,
            role: FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetPreGeneratedV2 as u32,
            asset_key: 9_301,
            emission_frame: BLOCK as u64,
            program_seek_frame: 5,
            retained_frames_after_activation: BLOCK as u64,
            emitter_position_enu: FbVec3 {
                east_m: 1.001,
                north_m: 0.0,
                up_m: 0.0,
            },
            local_horizon_m: 1.0,
            ..FbMacroEventRequestV2::default()
        };
        assert_eq!(
            unsafe { fb_session_admit_macro_event_group_v2(session, &request, 1) },
            FbResult::FbOk
        );
        let mut prepared = FbMacroPrepareBatchV3::default();
        assert_eq!(
            unsafe { fb_session_prepare_macro_token_v3(session, BLOCK as u64, &mut prepared) },
            FbResult::FbOk
        );
        assert_eq!(prepared.event_count, 1);
        assert_eq!(prepared.events[0].activation_frame, BLOCK as u64);
        let ready = FbMacroReadyAssetV3 {
            abi_version: FB_ABI_VERSION_V3,
            struct_size: size_of::<FbMacroReadyAssetV3>() as u32,
            token_id: prepared.token_id,
            event_id: 301,
            role: FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32,
            source_index: 14,
            asset_key: 9_301,
            program_seek_frame: 5,
            discontinuity_sequence: 8,
            reserved: [0; 2],
        };
        assert_eq!(
            unsafe { fb_session_stage_macro_ready_v3(session, &ready, 1) },
            FbResult::FbOk
        );
        // Start the prepared session with one ordinary silent callback. The
        // token remains Ready/dormant; its exact effective frame is the next
        // callback boundary.
        let mut initial_callback = FbMacroProgramRequestBatchV3::default();
        assert_eq!(
            unsafe { fb_session_macro_render_begin_v3(session, &mut initial_callback) },
            FbResult::FbOk
        );
        assert_eq!(initial_callback.request_count, 0);
        let initial_program_bank = vec![0.0_f32; MAX_ACTIVE_SOURCES * BLOCK];
        let initial_programs: [FbSourceProgramInputV2; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|source_index| FbSourceProgramInputV2 {
                source_index: source_index as u32,
                channel_count: 1,
                samples: initial_program_bank[source_index * BLOCK..].as_ptr(),
                sample_count: BLOCK,
                channel_stride_samples: BLOCK,
                reserved: [0; 4],
            });
        let mut initial_direct = vec![0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK];
        let mut initial_environmental = vec![0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK];
        let mut initial_feeds =
            [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut initial_metadata = FbSpatialBlockMetadataV2::default();
        let initial_block = FbSpatialRenderBlockV2 {
            source_programs: initial_programs.as_ptr(),
            source_program_count: initial_programs.len() as u32,
            direct_output: FbPlanarOutputV2 {
                samples: initial_direct.as_mut_ptr(),
                sample_capacity: initial_direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: initial_environmental.as_mut_ptr(),
                sample_capacity: initial_environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK,
                reserved: [0; 4],
            },
            feed_metadata: initial_feeds.as_mut_ptr(),
            feed_metadata_capacity: initial_feeds.len() as u32,
            block_metadata: &mut initial_metadata,
            ..FbSpatialRenderBlockV2::default()
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &initial_block) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe {
                fb_session_macro_render_end_v3(
                    session,
                    FbMacroRenderDispositionV3::FbMacroRenderCommitV3 as u32,
                )
            },
            FbResult::FbOk
        );

        let mut committed = FbMacroCommitResultV3::default();
        assert_eq!(
            unsafe {
                fb_session_update_control_frame_macro_v3(
                    session,
                    &frame,
                    prepared.token_id,
                    &mut committed,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(
            committed.status,
            FbMacroTokenStatusV3::FbMacroTokenCommittedV3 as u32
        );
        assert_ne!(committed.direct_generation, 0);
        assert_eq!(committed.effective_frame, BLOCK as u64);
        assert!(committed.tail_deadline_frame >= committed.effective_frame);
        let mut callback = FbMacroProgramRequestBatchV3::default();
        assert_eq!(
            unsafe { fb_session_macro_render_begin_v3(session, &mut callback) },
            FbResult::FbOk
        );
        assert_eq!(callback.token_id, prepared.token_id);
        assert_eq!(callback.request_count, 1);
        let interval = callback.requests[0];
        assert_eq!(interval.event_id, 301);
        assert_eq!(interval.source_index, 14);
        assert_eq!(interval.asset_frame_start, 5);
        assert_eq!(interval.destination_frame_offset, 0);
        assert_eq!(interval.frame_count, BLOCK as u32);
        assert_eq!(interval.discontinuity_sequence, 8);
        assert_eq!(
            unsafe {
                fb_session_macro_render_end_v3(
                    session,
                    FbMacroRenderDispositionV3::FbMacroRenderDiscardV3 as u32,
                )
            },
            FbResult::FbOk
        );
        let mut retried_callback = FbMacroProgramRequestBatchV3::default();
        assert_eq!(
            unsafe { fb_session_macro_render_begin_v3(session, &mut retried_callback) },
            FbResult::FbOk
        );
        assert_eq!(retried_callback.request_count, 1);
        assert_eq!(retried_callback.requests[0].asset_frame_start, 5);
        assert_eq!(retried_callback.requests[0].frame_count, BLOCK as u32);
        assert_eq!(retried_callback.requests[0].destination_frame_offset, 0);

        let mut program_bank = vec![0.0_f32; MAX_ACTIVE_SOURCES * BLOCK];
        program_bank[14 * BLOCK..15 * BLOCK].fill(1.0);
        let programs: [FbSourceProgramInputV2; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|source_index| FbSourceProgramInputV2 {
                source_index: source_index as u32,
                channel_count: 1,
                samples: program_bank[source_index * BLOCK..].as_ptr(),
                sample_count: BLOCK,
                channel_stride_samples: BLOCK,
                reserved: [0; 4],
            });
        let mut direct = vec![0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK];
        let mut environmental = vec![0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: programs.as_ptr(),
            source_program_count: programs.len() as u32,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_ne!(metadata.validity, 0);
        assert_ne!(feeds[14 * 3].valid, 0);
        assert_eq!(
            unsafe {
                fb_session_macro_render_end_v3(
                    session,
                    FbMacroRenderDispositionV3::FbMacroRenderCommitV3 as u32,
                )
            },
            FbResult::FbOk
        );
        program_bank.fill(0.0);
        for expected_block_start in [2 * BLOCK as u64, 3 * BLOCK as u64] {
            let mut tail_callback = FbMacroProgramRequestBatchV3::default();
            assert_eq!(
                unsafe { fb_session_macro_render_begin_v3(session, &mut tail_callback) },
                FbResult::FbOk
            );
            assert_eq!(tail_callback.block_start_frame, expected_block_start);
            assert_eq!(tail_callback.request_count, 0);
            assert_eq!(
                unsafe { fb_session_render_spatial_v2(session, &block) },
                FbResult::FbOk
            );
            assert_eq!(
                unsafe {
                    fb_session_macro_render_end_v3(
                        session,
                        FbMacroRenderDispositionV3::FbMacroRenderCommitV3 as u32,
                    )
                },
                FbResult::FbOk
            );
        }
        let mut ack = FbMacroAudioAckV3::default();
        assert_eq!(
            unsafe { fb_session_poll_macro_ack_v3(session, &mut ack) },
            FbResult::FbOk
        );
        assert_eq!(ack.token_id, prepared.token_id);
        assert_eq!(ack.event_id, 301);
        assert_eq!(ack.discontinuity_sequence, 8);
        assert_eq!(ack.program_seek_frame, 5);
        assert_eq!(ack.direct_generation, committed.direct_generation);
        assert_eq!(ack.effective_frame, committed.effective_frame);
        let mut wrong_seek = ack;
        wrong_seek.program_seek_frame += 1;
        assert_eq!(
            unsafe { fb_session_finalize_macro_ack_v3(session, &wrong_seek) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_finalize_macro_ack_v3(session, &ack) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_finalize_macro_ack_v3(session, &ack) },
            FbResult::FbInvalidState
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn macro_ffi_admits_atomic_crack_blast_and_delivers_seek_and_authority() {
        let session = create_configured_neutral_point_session(128, 1);
        let atmosphere = FbAtmosphereObservationV2::default();
        assert_eq!(
            unsafe { fb_session_freeze_atmosphere_v2(session, &atmosphere) },
            FbResult::FbOk
        );

        let request = |event_id, role, seek| FbMacroEventRequestV2 {
            event_id,
            atomic_group_id: 77,
            role,
            asset_transport: FbMacroAssetTransportV2::FbMacroAssetPreGeneratedV2 as u32,
            asset_key: 9000 + event_id,
            emission_frame: 100,
            program_seek_frame: seek,
            retained_frames_after_activation: 48_000,
            emitter_position_enu: FbVec3 {
                east_m: 601.0,
                north_m: 0.0,
                up_m: 0.0,
            },
            local_horizon_m: 600.0,
            ..FbMacroEventRequestV2::default()
        };
        let requests = [
            request(101, FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32, 9),
            request(102, FbMacroEventRoleV2::FbMacroBallisticBlastV2 as u32, 17),
        ];
        assert_eq!(
            unsafe {
                fb_session_admit_macro_event_group_v2(
                    session,
                    requests.as_ptr(),
                    requests.len() as u32,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_freeze_atmosphere_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );

        let mut activated = FbMacroActivationBatchV2::default();
        assert_eq!(
            unsafe { fb_session_activate_macro_events_v2(session, 300, &mut activated) },
            FbResult::FbOk
        );
        assert_eq!(activated.activation_count, 2);
        let crack = activated.activations[0];
        let blast = activated.activations[1];
        assert_eq!(crack.event_id, 101);
        assert_eq!(crack.program_seek_frame, 9);
        assert_eq!(crack.atomic_group_id, 77);
        assert_eq!(
            crack.render_authority,
            FbMacroRenderAuthorityV2::FbMacroRenderAuthorityDetailedLocalV2 as u32
        );
        assert_eq!(
            crack.eligibility_bits,
            FB_MACRO_ELIGIBILITY_DETAILED_DIRECT_V2
        );
        assert_eq!(crack.shared_diffuse_fallback, 0);
        assert_eq!(
            crack.remote_direction_enu.east_m.to_bits(),
            1.0_f32.to_bits()
        );
        assert_eq!(blast.event_id, 102);
        assert_eq!(blast.program_seek_frame, 17);
        assert_eq!(
            blast.render_authority,
            FbMacroRenderAuthorityV2::FbMacroRenderAuthorityFallbackV2 as u32
        );
        assert_eq!(
            blast.fallback_reason,
            FbMacroFallbackReasonV2::FbMacroFallbackMissingEchoAuthorityV2 as u32
        );
        assert_eq!(blast.shared_diffuse_fallback, 1);
        assert!(blast.fallback_ear_arrival_frame > blast.macro_arrival_frame);
        assert_eq!(
            unsafe {
                fb_session_release_macro_event_v2(
                    session,
                    FbMacroEventRoleV2::FbMacroBallisticCrackV2 as u32,
                    101,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe {
                fb_session_release_macro_event_v2(
                    session,
                    FbMacroEventRoleV2::FbMacroBallisticBlastV2 as u32,
                    102,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    fn replace_spatial_backend_for_test(
        session: *mut FbSession,
        backend: Box<dyn fightbox_runtime::backend::SpatialBackendRenderGraph>,
    ) {
        // Safety: tests call this only while the bound session is quiescent on
        // their one control thread and before preparation makes audio legal.
        let inner = unsafe { session_ref(session) }.unwrap();
        let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
        let shape = spatial.source_shapes[0];
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        let sequence = control.runner.latest_direct_sequence();
        let (writer, reader) = SnapshotPublication::new(propagation_snapshot_from_update(
            &control.update,
            sequence,
            0,
        ));
        control.propagation_writer = writer;
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            EngineConfig {
                block_size_frames: inner.block_size as u32,
                max_active_sources: inner.source_count as u8,
                ..EngineConfig::default()
            },
            reader,
            &[usize::from(shape.channel_count)],
            backend,
        )
        .unwrap();
        graph.set_listener_state(control.update.listener);
        graph
            .set_source(
                0,
                &source_profile_for_spatial_shape(0, shape, ReferenceLevel::CreativeDb { db: 0.0 })
                    .unwrap(),
                SceneCalibration::default(),
            )
            .unwrap();
        let render = unsafe { &mut *inner.spatial_render.get() }
            .as_mut()
            .unwrap();
        render.graph = graph;
    }

    fn read_session_telemetry(session: *mut FbSession) -> serde_json::Value {
        let mut required = 0_usize;
        assert_eq!(
            unsafe { fb_session_telemetry_json(session, ptr::null_mut(), 0, &mut required) },
            FbResult::FbBufferTooSmall
        );
        let mut bytes = vec![0_u8; required];
        assert_eq!(
            unsafe {
                fb_session_telemetry_json(
                    session,
                    bytes.as_mut_ptr().cast::<c_char>(),
                    bytes.len(),
                    &mut required,
                )
            },
            FbResult::FbOk
        );
        assert_eq!(bytes.last(), Some(&0));
        serde_json::from_slice(&bytes[..bytes.len() - 1]).unwrap()
    }

    fn assert_valid_maximum_block(
        metadata: &FbSpatialBlockMetadataV2,
        feeds: &[FbPresentationFeedMetadataV2; MAX_SPATIAL_PRESENTATION_FEEDS],
        direct: &[f32],
        environmental: &[f32],
        expected_block_start_frame: u64,
        expected_active_feed_count: usize,
        stable_generation: &mut Option<u64>,
        stable_discontinuity_sequence: &mut Option<u64>,
    ) {
        assert_eq!(metadata.abi_version, FB_ABI_VERSION_V2);
        assert_eq!(
            metadata.struct_size as usize,
            size_of::<FbSpatialBlockMetadataV2>()
        );
        assert_eq!(metadata.sample_rate_hz, 48_000);
        assert_eq!(metadata.block_size_frames, 128);
        assert_eq!(metadata.block_start_frame, expected_block_start_frame);
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert_ne!(metadata.flags & FB_SPATIAL_BLOCK_VALID_V2, 0);
        assert_eq!(metadata.flags & FB_SPATIAL_BLOCK_DISCONTINUITY_V2, 0);
        assert_eq!(
            metadata.active_presentation_feed_count,
            expected_active_feed_count as u32
        );
        assert_eq!(
            feeds.iter().filter(|feed| feed.valid != 0).count(),
            expected_active_feed_count
        );
        assert_eq!(metadata.environmental_order, 2);
        assert_eq!(metadata.environmental_channel_count, 9);
        assert_eq!(
            metadata.environmental_channel_order,
            FbEnvironmentalChannelOrderV2::FbEnvironmentalAcnV2 as u32
        );
        assert_eq!(
            metadata.environmental_normalization,
            FbEnvironmentalNormalizationV2::FbEnvironmentalN3dV2 as u32
        );
        assert_eq!(
            metadata.environmental_basis,
            FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2 as u32
        );
        assert!(direct.iter().all(|sample| sample.is_finite()));
        assert!(environmental.iter().all(|sample| sample.is_finite()));

        match *stable_generation {
            Some(expected) => assert_eq!(metadata.generation, expected),
            None => *stable_generation = Some(metadata.generation),
        }
        match *stable_discontinuity_sequence {
            Some(expected) => assert_eq!(metadata.discontinuity_sequence, expected),
            None => *stable_discontinuity_sequence = Some(metadata.discontinuity_sequence),
        }
    }

    #[test]
    fn null_and_misaligned_pointers_are_rejected() {
        // Safety: deliberately invalid pointers are rejected before dereference.
        unsafe {
            assert_eq!(
                fb_session_destroy(ptr::null_mut()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_update_listener(ptr::null_mut(), ptr::null(), ptr::null()),
                FbResult::FbInvalidArgument
            );
            assert_eq!(
                fb_session_destroy(1_usize as *mut FbSession),
                FbResult::FbInvalidArgument
            );
        }
    }

    #[test]
    fn handle_lifecycle_releases_a_live_allocation() {
        let session = fake_session();
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn render_validates_lengths_before_touching_backend_state() {
        let session = fake_session();
        // Safety: the fake handle points at a live `SessionInner` allocation.
        let inner = unsafe { session_ref(session) }.unwrap();
        inner.last_render_ns.store(73, Ordering::Release);
        let input = [0.0_f32; 4];
        let mut output = [1.0_f32; 8];
        // Safety: all buffers are valid for their declared lengths.
        let result =
            unsafe { fb_session_render_block(session, input.as_ptr(), 3, output.as_mut_ptr(), 8) };
        assert_eq!(result, FbResult::FbInvalidArgument);
        assert_eq!(inner.last_render_ns.load(Ordering::Acquire), 73);
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_wrong_route_calls_reject_without_reading_argument_structures() {
        let session = fake_session();
        // Safety: a live legacy handle must reject neutral-only calls before
        // dereferencing their deliberately null config/block pointers.
        unsafe {
            assert_eq!(
                fb_session_configure_source_v2(session, ptr::null()),
                FbResult::FbInvalidState
            );
            assert_eq!(
                fb_session_prepare_spatial_v2(session),
                FbResult::FbInvalidState
            );
            assert_eq!(
                fb_session_render_spatial_v2(session, ptr::null()),
                FbResult::FbInvalidState
            );
            assert_eq!(fb_session_destroy(session), FbResult::FbOk);
        }
        // Safety: the null handle is rejected before route inspection.
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(ptr::null_mut()) },
            FbResult::FbInvalidArgument
        );
    }

    #[test]
    fn v2_create_validates_header_config_paths_and_out_pointer_before_backend() {
        let path = std::ffi::CString::new("unused-but-valid").unwrap();
        let mut out = ptr::without_provenance_mut::<FbSession>(1);
        let config = FbSessionConfigV2::default();

        // Safety: all current-size inputs are live for each call. A valid
        // neutral config proceeds far enough to diagnose the missing package.
        assert_eq!(
            unsafe { fb_session_create_v2(&config, path.as_ptr(), path.as_ptr(), &mut out) },
            FbResult::FbInvalidPackage
        );
        assert!(out.is_null());

        let short = ShortV2Header {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: size_of::<ShortV2Header>() as u32,
        };
        out = ptr::without_provenance_mut(1);
        // Safety: `short` supplies exactly the universally readable header; its
        // declared size deliberately does not admit a current config copy.
        assert_eq!(
            unsafe {
                fb_session_create_v2(
                    (&short as *const ShortV2Header).cast(),
                    path.as_ptr(),
                    path.as_ptr(),
                    &mut out,
                )
            },
            FbResult::FbInvalidArgument
        );
        assert!(out.is_null());

        for invalid in [
            FbSessionConfigV2 {
                abi_version: 1,
                ..config
            },
            FbSessionConfigV2 {
                struct_size: (size_of::<FbSessionConfigV2>() - 1) as u32,
                ..config
            },
            FbSessionConfigV2 {
                render_route: u32::MAX,
                ..config
            },
            FbSessionConfigV2 {
                environmental_order: FB_MAX_ENVIRONMENTAL_ORDER_V2 + 1,
                ..config
            },
            FbSessionConfigV2 {
                reserved: [1, 0, 0, 0, 0, 0, 0],
                ..config
            },
        ] {
            out = ptr::without_provenance_mut(1);
            // Safety: invalid configs are full readable current-size objects.
            assert_eq!(
                unsafe { fb_session_create_v2(&invalid, ptr::null(), ptr::null(), &mut out) },
                FbResult::FbInvalidArgument
            );
            assert!(out.is_null());
        }

        let legacy_with_environment = FbSessionConfigV2 {
            render_route: FbRenderRouteV2::FbRenderLegacyFinalStereoV2 as u32,
            environmental_order: 1,
            ..config
        };
        out = ptr::without_provenance_mut(1);
        // Safety: full readable invalid config; invalid config wins before the
        // deliberately null paths.
        assert_eq!(
            unsafe {
                fb_session_create_v2(&legacy_with_environment, ptr::null(), ptr::null(), &mut out)
            },
            FbResult::FbInvalidArgument
        );
        assert!(out.is_null());

        let empty = std::ffi::CString::new("").unwrap();
        out = ptr::without_provenance_mut(1);
        // Safety: both C strings are live; the empty package path is rejected.
        assert_eq!(
            unsafe { fb_session_create_v2(&config, empty.as_ptr(), path.as_ptr(), &mut out) },
            FbResult::FbInvalidArgument
        );
        assert!(out.is_null());

        // Safety: a null output pointer is rejected before the null config is read.
        assert_eq!(
            unsafe { fb_session_create_v2(ptr::null(), ptr::null(), ptr::null(), ptr::null_mut()) },
            FbResult::FbInvalidArgument
        );
    }

    #[test]
    fn v2_render_requires_every_source_shape_before_reading_the_block() {
        let session = fake_session_with_route_and_source_count(SessionRoute::NeutralSpatial, 2);
        let source_zero = FbSourceProgramConfigV2::default();
        // Safety: the fake handle and full config are live.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_zero) },
            FbResult::FbOk
        );
        // The incomplete construction barrier wins before a null block read.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, ptr::null()) },
            FbResult::FbInvalidState,
            "incomplete construction wins before caller frame memory is read"
        );
        // Safety: the handle remains live.
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_starting_epoch_protocol_is_atomic_across_control_and_audio_threads() {
        let state = Arc::new(SpatialLifecycleState::new(
            SpatialShellLifecycle::PreparedNotStarted,
        ));
        let claimed = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let audio_state = Arc::clone(&state);
        let audio_claimed = Arc::clone(&claimed);
        let audio_release = Arc::clone(&release);
        let audio = std::thread::spawn(move || {
            let epoch = audio_state.claim_first_render().unwrap().unwrap();
            audio_claimed.wait();
            audio_release.wait();
            audio_state.finish_first_render_failure(epoch);
        });

        claimed.wait();
        assert_eq!(state.lifecycle(), SpatialShellLifecycle::Starting);
        assert_eq!(state.public_name(), "prepared_not_started");
        state.note_control_update();
        release.wait();
        audio.join().unwrap();
        let failed = state.snapshot();
        assert_eq!(failed.lifecycle, SpatialShellLifecycle::BoundUnprepared);
        assert_eq!(failed.control_update_epoch, 1);

        let state = Arc::new(SpatialLifecycleState::new(
            SpatialShellLifecycle::PreparedNotStarted,
        ));
        let claimed = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let audio_state = Arc::clone(&state);
        let audio_claimed = Arc::clone(&claimed);
        let audio_release = Arc::clone(&release);
        let audio = std::thread::spawn(move || {
            assert_eq!(audio_state.claim_first_render(), Ok(Some(0)));
            audio_claimed.wait();
            audio_release.wait();
            audio_state.finish_first_render_success();
        });

        claimed.wait();
        state.note_control_update();
        release.wait();
        audio.join().unwrap();
        let advanced = state.snapshot();
        assert_eq!(advanced.lifecycle, SpatialShellLifecycle::Running);
        assert_eq!(advanced.control_update_epoch, 1);
    }

    #[test]
    fn v2_source_config_reads_the_header_before_the_current_prefix() {
        let session = fake_session_with_route_and_source_count(SessionRoute::NeutralSpatial, 2);
        let short = ShortV2Header {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: size_of::<ShortV2Header>() as u32,
        };
        // Safety: exactly the universal header is readable. Its declared size
        // deliberately rejects a current source-config copy.
        assert_eq!(
            unsafe {
                fb_session_configure_source_v2(
                    session,
                    (&short as *const ShortV2Header).cast::<FbSourceProgramConfigV2>(),
                )
            },
            FbResult::FbInvalidArgument
        );

        let invalid_reserved = FbSourceProgramConfigV2 {
            reserved: [0, 1, 0, 0],
            ..FbSourceProgramConfigV2::default()
        };
        // Safety: the invalid current-size config is fully readable.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &invalid_reserved) },
            FbResult::FbInvalidArgument
        );

        // Neither rejected call consumed the one-shot source slot.
        let valid = FbSourceProgramConfigV2::default();
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &valid) },
            FbResult::FbOk
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_direct_success_path_failure_commits_matching_activity_before_error() {
        const BLOCK_SIZE: usize = 4;
        let initial = default_simulation_update();
        let mut update = initial;
        update.sources[0] = SourceMotion {
            active: true,
            pose: Pose {
                position: EnuVector3::new(7.0, 8.0, 9.0),
                ..default_pose()
            },
            linear_velocity_mps: EnuVector3::default(),
        };
        let (mut writer, mut reader) =
            SnapshotPublication::new(propagation_snapshot_from_update(&initial, 0, 0));
        let mut runner = DirectThenPathFailure::default();
        let mut update_sequence = 3;

        assert_eq!(
            advance_spatial_simulation_phases(
                &mut runner,
                &update,
                &mut update_sequence,
                &mut writer,
                128,
                48_000,
            ),
            FbResult::FbBackendError
        );
        assert!(runner.observed_active);
        assert_eq!(runner.direct_calls, 1);
        assert_eq!(runner.pathing_calls, 1);
        assert_eq!(runner.reflection_calls, 0);
        assert_eq!(update_sequence, 4);
        assert_eq!(runner.latest_spatial_direct_sequence(), 1);

        let committed = reader.read();
        assert_eq!(committed.sequence, runner.latest_spatial_direct_sequence());
        assert!(committed.sources[0].active);

        // RuntimeGraph's next callback reads the committed snapshot. Omitting
        // source zero must therefore be diagnosed as a missing active program;
        // stale pre-direct activity would incorrectly admit the empty block.
        let shape = SpatialSourceShape {
            channel_count: 1,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryPointV2 as u32,
            multipoint_count: 0,
            extent_m: 0.0,
            presentation_provenance: SpatialPresentationProvenance::NativeMono,
        };
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            EngineConfig {
                block_size_frames: BLOCK_SIZE as u32,
                max_active_sources: 1,
                ..EngineConfig::default()
            },
            reader,
            &[1],
            Box::new(FailingSpatialBackend),
        )
        .unwrap();
        graph
            .set_source(
                0,
                &source_profile_for_spatial_shape(0, shape, ReferenceLevel::CreativeDb { db: 0.0 })
                    .unwrap(),
                SceneCalibration::default(),
            )
            .unwrap();
        let mut presentation = [5.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [6.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut metadata = SpatialOutputMetadata::default();
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: frame_time_ns(128, 48_000),
                block_start_frame: 128,
                sources: &[],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environmental,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::MissingActiveProgram { source_index: 0 })
        );
        assert!(presentation.iter().all(|sample| *sample == 5.0));
        assert!(environmental.iter().all(|sample| *sample == 6.0));
        assert_eq!(metadata, SpatialOutputMetadata::default());
    }

    #[test]
    fn batched_direct_failure_commits_control_without_a_tick_and_retry_recovers() {
        let initial = default_simulation_update();
        let mut next = initial;
        next.sources[0].active = true;
        let lifecycle = SpatialLifecycleState::new(SpatialShellLifecycle::PreparedNotStarted);
        let (mut writer, mut reader) =
            SnapshotPublication::new(propagation_snapshot_from_update(&initial, 0, 0));
        let mut runner = DirectFailureOnce {
            observed_active: false,
            direct_calls: 0,
            direct_sequence: 0,
            failures_remaining: 1,
        };
        let mut update = initial;
        let mut update_sequence = 0;
        let mut listener_published = false;
        let mut source_published = [false; MAX_ACTIVE_SOURCES];
        let mut batched_frame_advances = 0;

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                128,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbBackendError
        );
        assert_eq!(update, next);
        assert!(runner.observed_active);
        assert_eq!(runner.direct_calls, 1);
        assert_eq!(runner.direct_sequence, 0);
        assert_eq!(update_sequence, 0);
        assert_eq!(batched_frame_advances, 0);
        assert!(!listener_published);
        assert!(!source_published[0]);
        assert!(!reader.read().sources[0].active);
        let failed = lifecycle.snapshot();
        assert_eq!(failed.lifecycle, SpatialShellLifecycle::BoundUnprepared);
        assert_eq!(failed.control_update_epoch, 1);

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                256,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbOk
        );
        assert_eq!(runner.direct_calls, 2);
        assert_eq!(runner.direct_sequence, 1);
        assert_eq!(update_sequence, 1);
        assert_eq!(batched_frame_advances, 1);
        assert!(listener_published);
        assert!(source_published[0]);
        let published = reader.read();
        assert_eq!(published.sequence, 1);
        assert!(published.sources[0].active);
        let recovered = lifecycle.snapshot();
        assert_eq!(recovered.lifecycle, SpatialShellLifecycle::BoundUnprepared);
        assert_eq!(recovered.control_update_epoch, 2);
    }

    #[test]
    fn batched_path_failure_commits_one_tick_and_is_not_immediately_retried() {
        let initial = default_simulation_update();
        let mut next = initial;
        next.sources[0] = SourceMotion {
            active: true,
            pose: Pose {
                position: EnuVector3::new(7.0, 8.0, 9.0),
                ..default_pose()
            },
            linear_velocity_mps: EnuVector3::default(),
        };
        let lifecycle = SpatialLifecycleState::new(SpatialShellLifecycle::PreparedNotStarted);
        let (mut writer, mut reader) =
            SnapshotPublication::new(propagation_snapshot_from_update(&initial, 0, 0));
        let mut runner = DirectThenPathFailure::default();
        let mut update = initial;
        let mut update_sequence = 3;
        let mut listener_published = false;
        let mut source_published = [false; MAX_ACTIVE_SOURCES];
        let mut batched_frame_advances = 0;

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                128,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbBackendError
        );
        assert_eq!(update, next);
        assert_eq!(update_sequence, 4);
        assert_eq!(batched_frame_advances, 1);
        assert!(!listener_published);
        assert!(!source_published[0]);
        let after_failure = lifecycle.snapshot();
        assert_eq!(
            after_failure.lifecycle,
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(after_failure.control_update_epoch, 1);
        assert!(reader.read().sources[0].active);
        assert_eq!(runner.pathing_calls, 1);

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                256,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbOk
        );
        assert_eq!(update_sequence, 5);
        assert_eq!(batched_frame_advances, 2);
        assert_eq!(
            runner.pathing_calls, 1,
            "a failed tick is not retried early"
        );
        assert!(listener_published);
        assert!(source_published[0]);
        assert_eq!(lifecycle.snapshot().control_update_epoch, 2);
    }

    #[test]
    fn batched_reflection_failure_commits_one_tick_and_is_not_immediately_retried() {
        let initial = default_simulation_update();
        let mut next = initial;
        next.sources[0].active = true;
        let lifecycle = SpatialLifecycleState::new(SpatialShellLifecycle::Running);
        let (mut writer, mut reader) =
            SnapshotPublication::new(propagation_snapshot_from_update(&initial, 0, 0));
        let mut runner = DirectThenReflectionFailure::default();
        let mut update = initial;
        let mut update_sequence = 11;
        let mut listener_published = false;
        let mut source_published = [false; MAX_ACTIVE_SOURCES];
        let mut batched_frame_advances = 0;

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                0,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbBackendError
        );
        assert_eq!(update_sequence, 12);
        assert_eq!(batched_frame_advances, 1);
        assert_eq!(runner.pathing_calls, 1);
        assert_eq!(runner.reflection_calls, 1);
        assert!(!listener_published);
        assert!(!source_published[0]);
        assert!(reader.read().sources[0].active);

        assert_eq!(
            commit_spatial_control_frame_phases(
                &lifecycle,
                &mut runner,
                &mut update,
                next,
                &mut update_sequence,
                &mut writer,
                0,
                48_000,
                &mut listener_published,
                &mut source_published,
                1,
                &mut batched_frame_advances,
            ),
            FbResult::FbOk
        );
        assert_eq!(update_sequence, 13);
        assert_eq!(batched_frame_advances, 2);
        assert_eq!(
            runner.reflection_calls, 1,
            "a failed tick is not retried early"
        );
        assert!(listener_published);
        assert!(source_published[0]);
        let after = lifecycle.snapshot();
        assert_eq!(after.lifecycle, SpatialShellLifecycle::Running);
        assert_eq!(after.control_update_epoch, 2);
    }

    #[test]
    fn legacy_direct_failure_retains_its_independent_published_snapshots() {
        let initial = default_simulation_update();
        let listener = initial.listener.pose;
        let (mut orientation_writer, mut orientation_reader) =
            SnapshotPublication::new(ListenerOrientation {
                forward: listener.forward,
                up: listener.up,
            });
        let (mut active_writer, mut active_reader) =
            SnapshotPublication::new([false; MAX_ACTIVE_SOURCES]);
        let mut next = initial;
        next.listener.pose.forward = EnuVector3::new(1.0, 0.0, 0.0);
        next.sources[0].active = true;
        let mut update = initial;
        publish_legacy_control_frame(
            &mut update,
            &mut orientation_writer,
            &mut active_writer,
            next,
        );
        let mut runner = LegacyDirectFailure::default();
        let mut update_sequence = 0;
        assert_eq!(
            advance_legacy_simulation_phases(&mut runner, &update, &mut update_sequence),
            FbResult::FbBackendError
        );
        assert_eq!(update, next);
        assert_eq!(update_sequence, 0);
        assert!(runner.observed_active);
        assert_eq!(runner.direct_calls, 1);

        // These remain intentionally independent publications. The assertions
        // prove retained legacy behavior, not callback-atomic correlation.
        assert_eq!(
            orientation_reader.read().forward,
            EnuVector3::new(1.0, 0.0, 0.0)
        );
        assert!(active_reader.read()[0]);
    }

    #[test]
    fn v2_final_bind_failure_rolls_back_and_supported_retry_reaches_ready() {
        const BLOCK_SIZE: usize = 4;
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames: BLOCK_SIZE as u32,
            source_count: 2,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        let inner = unsafe { session_ref(session) }.unwrap();

        let point = FbSourceProgramConfigV2::default();
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &point) },
            FbResult::FbOk
        );
        let build_fingerprint = {
            let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
            let build = spatial.build_inputs.as_ref().unwrap();
            assert_eq!(
                inner.spatial_lifecycle.lifecycle(),
                SpatialShellLifecycle::Collecting
            );
            assert_eq!(spatial.configured_count, 1);
            assert!(spatial.configured[0]);
            assert!(!spatial.configured[1]);
            assert_eq!(
                spatial.source_shapes[0].source_geometry,
                FbSourceGeometryV2::FbSourceGeometryPointV2 as u32
            );
            (
                build.mesh.vertices_enu_m.len(),
                build.mesh.triangles.len(),
                build.baked.bytes.len(),
                build.baked.metadata.content_sha256.clone(),
            )
        };

        // MonoExpanded is a frozen transport shape whose production renderer
        // is deliberately unavailable in Wave 0. As the final source it
        // exercises failure after staging and after taking build ownership.
        let unsupported_final = FbSourceProgramConfigV2 {
            source_index: 1,
            channel_count: 1,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
            extent_m: 2.0,
            ..FbSourceProgramConfigV2::default()
        };
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &unsupported_final) },
            FbResult::FbBackendUnavailable
        );
        assert!(unsafe { &*inner.spatial_control.get() }.is_none());
        assert!(unsafe { &*inner.spatial_render.get() }.is_none());
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        {
            let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
            let build = spatial.build_inputs.as_ref().unwrap();
            assert_eq!(
                inner.spatial_lifecycle.lifecycle(),
                SpatialShellLifecycle::Collecting
            );
            assert_eq!(spatial.configured_count, 1);
            assert!(spatial.configured[0]);
            assert!(!spatial.configured[1]);
            assert_eq!(spatial.source_shapes[0].channel_count, 1);
            assert_eq!(spatial.source_shapes[1].channel_count, 0);
            assert_eq!(
                (
                    build.mesh.vertices_enu_m.len(),
                    build.mesh.triangles.len(),
                    build.baked.bytes.len(),
                    build.baked.metadata.content_sha256.clone(),
                ),
                build_fingerprint
            );
        }
        // The previously accepted source remains consumed; only the failed
        // final slot is retryable.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &point) },
            FbResult::FbInvalidState
        );

        let supported_final = FbSourceProgramConfigV2 {
            source_index: 1,
            channel_count: 2,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
            extent_m: 2.0,
            ..FbSourceProgramConfigV2::default()
        };
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &supported_final) },
            FbResult::FbOk
        );
        {
            let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
            assert_eq!(
                inner.spatial_lifecycle.lifecycle(),
                SpatialShellLifecycle::BoundUnprepared
            );
            assert_eq!(spatial.configured_count, 2);
            assert_eq!(spatial.configured_channels()[..2], [1, 2]);
            assert!(spatial.build_inputs.is_none());
        }
        assert!(unsafe { &*inner.spatial_control.get() }.is_some());
        assert!(unsafe { &*inner.spatial_render.get() }.is_some());

        let listener = default_ffi_pose_at(0.0, 0.0, 0.0);
        let velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        for source_index in 0..2 {
            let source = FbSourceUpdate {
                active: 1,
                pose: default_ffi_pose_at(1.0 + source_index as f32, 2.0, 0.0),
                linear_velocity_mps: velocity,
            };
            assert_eq!(
                unsafe { fb_session_update_source(session, source_index, &source) },
                FbResult::FbOk
            );
        }
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );

        let mono = [0.125_f32; BLOCK_SIZE];
        let stereo = [0.25_f32; 2 * BLOCK_SIZE];
        let programs = [
            FbSourceProgramInputV2 {
                source_index: 0,
                channel_count: 1,
                samples: mono.as_ptr(),
                sample_count: mono.len(),
                channel_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            FbSourceProgramInputV2 {
                source_index: 1,
                channel_count: 2,
                samples: stereo.as_ptr(),
                sample_count: stereo.len(),
                channel_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
        ];
        let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: programs.as_ptr(),
            source_program_count: programs.len() as u32,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert_eq!(metadata.active_presentation_feed_count, 3);
        assert_eq!(metadata.environmental_channel_count, 9);
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_prepare_publishes_current_generation_without_consuming_frame_or_timing() {
        const BLOCK_SIZE: usize = 4;
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames: BLOCK_SIZE as u32,
            source_count: 1,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &FbSourceProgramConfigV2::default()) },
            FbResult::FbOk
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );

        let construction_sequence = unsafe { &*inner.spatial_control.get() }
            .as_ref()
            .unwrap()
            .runner
            .latest_direct_sequence();
        let listener = default_ffi_pose_at(0.0, 0.0, 0.0);
        let velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        let inactive_source = FbSourceUpdate {
            active: 0,
            pose: default_ffi_pose_at(1.0, 2.0, 0.0),
            linear_velocity_mps: velocity,
        };
        assert_eq!(
            unsafe { fb_session_update_source(session, 0, &inactive_source) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        let prepared_sequence = unsafe { &*inner.spatial_control.get() }
            .as_ref()
            .unwrap()
            .runner
            .latest_direct_sequence();
        assert!(prepared_sequence > construction_sequence);
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        let before_render = read_session_telemetry(session);
        assert_eq!(before_render["timing_ns"]["observations"], 0);
        assert_eq!(
            before_render["preparation"]["status"],
            "prepared_not_started"
        );
        assert_eq!(before_render["preparation"]["attempts"], 1);
        assert_eq!(before_render["preparation"]["successes"], 1);
        assert_eq!(before_render["preparation"]["failures"], 0);
        assert!(
            before_render["preparation"]["latest_duration_ns"]
                .as_u64()
                .is_some_and(|duration| duration > 0)
        );

        let mut direct = [7.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [11.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: ptr::null(),
            source_program_count: 0,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert!(metadata.generation > 0);
        assert_eq!(metadata.discontinuity_sequence, 0);
        assert_ne!(metadata.flags & FB_SPATIAL_BLOCK_VALID_V2, 0);
        assert_eq!(metadata.flags & FB_SPATIAL_BLOCK_DISCONTINUITY_V2, 0);
        assert_eq!(metadata.block_start_frame, 0);
        assert_eq!(metadata.active_presentation_feed_count, 0);
        assert_eq!(metadata.environmental_channel_count, 9);
        assert!(direct.iter().all(|sample| sample.is_finite()));
        assert!(environmental.iter().all(|sample| sample.is_finite()));
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::Running
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn invalid_control_frames_are_total_noops_for_neutral_state_and_preparation() {
        let session = create_configured_neutral_point_session(4, 2);
        let listener = default_ffi_pose_at(1.0, 0.0, 0.0);
        let velocity = FbVec3::default();
        let sources = [source_update_at(true, 10.0), source_update_at(false, 20.0)];
        let valid = control_frame_for(listener, velocity, &sources);
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &valid) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        let lifecycle_before = inner.spatial_lifecycle.snapshot();
        assert_eq!(
            lifecycle_before.lifecycle,
            SpatialShellLifecycle::PreparedNotStarted
        );
        let (update_before, sequence_before, counters_before, published_before) = {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            (
                control.update,
                control.update_sequence,
                (
                    control.batched_frame_advances,
                    control.granular_listener_advances,
                    control.granular_source_advances,
                ),
                (control.listener_published, control.source_published),
            )
        };

        let mut invalid_listener = control_frame_for(listener, velocity, &sources);
        invalid_listener.listener_pose.position.east_m = f32::NAN;
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &invalid_listener) },
            FbResult::FbInvalidArgument
        );

        let mut invalid_last_sources = sources;
        invalid_last_sources[1].linear_velocity_mps.north_m = f32::INFINITY;
        let invalid_last = control_frame_for(listener, velocity, &invalid_last_sources);
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &invalid_last) },
            FbResult::FbInvalidArgument
        );
        let short = ShortV2Header {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: size_of::<ShortV2Header>() as u32,
        };
        assert_eq!(
            unsafe {
                fb_session_update_control_frame_v2(
                    session,
                    (&short as *const ShortV2Header).cast::<FbControlFrameV2>(),
                )
            },
            FbResult::FbInvalidArgument
        );
        let mut invalid_reserved = valid;
        invalid_reserved.reserved[3] = 1;
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &invalid_reserved) },
            FbResult::FbInvalidArgument
        );
        let mut outer_storage = [0_u8; size_of::<FbControlFrameV2>() + 8];
        let outer_base = outer_storage.as_mut_ptr();
        let outer_offset = (0..8)
            .find(|offset| {
                !outer_base
                    .wrapping_add(*offset)
                    .addr()
                    .is_multiple_of(align_of::<FbControlFrameV2>())
            })
            .unwrap();
        let misaligned_outer = outer_base.wrapping_add(outer_offset);
        unsafe {
            ptr::write_unaligned(
                misaligned_outer.cast::<ShortV2Header>(),
                ShortV2Header {
                    abi_version: FB_ABI_VERSION_V2,
                    struct_size: size_of::<FbControlFrameV2>() as u32,
                },
            );
        }
        assert_eq!(
            unsafe {
                fb_session_update_control_frame_v2(
                    session,
                    misaligned_outer.cast::<FbControlFrameV2>(),
                )
            },
            FbResult::FbInvalidArgument
        );

        let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
        assert_eq!(control.update, update_before);
        assert_eq!(control.update_sequence, sequence_before);
        assert_eq!(
            (
                control.batched_frame_advances,
                control.granular_listener_advances,
                control.granular_source_advances,
            ),
            counters_before
        );
        assert_eq!(
            (control.listener_published, control.source_published),
            published_before
        );
        assert_eq!(inner.spatial_lifecycle.snapshot(), lifecycle_before);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v1_legacy_batch_consumes_one_tick_and_invalid_frames_publish_nothing() {
        let session = create_v1_legacy_session(4, 2);
        let inner = unsafe { session_ref(session) }.unwrap();
        let diagnostics_before = {
            let control = unsafe { &*inner.control.get() }.as_ref().unwrap();
            control.runner.world_diagnostics().vendor_pass_runs
        };
        let listener = default_ffi_pose_at(3.0, 4.0, 5.0);
        let velocity = FbVec3::default();
        let sources = [source_update_at(true, 11.0), source_update_at(false, 22.0)];
        let valid = control_frame_for(listener, velocity, &sources);
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &valid) },
            FbResult::FbOk
        );
        let (update_before, sequence_before, diagnostics_after) = {
            let control = unsafe { &*inner.control.get() }.as_ref().unwrap();
            (
                control.update,
                control.update_sequence,
                control.runner.world_diagnostics().vendor_pass_runs,
            )
        };
        assert_eq!(sequence_before, 1);
        assert_eq!(diagnostics_after[0], diagnostics_before[0] + 1);
        assert_eq!(diagnostics_after[1], diagnostics_before[1]);
        assert_eq!(diagnostics_after[2], diagnostics_before[2]);
        assert_eq!(
            update_before.listener.pose.position,
            EnuVector3::new(3.0, 4.0, 5.0)
        );
        assert!(update_before.sources[0].active);
        assert!(!update_before.sources[1].active);

        let (orientation_before, activity_before) = {
            let render = unsafe { &mut *inner.render.get() }.as_mut().unwrap();
            (
                render.orientation_reader.read(),
                render.active_sources_reader.read(),
            )
        };
        let mut invalid_listener = control_frame_for(listener, velocity, &sources);
        invalid_listener.listener_pose.up.up_m = f32::NAN;
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &invalid_listener) },
            FbResult::FbInvalidArgument
        );
        let mut invalid_last_sources = sources;
        invalid_last_sources[1].pose.forward.north_m = f32::NAN;
        let invalid_last = control_frame_for(listener, velocity, &invalid_last_sources);
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &invalid_last) },
            FbResult::FbInvalidArgument
        );
        let control = unsafe { &*inner.control.get() }.as_ref().unwrap();
        assert_eq!(control.update, update_before);
        assert_eq!(control.update_sequence, sequence_before);
        assert_eq!(
            control.runner.world_diagnostics().vendor_pass_runs,
            diagnostics_after
        );
        let render = unsafe { &mut *inner.render.get() }.as_mut().unwrap();
        assert_eq!(
            render.orientation_reader.read().forward,
            orientation_before.forward
        );
        assert_eq!(render.orientation_reader.read().up, orientation_before.up);
        assert_eq!(render.active_sources_reader.read(), activity_before);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn neutral_batch_uses_one_tick_per_frame_and_preserves_twelve_tick_cadence() {
        #[repr(C)]
        struct ExtendedSourceUpdate {
            current: FbSourceUpdate,
            extension: [u32; 3],
        }

        let session = create_configured_neutral_point_session(4, 1);
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState,
            "the batch is the complete initial publication prerequisite"
        );
        let diagnostics_before = {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            control.runner.world_diagnostics().vendor_pass_runs
        };
        let listener = default_ffi_pose_at(0.0, 0.0, 1.5);
        let velocity = FbVec3::default();
        let extended_sources = [ExtendedSourceUpdate {
            current: source_update_at(true, 12.0),
            extension: [0x1122_3344, 0x5566_7788, 0x99aa_bbcc],
        }];
        let extended = FbControlFrameV2 {
            source_updates: extended_sources.as_ptr().cast::<FbSourceUpdate>(),
            source_count: 1,
            source_update_stride_bytes: size_of::<ExtendedSourceUpdate>() as u32,
            listener_pose: listener,
            listener_linear_velocity_mps: velocity,
            ..FbControlFrameV2::default()
        };
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &extended) },
            FbResult::FbOk
        );
        {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, 1);
            assert_eq!(control.batched_frame_advances, 1);
            assert_eq!(control.update.sources[0].pose.position.east_m, 12.0);
            let diagnostics = control.runner.world_diagnostics().vendor_pass_runs;
            assert_eq!(diagnostics[0], diagnostics_before[0] + 1);
            assert_eq!(diagnostics[1], diagnostics_before[1]);
            assert_eq!(diagnostics[2], diagnostics_before[2]);
        }

        let exact_sources = [source_update_at(true, 13.0)];
        let exact = control_frame_for(listener, velocity, &exact_sources);
        for _ in 1..12 {
            assert_eq!(
                unsafe { fb_session_update_control_frame_v2(session, &exact) },
                FbResult::FbOk
            );
        }
        {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, 12);
            assert_eq!(control.batched_frame_advances, 12);
            assert_eq!(control.granular_listener_advances, 0);
            assert_eq!(control.granular_source_advances, 0);
            let diagnostics = control.runner.world_diagnostics().vendor_pass_runs;
            assert_eq!(diagnostics[0], diagnostics_before[0] + 12);
            assert_eq!(diagnostics[1], diagnostics_before[1] + 3);
            assert_eq!(diagnostics[2], diagnostics_before[2] + 1);
        }
        let before_prepare = inner.spatial_lifecycle.snapshot();
        assert_eq!(before_prepare.control_update_epoch, 12);
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        let prepared = inner.spatial_lifecycle.snapshot();
        assert_eq!(
            prepared.lifecycle,
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(prepared.control_update_epoch, 12);

        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &exact) },
            FbResult::FbOk
        );
        let invalidated = inner.spatial_lifecycle.snapshot();
        assert_eq!(
            invalidated.lifecycle,
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(invalidated.control_update_epoch, 13);
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn neutral_control_schedule_telemetry_attributes_batch_and_granular_ticks() {
        let session = create_bound_active_point_session(4);
        let listener = default_ffi_pose_at(0.5, 0.0, 0.0);
        let velocity = FbVec3::default();
        let sources = [source_update_at(true, 4.0)];
        let frame = control_frame_for(listener, velocity, &sources);
        assert_eq!(
            unsafe { fb_session_update_control_frame_v2(session, &frame) },
            FbResult::FbOk
        );
        let telemetry = read_session_telemetry(session);
        let schedule = &telemetry["control_schedule"];
        assert_eq!(schedule["owner"], "ffi_caller_control_thread");
        assert_eq!(schedule["execution"], "synchronous");
        assert_eq!(
            schedule["cadence_basis"],
            "successful_direct_control_advance"
        );
        assert_eq!(schedule["cadence_advances"], 3);
        assert_eq!(schedule["batched_frame_advances"], 1);
        assert_eq!(schedule["granular_listener_advances"], 1);
        assert_eq!(schedule["granular_source_advances"], 1);
        assert_eq!(schedule["pathing_every_n_advances"], 4);
        assert_eq!(schedule["base_reflections_every_n_advances"], 12);
        assert_eq!(schedule["worker_busy_feedback"], "not_applicable");
        assert_eq!(schedule["interval_lateness_policy"], "diagnostic_only");
        assert_eq!(schedule["pass_overrun_policy"], "actionable");
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_construction_seed_preserves_mobile_reflection_cadence_and_governor_timing() {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            source_count: 1,
            quality_tier: FbQualityTier::FbQualityMobile as u32,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &FbSourceProgramConfigV2::default()) },
            FbResult::FbOk
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        let reflection_runs_after_seed = {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            let telemetry = control.runner.quality_governor_telemetry().unwrap();
            assert_eq!(control.update_sequence, 0);
            assert_eq!(telemetry.p50_ns, 0);
            assert_eq!(telemetry.p95_ns, 0);
            assert_eq!(telemetry.p99_ns, 0);
            assert_eq!(telemetry.p99_9_ns, 0);
            assert_eq!(telemetry.callback_deadline_misses, 0);
            assert_eq!(telemetry.simulation_lateness_ns, [0; 3]);
            assert_eq!(telemetry.reflections.cadence_divisor, 2);
            control.runner.world_diagnostics().vendor_pass_runs[2]
        };
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );

        let listener = default_ffi_pose_at(0.0, 0.0, 0.0);
        let velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        let source = FbSourceUpdate {
            active: 1,
            pose: default_ffi_pose_at(1.0, 2.0, 0.0),
            linear_velocity_mps: velocity,
        };
        assert_eq!(
            unsafe { fb_session_update_source(session, 0, &source) },
            FbResult::FbOk
        );
        for _ in 0..9 {
            assert_eq!(
                unsafe { fb_session_update_listener(session, &listener, &velocity) },
                FbResult::FbOk
            );
        }
        {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, 11);
            assert_eq!(
                control.runner.world_diagnostics().vendor_pass_runs[2],
                reflection_runs_after_seed,
                "construction must not consume mobile reflection cadence tick zero"
            );
        }
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, 12);
            assert_eq!(
                control.runner.world_diagnostics().vendor_pass_runs[2],
                reflection_runs_after_seed + 1,
                "the first ordinary reflection cadence call must remain due"
            );
        }
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_control_prepare_hands_first_public_render_to_distinct_audio_thread() {
        const BLOCK_SIZE: usize = 4;
        let session = create_bound_active_point_session(BLOCK_SIZE as u32);
        let control_thread = std::thread::current().id();
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        let before_render = read_session_telemetry(session);
        assert_eq!(
            before_render["preparation"]["status"],
            "prepared_not_started"
        );
        assert_eq!(before_render["timing_ns"]["observations"], 0);
        assert_eq!(
            before_render["timing_ns"]["callback_local_run"]["observations"],
            0
        );

        // The opaque C handle crosses the test handoff as an address. The
        // control role remains quiescent until the one audio-thread call joins.
        let session_address = session.expose_provenance();
        let (audio_thread, result, metadata) = std::thread::spawn(move || {
            let audio_thread = std::thread::current().id();
            let session = ptr::with_exposed_provenance_mut::<FbSession>(session_address);
            let input = [0.25_f32; BLOCK_SIZE];
            let program = [FbSourceProgramInputV2 {
                source_index: 0,
                channel_count: 1,
                samples: input.as_ptr(),
                sample_count: input.len(),
                channel_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            }];
            let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
            let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
            let mut feeds =
                [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
            let mut metadata = FbSpatialBlockMetadataV2::default();
            let block = FbSpatialRenderBlockV2 {
                source_programs: program.as_ptr(),
                source_program_count: 1,
                direct_output: FbPlanarOutputV2 {
                    samples: direct.as_mut_ptr(),
                    sample_capacity: direct.len(),
                    plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                environmental_output: FbPlanarOutputV2 {
                    samples: environmental.as_mut_ptr(),
                    sample_capacity: environmental.len(),
                    plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                feed_metadata: feeds.as_mut_ptr(),
                feed_metadata_capacity: feeds.len() as u32,
                block_metadata: &mut metadata,
                ..FbSpatialRenderBlockV2::default()
            };
            let result = unsafe { fb_session_render_spatial_v2(session, &block) };
            (audio_thread, result, metadata)
        })
        .join()
        .expect("audio-thread handoff must not panic");

        assert_ne!(audio_thread, control_thread);
        assert_eq!(result, FbResult::FbOk);
        assert_eq!(metadata.block_start_frame, 0);
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert_eq!(metadata.active_presentation_feed_count, 1);
        assert!(metadata.generation > 0);
        assert_eq!(metadata.discontinuity_sequence, 0);
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::Running
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        let after_render = read_session_telemetry(session);
        assert_eq!(after_render["preparation"]["status"], "running");
        assert_eq!(after_render["timing_ns"]["observations"], 1);
        assert_eq!(
            after_render["timing_ns"]["callback_local_run"]["observations"],
            1
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_updates_invalidate_preparation_and_repeat_prepare_is_allowed_before_render() {
        let session = create_ready_active_point_session(4);
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );

        // A malformed/nonadvancing call after preparation reads and rejects
        // its block but does not consume the one-shot first-render claim.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        let listener = default_ffi_pose_at(0.25, 0.0, 0.0);
        let velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener, &velocity) },
            FbResult::FbOk
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );
        // BoundUnprepared wins before the deliberately null block is read.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );

        let source = FbSourceUpdate {
            active: 1,
            pose: default_ffi_pose_at(1.25, 2.0, 0.0),
            linear_velocity_mps: velocity,
        };
        assert_eq!(
            unsafe { fb_session_update_source(session, 0, &source) },
            FbResult::FbOk
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        let telemetry = read_session_telemetry(session);
        assert_eq!(telemetry["preparation"]["status"], "prepared_not_started");
        assert_eq!(telemetry["preparation"]["attempts"], 4);
        assert_eq!(telemetry["preparation"]["successes"], 4);
        assert_eq!(telemetry["preparation"]["failures"], 0);
        assert_eq!(telemetry["timing_ns"]["observations"], 0);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_prepare_failure_rolls_back_to_unprepared_and_retry_succeeds() {
        let session = create_bound_active_point_session(4);
        let failures_remaining = Arc::new(AtomicU64::new(1));
        replace_spatial_backend_for_test(
            session,
            Box::new(PreparationProbeBackend {
                failures_remaining: Arc::clone(&failures_remaining),
            }),
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        let (sequence_before, cadence_before) = {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            (
                control.runner.latest_direct_sequence(),
                control.update_sequence,
            )
        };

        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbBackendError
        );
        let sequence_after_failure = {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, cadence_before);
            control.runner.latest_direct_sequence()
        };
        assert!(sequence_after_failure > sequence_before);
        assert_eq!(failures_remaining.load(Ordering::Acquire), 0);
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::BoundUnprepared
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        let failed = read_session_telemetry(session);
        assert_eq!(failed["preparation"]["status"], "bound_unprepared");
        assert_eq!(failed["preparation"]["attempts"], 1);
        assert_eq!(failed["preparation"]["successes"], 0);
        assert_eq!(failed["preparation"]["failures"], 1);
        assert_eq!(failed["timing_ns"]["observations"], 0);
        // A failed forced prepare leaves the session non-renderable until an
        // explicit retry succeeds. BoundUnprepared wins before the null block
        // can be read, and the public clock remains untouched.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        assert_eq!(
            read_session_telemetry(session)["timing_ns"]["observations"],
            0
        );

        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );
        {
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(control.update_sequence, cadence_before);
            assert!(control.runner.latest_direct_sequence() > sequence_after_failure);
        }
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        let recovered = read_session_telemetry(session);
        assert_eq!(recovered["preparation"]["attempts"], 2);
        assert_eq!(recovered["preparation"]["successes"], 1);
        assert_eq!(recovered["preparation"]["failures"], 1);
        assert_eq!(recovered["timing_ns"]["observations"], 0);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_first_runtime_rejection_preserves_prepared_claim_and_public_clock() {
        const BLOCK_SIZE: usize = 4;
        let session = create_ready_active_point_session(BLOCK_SIZE as u32);
        let inner = unsafe { session_ref(session) }.unwrap();
        let input = [0.25_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let mut direct = [7.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [11.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let mut block = FbSpatialRenderBlockV2 {
            source_programs: ptr::null(),
            source_program_count: 0,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        // FFI structure validation admits an empty program list; RuntimeGraph
        // rejects the missing active source after the Starting claim. That
        // nonadvancing failure must return the claim to PreparedNotStarted.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbInvalidArgument
        );
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::PreparedNotStarted
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        assert!(direct.iter().all(|sample| *sample == 7.0));
        assert!(environmental.iter().all(|sample| *sample == 11.0));
        let after_rejection = read_session_telemetry(session);
        assert_eq!(after_rejection["timing_ns"]["observations"], 0);

        block.source_programs = program.as_ptr();
        block.source_program_count = 1;
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_eq!(metadata.block_start_frame, 0);
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert!(metadata.generation > 0);
        assert_eq!(metadata.discontinuity_sequence, 0);
        assert_ne!(metadata.flags & FB_SPATIAL_BLOCK_VALID_V2, 0);
        assert_eq!(metadata.flags & FB_SPATIAL_BLOCK_DISCONTINUITY_V2, 0);
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::Running
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn public_v2_neutral_binding_renders_exact_feed_and_environment_metadata() {
        let (package, bake) = chicago_fixture_c_paths();
        let config = FbSessionConfigV2 {
            block_size_frames: 4,
            source_count: 2,
            ..FbSessionConfigV2::default()
        };
        let mut session = ptr::null_mut();
        // Safety: the config, paths, and output slot are live for this call.
        assert_eq!(
            unsafe { fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session) },
            FbResult::FbOk
        );
        assert!(!session.is_null());
        // Safety: create returned one live owned handle.
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(inner.route, SessionRoute::NeutralSpatial);
        let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
        assert!(spatial.build_inputs.is_some());
        assert_eq!(
            inner.spatial_lifecycle.lifecycle(),
            SpatialShellLifecycle::Collecting
        );

        // The public shell is externally reachable but cannot render before
        // every immutable source shape is configured.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        let source_zero = FbSourceProgramConfigV2::default();
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_zero) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );

        let source_one = FbSourceProgramConfigV2 {
            source_index: 1,
            channel_count: 2,
            source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
            extent_m: 2.0,
            ..FbSourceProgramConfigV2::default()
        };
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_one) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        let spatial = unsafe { &*inner.spatial.get() }.as_ref().unwrap();
        assert_eq!(
            spatial.source_shapes[0].presentation_provenance,
            SpatialPresentationProvenance::NativeMono
        );
        assert_eq!(
            spatial.source_shapes[1].presentation_provenance,
            SpatialPresentationProvenance::AuthoredStereo
        );
        assert_eq!(
            SpatialPresentationProvenance::infer(
                FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
                1,
            ),
            SpatialPresentationProvenance::MonoExpanded
        );

        let listener_pose = FbPose {
            position: FbVec3::default(),
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
        };
        let zero_velocity = FbVec3::default();
        assert_eq!(
            unsafe { fb_session_update_listener(session, &listener_pose, &zero_velocity) },
            FbResult::FbOk
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbInvalidState
        );
        for source_index in 0..2 {
            let source_update = FbSourceUpdate {
                active: 1,
                pose: FbPose {
                    position: FbVec3 {
                        east_m: 1.0 + source_index as f32,
                        north_m: 2.0,
                        up_m: 0.0,
                    },
                    ..listener_pose
                },
                linear_velocity_mps: zero_velocity,
            };
            assert_eq!(
                unsafe { fb_session_update_source(session, source_index as u32, &source_update) },
                FbResult::FbOk
            );
            if source_index == 0 {
                assert_eq!(
                    unsafe { fb_session_prepare_spatial_v2(session) },
                    FbResult::FbInvalidState
                );
            }
        }
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, ptr::null()) },
            FbResult::FbInvalidState
        );
        assert_eq!(
            unsafe { fb_session_prepare_spatial_v2(session) },
            FbResult::FbOk
        );

        let mono = [0.25_f32; 4];
        let stereo = [0.5_f32; 8];
        let programs = [
            FbSourceProgramInputV2 {
                source_index: 0,
                channel_count: 1,
                samples: mono.as_ptr(),
                sample_count: mono.len(),
                channel_stride_samples: mono.len(),
                reserved: [0; 4],
            },
            FbSourceProgramInputV2 {
                source_index: 1,
                channel_count: 2,
                samples: stereo.as_ptr(),
                sample_count: stereo.len(),
                channel_stride_samples: 4,
                reserved: [0; 4],
            },
        ];
        let mut direct = [7.0_f32; FB_MAX_PRESENTATION_FEEDS_V2 as usize * 4];
        let mut environmental = [11.0_f32; FB_MAX_ENVIRONMENTAL_CHANNELS_V2 as usize * 4];
        let mut feeds =
            [FbPresentationFeedMetadataV2::default(); FB_MAX_PRESENTATION_FEEDS_V2 as usize];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let mut block = FbSpatialRenderBlockV2 {
            source_programs: programs.as_ptr(),
            source_program_count: programs.len() as u32,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: 4,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: 4,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert!(direct.iter().all(|sample| sample.is_finite()));
        assert!(environmental.iter().all(|sample| sample.is_finite()));
        assert!(direct.iter().any(|sample| *sample != 7.0));
        assert!(environmental.iter().any(|sample| *sample != 11.0));
        assert_eq!(
            metadata.validity,
            FbSpatialOutputValidityV2::FbSpatialValidV2 as u32
        );
        assert_eq!(metadata.block_start_frame, 0);
        assert_eq!(metadata.active_presentation_feed_count, 3);
        assert_eq!(metadata.environmental_order, 2);
        assert_eq!(metadata.environmental_channel_count, 9);
        assert_eq!(
            metadata.environmental_basis,
            FbEnvironmentalBasisV2::FbEnvironmentalSteamXRightYUpZBackV2 as u32
        );
        assert_eq!(
            metadata.component_mask,
            FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2
                | FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2
                | FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2
        );
        assert_eq!(
            metadata.flags,
            FB_SPATIAL_BLOCK_VALID_V2
                | FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2
                | FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2
                | FB_SPATIAL_WORLD_UNROTATED_V2
                | FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2
                | FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2
                | FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2
        );
        assert_eq!(feeds[0].valid, 1);
        assert_eq!(feeds[0].source_index, 0);
        assert_eq!(
            feeds[0].component,
            FB_PRESENTATION_COMPONENT_DIRECT_CENTER_V2
        );
        assert_eq!(
            feeds[0].placement,
            FbPresentationPlacementV2::FbPresentationDirectionV2 as u32
        );
        assert_eq!(feeds[0].pose.position.east_m, 1.0);
        assert_eq!(feeds[0].pose.position.north_m, 2.0);
        assert_eq!(feeds[0].pose.position.up_m, 0.0);
        assert!((feeds[0].direction_enu.east_m - 1.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
        assert!((feeds[0].direction_enu.north_m - 2.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
        assert_eq!(feeds[0].direction_enu.up_m, 0.0);
        assert_eq!(feeds[0].processing_latency_frames, 0);
        assert_eq!(feeds[3].valid, 0);
        assert_eq!(feeds[4].valid, 1);
        assert_eq!(feeds[4].source_index, 1);
        assert_eq!(
            feeds[4].component,
            FB_PRESENTATION_COMPONENT_WIDTH_POSITIVE_V2
        );
        assert_eq!(feeds[4].pose.position.east_m, 3.0);
        assert_eq!(feeds[4].pose.position.north_m, 2.0);
        assert_eq!(
            feeds[4].placement,
            FbPresentationPlacementV2::FbPresentationDirectionV2 as u32
        );
        assert!((feeds[4].direction_enu.east_m - 3.0 / 13.0_f32.sqrt()).abs() < 1.0e-6);
        assert!((feeds[4].direction_enu.north_m - 2.0 / 13.0_f32.sqrt()).abs() < 1.0e-6);
        assert_eq!(feeds[5].valid, 1);
        assert_eq!(
            feeds[5].component,
            FB_PRESENTATION_COMPONENT_WIDTH_NEGATIVE_V2
        );
        assert_eq!(feeds[5].pose.position.east_m, 1.0);
        assert_eq!(feeds[5].pose.position.north_m, 2.0);
        assert_eq!(
            feeds[5].placement,
            FbPresentationPlacementV2::FbPresentationDirectionV2 as u32
        );
        assert!((feeds[5].direction_enu.east_m - 1.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);
        assert!((feeds[5].direction_enu.north_m - 2.0 / 5.0_f32.sqrt()).abs() < 1.0e-6);

        let direct_after_first = direct;
        let environmental_after_first = environmental;
        let metadata_after_first = metadata;
        let missing_active_block = FbSpatialRenderBlockV2 {
            source_program_count: 1,
            ..block
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &missing_active_block) },
            FbResult::FbInvalidArgument
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(direct, direct_after_first);
        assert_eq!(environmental, environmental_after_first);
        assert_eq!(
            metadata.block_start_frame,
            metadata_after_first.block_start_frame
        );

        block.abi_version = 1;
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbInvalidArgument
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(direct, direct_after_first);
        assert_eq!(environmental, environmental_after_first);
        assert_eq!(
            metadata.block_start_frame,
            metadata_after_first.block_start_frame
        );
        block.abi_version = FB_ABI_VERSION_V2;
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        assert_eq!(metadata.block_start_frame, 4);
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 8);

        let (governor, binding_memory) = {
            let control = unsafe { &mut *inner.spatial_control.get() }
                .as_mut()
                .unwrap();
            (
                control.runner.quality_governor_telemetry().unwrap(),
                control.memory,
            )
        };
        assert_eq!(binding_memory.neutral_graph.configured_source_count, 2);
        assert_eq!(
            binding_memory.neutral_graph.configured_stereo_source_count,
            1
        );
        assert_eq!(
            binding_memory
                .neutral_graph
                .stereo_indirect_suppressed_source_count,
            1
        );
        assert!(
            binding_memory
                .neutral_graph
                .additional_program_channel_payload_bytes
                <= binding_memory
                    .neutral_graph
                    .program_delay_audio_history_payload_bytes
        );
        assert!(
            binding_memory
                .neutral_graph
                .delayed_program_scratch_payload_bytes
                <= binding_memory.neutral_graph.rust_scratch_payload_bytes
        );
        assert!(binding_memory.neutral_graph.outer_vec_payload_bytes > 0);
        assert_eq!(
            governor.memory.render_scratch_bytes,
            binding_memory
                .neutral_graph
                .rust_scratch_payload_bytes
                .saturating_add(binding_memory.neutral_graph.outer_vec_payload_bytes)
        );
        let shared_propagation_kernel_bytes = governor
            .memory
            .propagation_delay_line_bytes
            .saturating_sub(
                binding_memory
                    .neutral_graph
                    .program_delay_audio_history_payload_bytes,
            )
            .saturating_sub(
                binding_memory
                    .neutral_graph
                    .program_delay_geometry_history_payload_bytes,
            );
        assert!(shared_propagation_kernel_bytes > 0);
        assert_eq!(
            governor
                .memory
                .audio_buffer_payload_bytes
                .saturating_add(governor.memory.render_scratch_bytes)
                .saturating_add(governor.memory.propagation_delay_line_bytes),
            binding_memory
                .neutral_graph
                .total_tracked_payload_bytes
                .saturating_add(shared_propagation_kernel_bytes)
        );
        assert_eq!(
            binding_memory.ffi_presentation_bank_payload_bytes,
            (MAX_SPATIAL_PRESENTATION_FEEDS * 4 * size_of::<f32>()) as u64
        );
        assert_eq!(
            binding_memory.ffi_environmental_bank_payload_bytes,
            (MAX_SPATIAL_ENVIRONMENT_PLANES * 4 * size_of::<f32>()) as u64
        );
        assert_eq!(
            binding_memory.runtime_graph.spatial_scratch_payload_bytes,
            (MAX_ACTIVE_SOURCES * 2 * 4 * size_of::<f32>()) as u64
        );
        assert_eq!(
            binding_memory.propagation_snapshot_publication_payload_bytes,
            SnapshotPublication::shared_payload_bytes::<PropagationSnapshot>()
        );
        assert_eq!(
            binding_memory.callback_timing_publication_payload_bytes,
            CallbackTimingPublication::shared_payload_bytes()
        );

        let telemetry = read_session_telemetry(session);
        assert_eq!(telemetry["preparation"]["status"], "running");
        assert_eq!(telemetry["preparation"]["attempts"], 1);
        assert_eq!(telemetry["preparation"]["successes"], 1);
        assert_eq!(telemetry["preparation"]["failures"], 0);
        let external_payload_bytes = binding_memory.external_payload_bytes();
        assert_eq!(
            telemetry["memory"]["tracked_at_create_bytes"],
            governor
                .memory
                .tracked_at_create_bytes
                .saturating_add(external_payload_bytes)
        );
        assert_eq!(
            telemetry["memory"]["tracked_current_bytes"],
            governor
                .memory
                .tracked_current_bytes
                .saturating_add(external_payload_bytes)
        );
        assert_eq!(
            telemetry["memory"]["tracked_peak_bytes"],
            governor
                .memory
                .tracked_peak_bytes
                .saturating_add(external_payload_bytes)
        );
        assert_eq!(
            telemetry["memory"]["categories"]["neutral_spatial_graph"]["total_tracked_payload_bytes"],
            binding_memory.neutral_graph.total_tracked_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["neutral_spatial_graph"]["delayed_program_scratch_payload_bytes"],
            binding_memory
                .neutral_graph
                .delayed_program_scratch_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["neutral_spatial_graph"]["outer_vec_payload_bytes"],
            binding_memory.neutral_graph.outer_vec_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["runtime_graph"]["total_payload_bytes"],
            binding_memory.runtime_graph.total_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["runtime_graph"]["spatial_scratch_payload_bytes"],
            binding_memory.runtime_graph.spatial_scratch_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["propagation_snapshot_publication"]["shared_payload_bytes"],
            binding_memory.propagation_snapshot_publication_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["categories"]["callback_timing_publication"]["shared_payload_bytes"],
            binding_memory.callback_timing_publication_payload_bytes
        );
        assert_eq!(
            telemetry["memory"]["untracked"][1]["category"],
            "v2_publication_and_runtime_overhead"
        );
        assert_eq!(telemetry["timing_ns"]["observations"], 2);
        assert_eq!(telemetry["timing_ns"]["dropped_observations"], 0);

        // Every shape is now frozen, so no later call can reinterpret it.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_one) },
            FbResult::FbInvalidState
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_neutral_callback_is_allocation_free_after_construction() {
        const BLOCK_SIZE: usize = 4;
        const MEASURED_BLOCKS: usize = 32;
        let session = create_ready_active_point_session(BLOCK_SIZE as u32);
        let input = [0.125_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let mut direct = [0.0_f32; FB_MAX_PRESENTATION_FEEDS_V2 as usize * BLOCK_SIZE];
        let mut environmental = [0.0_f32; FB_MAX_ENVIRONMENTAL_CHANNELS_V2 as usize * BLOCK_SIZE];
        let mut feeds =
            [FbPresentationFeedMetadataV2::default(); FB_MAX_PRESENTATION_FEEDS_V2 as usize];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        // Exercise the exact measured path once before enabling the test
        // allocator so one-time test harness initialization is out of scope.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        let mut result = FbResult::FbOk;
        let (allocations, ()) = count_allocations(|| {
            for _ in 0..MEASURED_BLOCKS {
                // Safety: all records and buffers remain live, disjoint, and
                // unchanged in shape for the measured callback sequence.
                result = unsafe { fb_session_render_spatial_v2(session, &block) };
                if result != FbResult::FbOk {
                    break;
                }
            }
        });
        assert_eq!(result, FbResult::FbOk);
        assert_eq!(allocations, 0);
        // Safety: the session is quiescent and uniquely destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_callback_timings_drain_without_loss_on_updates_and_telemetry() {
        // Use the production callback quantum. A four-frame debug callback has
        // an 83 us deadline and intentionally drives the governor through
        // repeated miss transitions, resetting its percentile window.
        const BLOCK_SIZE: usize = 128;
        const FIRST_BATCH: usize = 8;
        // Leave a full evidence window after any two-block quality adoption
        // caused by the preceding active-source update.
        const SECOND_BATCH: usize = 32;
        let session = create_ready_active_point_session(BLOCK_SIZE as u32);
        let input = [0.125_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        for _ in 0..FIRST_BATCH {
            assert_eq!(
                unsafe { fb_session_render_spatial_v2(session, &block) },
                FbResult::FbOk
            );
        }
        // A RuntimeGraph structural rejection enters the timing scope but is
        // discarded rather than published as a completed observation.
        let missing_active = FbSpatialRenderBlockV2 {
            source_program_count: 0,
            ..block
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &missing_active) },
            FbResult::FbInvalidArgument
        );
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(inner.last_render_ns.load(Ordering::Acquire), 0);
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        assert_eq!(control.callback_timing_run.len(), 0);
        assert_eq!(control.timing_reader.dropped_observations(), 0);

        let source = FbSourceUpdate {
            active: 1,
            pose: default_ffi_pose_at(1.0, 2.0, 0.0),
            linear_velocity_mps: FbVec3::default(),
        };
        assert_eq!(
            unsafe { fb_session_update_source(session, 0, &source) },
            FbResult::FbOk
        );
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        assert_eq!(control.callback_timing_run.len(), FIRST_BATCH as u64);
        assert_eq!(control.timing_reader.dropped_observations(), 0);

        for _ in 0..SECOND_BATCH {
            assert_eq!(
                unsafe { fb_session_render_spatial_v2(session, &block) },
                FbResult::FbOk
            );
        }
        let value = read_session_telemetry(session);
        assert_eq!(
            value["timing_ns"]["observations"],
            (FIRST_BATCH + SECOND_BATCH) as u64
        );
        assert_eq!(value["timing_ns"]["dropped_observations"], 0);
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["observations"],
            (FIRST_BATCH + SECOND_BATCH) as u64
        );
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["coverage_complete"],
            true
        );
        assert!(
            value["timing_ns"]["callback_local_run"]["p99"]
                .as_u64()
                .is_some()
        );
        assert!(
            value["timing_ns"]["callback_local_run"]["p99_9"]
                .as_u64()
                .is_some()
        );
        let repeated = read_session_telemetry(session);
        assert_eq!(
            repeated["timing_ns"]["callback_local_run"]["observations"],
            (FIRST_BATCH + SECOND_BATCH) as u64
        );
        assert!(value["timing_ns"]["p50"].as_u64().unwrap() > 0);
        assert!(value["timing_ns"]["p99"].as_u64().unwrap() > 0);
        assert!(value["timing_ns"]["p99_9"].as_u64().unwrap() > 0);
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        assert_eq!(
            control.callback_timing_run.len(),
            (FIRST_BATCH + SECOND_BATCH) as u64
        );
        assert_eq!(control.timing_reader.dropped_observations(), 0);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_callback_timing_publication_reports_exact_lap_delivery_and_drop_counts() {
        const BLOCK_SIZE: usize = 128;
        const LAP_EXCESS: usize = 137;
        const TOTAL_CALLBACKS: usize = MAX_TIMING_RECORDS + LAP_EXCESS;
        let session = create_ready_active_point_session(BLOCK_SIZE as u32);
        let input = [0.125_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        for _ in 0..TOTAL_CALLBACKS {
            assert_eq!(
                unsafe { fb_session_render_spatial_v2(session, &block) },
                FbResult::FbOk
            );
        }
        let inner = unsafe { session_ref(session) }.unwrap();
        {
            let control = unsafe { &mut *inner.spatial_control.get() }
                .as_mut()
                .unwrap();
            assert_eq!(control.callback_timing_run.len(), 0);
            assert_eq!(control.timing_reader.dropped_observations(), 0);
        }
        assert_eq!(
            inner.spatial_block_start_frame.load(Ordering::Acquire),
            (TOTAL_CALLBACKS * BLOCK_SIZE) as u64
        );
        assert_eq!(
            metadata.block_start_frame,
            ((TOTAL_CALLBACKS - 1) * BLOCK_SIZE) as u64
        );

        let telemetry = read_session_telemetry(session);
        assert_eq!(
            telemetry["timing_ns"]["observations"],
            MAX_TIMING_RECORDS as u64
        );
        assert_eq!(
            telemetry["timing_ns"]["dropped_observations"],
            LAP_EXCESS as u64
        );
        assert_eq!(
            telemetry["timing_ns"]["callback_local_run"]["observations"],
            MAX_TIMING_RECORDS as u64
        );
        assert_eq!(
            telemetry["timing_ns"]["callback_local_run"]["coverage_complete"],
            false
        );
        assert_eq!(
            telemetry["timing_ns"]["observations"].as_u64().unwrap()
                + telemetry["timing_ns"]["dropped_observations"]
                    .as_u64()
                    .unwrap(),
            TOTAL_CALLBACKS as u64
        );
        let control = unsafe { &mut *inner.spatial_control.get() }
            .as_mut()
            .unwrap();
        assert_eq!(control.callback_timing_run.len(), MAX_TIMING_RECORDS as u64);
        assert_eq!(
            control.timing_reader.dropped_observations(),
            LAP_EXCESS as u64
        );
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    #[ignore = "Wave 0 V2 observational gate; requires the local Steam Audio SDK, release mode, and an uncontended host"]
    fn v2_sixteen_point_source_full_callback_short_soak() {
        run_v2_sixteen_mono_source_full_callback_short_soak(
            FbSourceGeometryV2::FbSourceGeometryPointV2 as u32,
            0.0,
            "point",
            MAX_ACTIVE_SOURCES,
        );
    }

    #[test]
    #[ignore = "Wave 0 V2 observational gate; requires the local Steam Audio SDK, release mode, and an uncontended host"]
    fn v2_sixteen_line_source_full_callback_short_soak() {
        run_v2_sixteen_mono_source_full_callback_short_soak(
            FbSourceGeometryV2::FbSourceGeometryLineSegmentV2 as u32,
            2.0,
            "line_segment",
            MAX_SPATIAL_PRESENTATION_FEEDS,
        );
    }

    fn run_v2_sixteen_mono_source_full_callback_short_soak(
        source_geometry: u32,
        extent_m: f32,
        geometry_name: &str,
        expected_active_feed_count: usize,
    ) {
        const BLOCK_SIZE: usize = 128;
        const SOURCE_COUNT: usize = 16;
        const WARMUP_BLOCKS: usize = 64;
        const MEASURED_BLOCKS: usize = 48_000 * 2 / BLOCK_SIZE;

        for (quality_tier, tier_name, expected_full_sources) in [
            (FbQualityTier::FbQualityDesktop, "desktop", 8_usize),
            (FbQualityTier::FbQualityMobile, "mobile", 4_usize),
        ] {
            let (package, bake) = chicago_fixture_c_paths();
            let config = FbSessionConfigV2 {
                block_size_frames: BLOCK_SIZE as u32,
                source_count: SOURCE_COUNT as u32,
                quality_tier: quality_tier as u32,
                ..FbSessionConfigV2::default()
            };
            let mut session = ptr::null_mut();
            assert_eq!(
                unsafe {
                    fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session)
                },
                FbResult::FbOk
            );
            for source_index in 0..SOURCE_COUNT {
                let source_config = FbSourceProgramConfigV2 {
                    source_index: source_index as u32,
                    source_geometry,
                    extent_m,
                    ..FbSourceProgramConfigV2::default()
                };
                assert_eq!(
                    unsafe { fb_session_configure_source_v2(session, &source_config) },
                    FbResult::FbOk
                );
            }

            let listener = default_ffi_pose_at(0.0, 0.0, 1.5);
            let velocity = FbVec3::default();
            let source_updates: [FbSourceUpdate; SOURCE_COUNT] =
                std::array::from_fn(|source_index| FbSourceUpdate {
                    active: 1,
                    pose: default_ffi_pose_at(
                        source_index as f32 * 0.5 - 4.0,
                        4.0 + source_index as f32 * 0.125,
                        1.5,
                    ),
                    linear_velocity_mps: velocity,
                });
            let control_frame = control_frame_for(listener, velocity, &source_updates);
            // Twelve complete production-shape frames exercise direct at
            // 60 Hz, pathing every fourth frame, and reflections every twelfth
            // without multiplying cadence by the logical source count.
            for _ in 0..12 {
                assert_eq!(
                    unsafe { fb_session_update_control_frame_v2(session, &control_frame) },
                    FbResult::FbOk
                );
            }
            let preprepare_telemetry = read_session_telemetry(session);
            let control_schedule = &preprepare_telemetry["control_schedule"];
            assert_eq!(control_schedule["cadence_advances"], 12);
            assert_eq!(control_schedule["batched_frame_advances"], 12);
            assert_eq!(control_schedule["granular_listener_advances"], 0);
            assert_eq!(control_schedule["granular_source_advances"], 0);
            assert_eq!(
                unsafe { fb_session_prepare_spatial_v2(session) },
                FbResult::FbOk
            );
            let inner = unsafe { session_ref(session) }.unwrap();
            let control = unsafe { &*inner.spatial_control.get() }.as_ref().unwrap();
            assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
            assert_eq!(control.callback_timing_run.len(), 0);
            assert_eq!(control.timing_reader.dropped_observations(), 0);

            let input: [[f32; BLOCK_SIZE]; SOURCE_COUNT] = std::array::from_fn(|source_index| {
                std::array::from_fn(|frame| {
                    ((frame as f32 * 0.05) + source_index as f32 * 0.17).sin() * 0.000_1
                })
            });
            let programs: [FbSourceProgramInputV2; SOURCE_COUNT] =
                std::array::from_fn(|source_index| FbSourceProgramInputV2 {
                    source_index: source_index as u32,
                    channel_count: 1,
                    samples: input[source_index].as_ptr(),
                    sample_count: BLOCK_SIZE,
                    channel_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                });
            let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
            let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
            let mut feeds =
                [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
            let mut metadata = FbSpatialBlockMetadataV2::default();
            let block = FbSpatialRenderBlockV2 {
                source_programs: programs.as_ptr(),
                source_program_count: programs.len() as u32,
                direct_output: FbPlanarOutputV2 {
                    samples: direct.as_mut_ptr(),
                    sample_capacity: direct.len(),
                    plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                environmental_output: FbPlanarOutputV2 {
                    samples: environmental.as_mut_ptr(),
                    sample_capacity: environmental.len(),
                    plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                feed_metadata: feeds.as_mut_ptr(),
                feed_metadata_capacity: feeds.len() as u32,
                block_metadata: &mut metadata,
                ..FbSpatialRenderBlockV2::default()
            };

            let mut stable_generation = None;
            let mut stable_discontinuity_sequence = None;
            let mut completed_blocks = 0_u64;
            for _ in 0..WARMUP_BLOCKS {
                assert_eq!(
                    unsafe { fb_session_render_spatial_v2(session, &block) },
                    FbResult::FbOk
                );
                assert_valid_maximum_block(
                    &metadata,
                    &feeds,
                    &direct,
                    &environmental,
                    completed_blocks * BLOCK_SIZE as u64,
                    expected_active_feed_count,
                    &mut stable_generation,
                    &mut stable_discontinuity_sequence,
                );
                completed_blocks += 1;
            }
            let mut timings = Vec::with_capacity(MEASURED_BLOCKS);
            for _ in 0..MEASURED_BLOCKS {
                let started = std::time::Instant::now();
                let result = unsafe { fb_session_render_spatial_v2(session, &block) };
                let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                assert_eq!(result, FbResult::FbOk);
                assert_valid_maximum_block(
                    &metadata,
                    &feeds,
                    &direct,
                    &environmental,
                    completed_blocks * BLOCK_SIZE as u64,
                    expected_active_feed_count,
                    &mut stable_generation,
                    &mut stable_discontinuity_sequence,
                );
                completed_blocks += 1;
                timings.push(elapsed);
            }
            timings.sort_unstable();
            let percentile = |fraction: f64| {
                let rank = ((fraction * timings.len() as f64).ceil() as usize)
                    .max(1)
                    .min(timings.len())
                    - 1;
                timings[rank]
            };
            let p50_ns = percentile(0.50);
            let p99_ns = percentile(0.99);
            let p99_9_ns = percentile(0.999);
            let telemetry = read_session_telemetry(session);
            let callback_local_run = &telemetry["timing_ns"]["callback_local_run"];
            let published_run_p99_ns = callback_local_run["p99"].as_u64().unwrap();
            let published_run_p99_9_ns = callback_local_run["p99_9"].as_u64().unwrap();
            let published_run_max_ns = callback_local_run["max"].as_u64().unwrap();
            let published_run_max_observation = callback_local_run["max_observation_index"]
                .as_u64()
                .unwrap();
            let full_sources = telemetry["delivered_quality"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|source| source["quality"] == "full")
                .count();
            let simulation_lateness_ns = telemetry["simulation_lateness_ns"]
                .as_array()
                .expect("telemetry has simulation lateness");
            println!(
                "FFI_V2_MAX_SHORT_SOAK duration_s=2 tier={} sources=16 geometry={} \
             presentation_feeds={} environmental_planes=9 outer_host_wall_p50_ms={:.4} \
             outer_host_wall_p99_ms={:.4} outer_host_wall_p99_9_ms={:.4} \
             outer_host_wall_gate=diagnostic_only published_run_callback_p99_ms={:.4} \
             published_run_callback_p99_9_ms={:.4} published_run_callback_max_ms={:.4} \
             published_run_callback_max_observation={} \
             tracked_payload_mib={:.3} detailed_sources={} governor_ladder={} governor_reason={} \
             simulation_lateness_ms=[{:.3},{:.3},{:.3}] callback_deadline_misses={} \
             timing_observations={} dropped_observations={}",
                tier_name,
                geometry_name,
                expected_active_feed_count,
                p50_ns as f64 / 1_000_000.0,
                p99_ns as f64 / 1_000_000.0,
                p99_9_ns as f64 / 1_000_000.0,
                published_run_p99_ns as f64 / 1_000_000.0,
                published_run_p99_9_ns as f64 / 1_000_000.0,
                published_run_max_ns as f64 / 1_000_000.0,
                published_run_max_observation,
                telemetry["memory"]["tracked_current_bytes"]
                    .as_u64()
                    .unwrap() as f64
                    / (1024.0 * 1024.0),
                full_sources,
                telemetry["ladder_position"].as_u64().unwrap(),
                telemetry["reason"].as_str().unwrap(),
                simulation_lateness_ns[0].as_u64().unwrap() as f64 / 1_000_000.0,
                simulation_lateness_ns[1].as_u64().unwrap() as f64 / 1_000_000.0,
                simulation_lateness_ns[2].as_u64().unwrap() as f64 / 1_000_000.0,
                telemetry["timing_ns"]["deadline_misses"],
                telemetry["timing_ns"]["observations"],
                telemetry["timing_ns"]["dropped_observations"],
            );

            assert_eq!(
                metadata.active_presentation_feed_count,
                expected_active_feed_count as u32
            );
            assert_eq!(metadata.environmental_channel_count, 9);
            assert!(direct.iter().any(|sample| *sample != 0.0));
            assert!(stable_generation.is_some());
            assert!(stable_discontinuity_sequence.is_some());
            assert_eq!(completed_blocks, (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64);
            assert_eq!(
                telemetry["timing_ns"]["observations"],
                (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64
            );
            assert_eq!(
                callback_local_run["observations"],
                (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64
            );
            assert_eq!(callback_local_run["coverage_complete"], true);
            assert_eq!(telemetry["timing_ns"]["dropped_observations"], 0);
            assert_eq!(telemetry["timing_ns"]["deadline_misses"], 0);
            assert_eq!(full_sources, expected_full_sources);
            // `published_run_*` conservatively summarizes every callback-local
            // ready-barrier-through-scatter interval in this run. This
            // surrounding `Instant` is retained as host-preemption diagnostics
            // only: the non-real-time test thread can be descheduled after the
            // FFI call returns but before its outer timestamp is sampled.
            assert!(
                published_run_p99_ns < 1_330_000,
                "V2 {geometry_name} published run-wide callback p99 was {published_run_p99_ns} ns"
            );
            assert!(
                published_run_p99_9_ns < 2_130_000,
                "V2 {geometry_name} published run-wide callback p99.9 was {published_run_p99_9_ns} ns"
            );
            assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
        }
    }

    #[test]
    #[ignore = "Wave 0 V2 residency gate; requires the local Steam Audio SDK and release mode"]
    fn v2_sixteen_stereo_images_bind_extra_history_and_thirty_two_feeds() {
        const BLOCK_SIZE: usize = 128;
        const SOURCE_COUNT: usize = 16;
        const CHANNEL_COUNT: usize = 2;
        const WARMUP_BLOCKS: usize = 32;
        const MEASURED_BLOCKS: usize = 48_000 / BLOCK_SIZE;
        const EXPECTED_EXTRA_STEREO_HISTORY_BYTES: u64 = 18_342_720;
        const EXPECTED_DELAYED_PROGRAM_SCRATCH_BYTES: u64 =
            (SOURCE_COUNT * CHANNEL_COUNT * BLOCK_SIZE * size_of::<f32>()) as u64;

        for (quality_tier, tier_name, expected_full_sources) in [
            (FbQualityTier::FbQualityDesktop, "desktop", 8_usize),
            (FbQualityTier::FbQualityMobile, "mobile", 4_usize),
        ] {
            let (package, bake) = chicago_fixture_c_paths();
            let config = FbSessionConfigV2 {
                block_size_frames: BLOCK_SIZE as u32,
                source_count: SOURCE_COUNT as u32,
                quality_tier: quality_tier as u32,
                ..FbSessionConfigV2::default()
            };
            let mut session = ptr::null_mut();
            assert_eq!(
                unsafe {
                    fb_session_create_v2(&config, package.as_ptr(), bake.as_ptr(), &mut session)
                },
                FbResult::FbOk
            );

            for source_index in 0..SOURCE_COUNT {
                let source_config = FbSourceProgramConfigV2 {
                    source_index: source_index as u32,
                    channel_count: CHANNEL_COUNT as u32,
                    source_geometry: FbSourceGeometryV2::FbSourceGeometryStereoImageV2 as u32,
                    extent_m: 2.0,
                    ..FbSourceProgramConfigV2::default()
                };
                assert_eq!(
                    unsafe { fb_session_configure_source_v2(session, &source_config) },
                    FbResult::FbOk
                );
            }

            let inner = unsafe { session_ref(session) }.unwrap();
            let (governor, binding_memory) = {
                let control = unsafe { &mut *inner.spatial_control.get() }
                    .as_mut()
                    .unwrap();
                (
                    control.runner.quality_governor_telemetry().unwrap(),
                    control.memory,
                )
            };
            let neutral = binding_memory.neutral_graph;
            assert_eq!(neutral.configured_source_count, SOURCE_COUNT as u32);
            assert_eq!(neutral.configured_stereo_source_count, SOURCE_COUNT as u32);
            assert_eq!(
                neutral.stereo_indirect_suppressed_source_count,
                SOURCE_COUNT as u32
            );
            assert_eq!(
                neutral.additional_program_channel_payload_bytes,
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES
            );
            assert_eq!(
                neutral.program_delay_audio_history_payload_bytes,
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES * CHANNEL_COUNT as u64
            );
            assert_eq!(
                neutral.program_delay_geometry_history_payload_bytes,
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES
            );
            assert_eq!(
                neutral.delayed_program_scratch_payload_bytes,
                EXPECTED_DELAYED_PROGRAM_SCRATCH_BYTES
            );
            assert!(neutral.outer_vec_payload_bytes > 0);
            assert_eq!(
                governor.memory.render_scratch_bytes,
                neutral
                    .rust_scratch_payload_bytes
                    .saturating_add(neutral.outer_vec_payload_bytes)
            );
            assert_eq!(
                governor
                    .memory
                    .audio_buffer_payload_bytes
                    .saturating_add(governor.memory.render_scratch_bytes)
                    .saturating_add(governor.memory.propagation_delay_line_bytes),
                neutral.total_tracked_payload_bytes
            );

            let listener = default_ffi_pose_at(0.0, 0.0, 1.5);
            let velocity = FbVec3::default();
            let source_updates: [FbSourceUpdate; SOURCE_COUNT] =
                std::array::from_fn(|source_index| FbSourceUpdate {
                    active: 1,
                    pose: default_ffi_pose_at(
                        source_index as f32 * 0.5 - 4.0,
                        4.0 + source_index as f32 * 0.125,
                        1.5,
                    ),
                    linear_velocity_mps: velocity,
                });
            let control_frame = control_frame_for(listener, velocity, &source_updates);
            for _ in 0..12 {
                assert_eq!(
                    unsafe { fb_session_update_control_frame_v2(session, &control_frame) },
                    FbResult::FbOk
                );
            }
            let preprepare_telemetry = read_session_telemetry(session);
            let control_schedule = &preprepare_telemetry["control_schedule"];
            assert_eq!(control_schedule["cadence_advances"], 12);
            assert_eq!(control_schedule["batched_frame_advances"], 12);
            assert_eq!(control_schedule["granular_listener_advances"], 0);
            assert_eq!(control_schedule["granular_source_advances"], 0);
            assert_eq!(
                unsafe { fb_session_prepare_spatial_v2(session) },
                FbResult::FbOk
            );

            let input: [[f32; CHANNEL_COUNT * BLOCK_SIZE]; SOURCE_COUNT] =
                std::array::from_fn(|source_index| {
                    std::array::from_fn(|sample_index| {
                        let channel = sample_index / BLOCK_SIZE;
                        let frame = sample_index % BLOCK_SIZE;
                        ((frame as f32 * 0.05) + source_index as f32 * 0.17 + channel as f32 * 0.41)
                            .sin()
                            * 0.000_1
                    })
                });
            let programs: [FbSourceProgramInputV2; SOURCE_COUNT] =
                std::array::from_fn(|source_index| FbSourceProgramInputV2 {
                    source_index: source_index as u32,
                    channel_count: CHANNEL_COUNT as u32,
                    samples: input[source_index].as_ptr(),
                    sample_count: CHANNEL_COUNT * BLOCK_SIZE,
                    channel_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                });
            let mut direct = [0.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
            let mut environmental = [0.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
            let mut feeds =
                [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
            let mut metadata = FbSpatialBlockMetadataV2::default();
            let block = FbSpatialRenderBlockV2 {
                source_programs: programs.as_ptr(),
                source_program_count: programs.len() as u32,
                direct_output: FbPlanarOutputV2 {
                    samples: direct.as_mut_ptr(),
                    sample_capacity: direct.len(),
                    plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                environmental_output: FbPlanarOutputV2 {
                    samples: environmental.as_mut_ptr(),
                    sample_capacity: environmental.len(),
                    plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                    reserved_u32: 0,
                    plane_stride_samples: BLOCK_SIZE,
                    reserved: [0; 4],
                },
                feed_metadata: feeds.as_mut_ptr(),
                feed_metadata_capacity: feeds.len() as u32,
                block_metadata: &mut metadata,
                ..FbSpatialRenderBlockV2::default()
            };
            let mut stable_generation = None;
            let mut stable_discontinuity_sequence = None;
            let mut completed_blocks = 0_u64;
            for _ in 0..WARMUP_BLOCKS {
                assert_eq!(
                    unsafe { fb_session_render_spatial_v2(session, &block) },
                    FbResult::FbOk
                );
                assert_valid_maximum_block(
                    &metadata,
                    &feeds,
                    &direct,
                    &environmental,
                    completed_blocks * BLOCK_SIZE as u64,
                    SOURCE_COUNT * CHANNEL_COUNT,
                    &mut stable_generation,
                    &mut stable_discontinuity_sequence,
                );
                completed_blocks += 1;
                assert_eq!(
                    inner.spatial_block_start_frame.load(Ordering::Acquire),
                    completed_blocks * BLOCK_SIZE as u64
                );
            }
            let mut timings = Vec::with_capacity(MEASURED_BLOCKS);
            for _ in 0..MEASURED_BLOCKS {
                let started = std::time::Instant::now();
                let result = unsafe { fb_session_render_spatial_v2(session, &block) };
                let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                assert_eq!(result, FbResult::FbOk);
                assert_valid_maximum_block(
                    &metadata,
                    &feeds,
                    &direct,
                    &environmental,
                    completed_blocks * BLOCK_SIZE as u64,
                    SOURCE_COUNT * CHANNEL_COUNT,
                    &mut stable_generation,
                    &mut stable_discontinuity_sequence,
                );
                completed_blocks += 1;
                assert_eq!(
                    inner.spatial_block_start_frame.load(Ordering::Acquire),
                    completed_blocks * BLOCK_SIZE as u64
                );
                timings.push(elapsed);
            }
            timings.sort_unstable();
            let percentile = |fraction: f64| {
                let rank = ((fraction * timings.len() as f64).ceil() as usize)
                    .max(1)
                    .min(timings.len())
                    - 1;
                timings[rank]
            };
            let p50_ns = percentile(0.50);
            let p99_ns = percentile(0.99);
            let p99_9_ns = percentile(0.999);
            assert_eq!(metadata.active_presentation_feed_count, 32);
            assert_eq!(metadata.environmental_channel_count, 9);
            assert_eq!(
                feeds.iter().filter(|feed| feed.valid != 0).count(),
                SOURCE_COUNT * CHANNEL_COUNT
            );
            assert!(direct.iter().all(|sample| sample.is_finite()));
            assert!(environmental.iter().all(|sample| sample.is_finite()));
            assert!(direct.iter().any(|sample| *sample != 0.0));
            assert!(stable_generation.is_some());
            assert!(stable_discontinuity_sequence.is_some());
            assert_eq!(completed_blocks, (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64);

            let telemetry = read_session_telemetry(session);
            let callback_local_run = &telemetry["timing_ns"]["callback_local_run"];
            let published_run_p99_ns = callback_local_run["p99"].as_u64().unwrap();
            let published_run_p99_9_ns = callback_local_run["p99_9"].as_u64().unwrap();
            let published_run_max_ns = callback_local_run["max"].as_u64().unwrap();
            let published_run_max_observation = callback_local_run["max_observation_index"]
                .as_u64()
                .unwrap();
            let full_sources = telemetry["delivered_quality"]["sources"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|source| source["quality"] == "full")
                .count();
            assert_eq!(
                telemetry["memory"]["categories"]["neutral_spatial_graph"]["additional_program_channel_payload_bytes"],
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES
            );
            assert_eq!(
                telemetry["memory"]["categories"]["neutral_spatial_graph"]["stereo_indirect_suppressed_source_count"],
                SOURCE_COUNT as u64
            );
            assert_eq!(
                telemetry["memory"]["categories"]["neutral_spatial_graph"]["outer_vec_payload_bytes"],
                neutral.outer_vec_payload_bytes
            );
            println!(
                "FFI_V2_STEREO_SHORT_SOAK duration_s=1 tier={} sources=16 presentation_feeds=32 \
             stereo_indirect_suppressed=16 extra_history_bytes={} extra_history_mib={:.6} \
             outer_host_wall_p50_ms={:.4} outer_host_wall_p99_ms={:.4} \
             outer_host_wall_p99_9_ms={:.4} outer_host_wall_gate=diagnostic_only \
             published_run_callback_p99_ms={:.4} published_run_callback_p99_9_ms={:.4} \
             published_run_callback_max_ms={:.4} published_run_callback_max_observation={} \
             tracked_payload_mib={:.3} detailed_sources={} timing_observations={} dropped_observations={}",
                tier_name,
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES,
                EXPECTED_EXTRA_STEREO_HISTORY_BYTES as f64 / 1_048_576.0,
                p50_ns as f64 / 1_000_000.0,
                p99_ns as f64 / 1_000_000.0,
                p99_9_ns as f64 / 1_000_000.0,
                published_run_p99_ns as f64 / 1_000_000.0,
                published_run_p99_9_ns as f64 / 1_000_000.0,
                published_run_max_ns as f64 / 1_000_000.0,
                published_run_max_observation,
                telemetry["memory"]["tracked_current_bytes"]
                    .as_u64()
                    .unwrap() as f64
                    / 1_048_576.0,
                full_sources,
                telemetry["timing_ns"]["observations"],
                telemetry["timing_ns"]["dropped_observations"],
            );
            assert_eq!(
                telemetry["timing_ns"]["observations"],
                (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64
            );
            assert_eq!(
                callback_local_run["observations"],
                (WARMUP_BLOCKS + MEASURED_BLOCKS) as u64
            );
            assert_eq!(callback_local_run["coverage_complete"], true);
            assert_eq!(telemetry["timing_ns"]["dropped_observations"], 0);
            assert_eq!(telemetry["timing_ns"]["deadline_misses"], 0);
            assert_eq!(full_sources, expected_full_sources);
            assert!(
                published_run_p99_ns < 1_330_000,
                "stereo published run-wide callback p99 was {published_run_p99_ns} ns"
            );
            assert!(
                published_run_p99_9_ns < 2_130_000,
                "stereo published run-wide callback p99.9 was {published_run_p99_9_ns} ns"
            );

            assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
        }
    }

    #[test]
    fn v2_packed_scratch_scatter_preserves_caller_stride_padding() {
        const BLOCK_SIZE: usize = 4;
        const DIRECT_STRIDE: usize = 7;
        const ENVIRONMENT_STRIDE: usize = 6;
        const DIRECT_SENTINEL: f32 = 77.0;
        const ENVIRONMENT_SENTINEL: f32 = 88.0;
        const FEED_GUARD: u64 = 0xA55A_5AA5_C33C_3CC3;

        #[repr(C)]
        #[derive(Clone, Copy)]
        struct StridedFeed {
            value: FbPresentationFeedMetadataV2,
            guard: u64,
        }

        let session = create_ready_active_point_session(BLOCK_SIZE as u32);
        let input = [0.25_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let direct_len = (MAX_SPATIAL_PRESENTATION_FEEDS - 1) * DIRECT_STRIDE + BLOCK_SIZE;
        let environment_len =
            (MAX_SPATIAL_ENVIRONMENT_PLANES - 1) * ENVIRONMENT_STRIDE + BLOCK_SIZE;
        let mut direct = vec![DIRECT_SENTINEL; direct_len];
        let mut environment = vec![ENVIRONMENT_SENTINEL; environment_len];
        let mut feeds = [StridedFeed {
            value: FbPresentationFeedMetadataV2::default(),
            guard: FEED_GUARD,
        }; MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: DIRECT_STRIDE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environment.as_mut_ptr(),
                sample_capacity: environment.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: ENVIRONMENT_STRIDE,
                reserved: [0; 4],
            },
            feed_metadata: (&mut feeds[0].value) as *mut FbPresentationFeedMetadataV2,
            feed_metadata_capacity: feeds.len() as u32,
            feed_metadata_stride_bytes: size_of::<StridedFeed>() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbOk
        );
        for plane in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
            let start = plane * DIRECT_STRIDE;
            assert!(
                direct[start..start + BLOCK_SIZE]
                    .iter()
                    .all(|sample| sample.is_finite())
            );
            if plane + 1 != MAX_SPATIAL_PRESENTATION_FEEDS {
                assert!(
                    direct[start + BLOCK_SIZE..start + DIRECT_STRIDE]
                        .iter()
                        .all(|sample| *sample == DIRECT_SENTINEL)
                );
            }
        }
        for plane in 0..MAX_SPATIAL_ENVIRONMENT_PLANES {
            let start = plane * ENVIRONMENT_STRIDE;
            assert!(
                environment[start..start + BLOCK_SIZE]
                    .iter()
                    .all(|sample| sample.is_finite())
            );
            if plane + 1 != MAX_SPATIAL_ENVIRONMENT_PLANES {
                assert!(
                    environment[start + BLOCK_SIZE..start + ENVIRONMENT_STRIDE]
                        .iter()
                        .all(|sample| *sample == ENVIRONMENT_SENTINEL)
                );
            }
        }
        assert!(feeds.iter().all(|feed| feed.guard == FEED_GUARD));
        assert_eq!(feeds[0].value.valid, 1);
        assert_eq!(metadata.block_start_frame, 0);

        let direct_after_valid = direct.clone();
        let environment_after_valid = environment.clone();
        let missing_active_block = FbSpatialRenderBlockV2 {
            source_program_count: 0,
            ..block
        };
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &missing_active_block) },
            FbResult::FbInvalidArgument
        );
        assert_eq!(direct, direct_after_valid);
        assert_eq!(environment, environment_after_valid);
        assert_eq!(metadata.block_start_frame, 0);
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 4);
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_backend_failure_publishes_silent_discontinuity_and_advances_clock() {
        const BLOCK_SIZE: usize = 4;
        let session = fake_bound_failing_spatial_session();
        let input = [0.25_f32; BLOCK_SIZE];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: BLOCK_SIZE,
            reserved: [0; 4],
        }];
        let mut direct = [9.0_f32; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environmental = [11.0_f32; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut feeds = [FbPresentationFeedMetadataV2::default(); MAX_SPATIAL_PRESENTATION_FEEDS];
        let mut metadata = FbSpatialBlockMetadataV2::default();
        let block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr(),
                sample_capacity: direct.len(),
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr(),
                sample_capacity: environmental.len(),
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: BLOCK_SIZE,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr(),
            feed_metadata_capacity: feeds.len() as u32,
            block_metadata: &mut metadata,
            ..FbSpatialRenderBlockV2::default()
        };

        let expected_flags = FB_SPATIAL_BLOCK_DISCONTINUITY_V2
            | FB_SPATIAL_SOURCE_SAFETY_APPLIED_V2
            | FB_SPATIAL_OUTPUT_LIMITER_UNAPPLIED_V2
            | FB_SPATIAL_WORLD_UNROTATED_V2
            | FB_SPATIAL_FINAL_HRTF_UNAPPLIED_V2
            | FB_SPATIAL_SOURCE_DRIVE_APPLIED_V2
            | FB_SPATIAL_MONITOR_GAIN_UNAPPLIED_V2;
        for expected in [(0_u64, 1_u64, 4_u64), (4, 2, 8)] {
            direct.fill(9.0);
            environmental.fill(11.0);
            feeds.fill(FbPresentationFeedMetadataV2::default());
            assert_eq!(
                unsafe { fb_session_render_spatial_v2(session, &block) },
                FbResult::FbOk
            );
            assert!(direct.iter().all(|sample| *sample == 0.0));
            assert!(environmental.iter().all(|sample| *sample == 0.0));
            assert!(feeds.iter().all(|feed| feed.valid == 0));
            assert_eq!(metadata.block_start_frame, expected.0);
            assert_eq!(metadata.discontinuity_sequence, expected.1);
            assert_eq!(
                metadata.validity,
                FbSpatialOutputValidityV2::FbSpatialSilentDiscontinuityV2 as u32
            );
            assert_eq!(metadata.active_presentation_feed_count, 0);
            assert_eq!(metadata.environmental_channel_count, 0);
            assert_eq!(metadata.component_mask, 0);
            assert_eq!(metadata.flags, expected_flags);
            let inner = unsafe { session_ref(session) }.unwrap();
            assert_eq!(
                inner.spatial_block_start_frame.load(Ordering::Acquire),
                expected.2
            );
        }
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn v2_incomplete_shell_never_touches_outputs_or_advances() {
        #[repr(C)]
        struct MetadataGuard {
            before: u64,
            value: FbSpatialBlockMetadataV2,
            after: u64,
        }

        let session = fake_session_with_route_and_source_count(SessionRoute::NeutralSpatial, 2);
        let source_config = FbSourceProgramConfigV2::default();
        // Safety: the fake neutral handle and config are live for each call.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_config) },
            FbResult::FbOk
        );
        // A source shape is immutable after the first successful configure.
        assert_eq!(
            unsafe { fb_session_configure_source_v2(session, &source_config) },
            FbResult::FbInvalidState
        );

        let input = [0.25_f32; 4];
        let program = [FbSourceProgramInputV2 {
            source_index: 0,
            channel_count: 1,
            samples: input.as_ptr(),
            sample_count: input.len(),
            channel_stride_samples: input.len(),
            reserved: [0; 4],
        }];
        let direct_len = FB_MAX_PRESENTATION_FEEDS_V2 as usize * 4;
        let environmental_len = FB_MAX_ENVIRONMENTAL_CHANNELS_V2 as usize * 4;
        let mut direct = vec![7.0_f32; direct_len + 2];
        let mut environmental = vec![11.0_f32; environmental_len + 2];
        let mut feeds = vec![
            FbPresentationFeedMetadataV2::default();
            FB_MAX_PRESENTATION_FEEDS_V2 as usize + 2
        ];
        feeds[0].valid = 0xA5A5_A5A5;
        feeds[FB_MAX_PRESENTATION_FEEDS_V2 as usize + 1].valid = 0x5A5A_5A5A;
        let mut metadata = MetadataGuard {
            before: 0x1122_3344_5566_7788,
            value: FbSpatialBlockMetadataV2::default(),
            after: 0x8877_6655_4433_2211,
        };
        let mut block = FbSpatialRenderBlockV2 {
            source_programs: program.as_ptr(),
            source_program_count: 1,
            direct_output: FbPlanarOutputV2 {
                samples: direct.as_mut_ptr().wrapping_add(1),
                sample_capacity: direct_len,
                plane_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
                reserved_u32: 0,
                plane_stride_samples: 4,
                reserved: [0; 4],
            },
            environmental_output: FbPlanarOutputV2 {
                samples: environmental.as_mut_ptr().wrapping_add(1),
                sample_capacity: environmental_len,
                plane_capacity: FB_MAX_ENVIRONMENTAL_CHANNELS_V2,
                reserved_u32: 0,
                plane_stride_samples: 4,
                reserved: [0; 4],
            },
            feed_metadata: feeds.as_mut_ptr().wrapping_add(1),
            feed_metadata_capacity: FB_MAX_PRESENTATION_FEEDS_V2,
            block_metadata: &mut metadata.value,
            ..FbSpatialRenderBlockV2::default()
        };

        // Safety: every active record and buffer is live, aligned, disjoint,
        // and has the declared capacity.
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbInvalidState
        );
        assert!(direct.iter().all(|sample| *sample == 7.0));
        assert!(environmental.iter().all(|sample| *sample == 11.0));
        assert_eq!(feeds[0].valid, 0xA5A5_A5A5);
        assert!(
            feeds[1..=FB_MAX_PRESENTATION_FEEDS_V2 as usize]
                .iter()
                .all(|feed| feed.valid == 0)
        );
        assert_eq!(
            feeds[FB_MAX_PRESENTATION_FEEDS_V2 as usize + 1].valid,
            0x5A5A_5A5A
        );
        assert_eq!(
            metadata.value.validity,
            FbSpatialOutputValidityV2::FbSpatialInvalidV2 as u32
        );
        assert_eq!(metadata.before, 0x1122_3344_5566_7788);
        assert_eq!(metadata.after, 0x8877_6655_4433_2211);

        // Safety: the fake handle remains live until the final destroy.
        let inner = unsafe { session_ref(session) }.unwrap();
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        block.abi_version = 1;
        assert_eq!(
            unsafe { fb_session_render_spatial_v2(session, &block) },
            FbResult::FbInvalidState
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);

        let short = ShortV2Header {
            abi_version: FB_ABI_VERSION_V2,
            struct_size: size_of::<ShortV2Header>() as u32,
        };
        // Safety: exactly the universal header is readable; its short declared
        // size must reject the call before any full render-block copy.
        assert_eq!(
            unsafe {
                fb_session_render_spatial_v2(
                    session,
                    (&short as *const ShortV2Header).cast::<FbSpatialRenderBlockV2>(),
                )
            },
            FbResult::FbInvalidState
        );
        assert_eq!(inner.spatial_block_start_frame.load(Ordering::Acquire), 0);
        assert!(direct.iter().all(|sample| *sample == 7.0));
        assert!(environmental.iter().all(|sample| *sample == 11.0));
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn telemetry_size_query_rejects_an_inactive_test_handle_without_writing_buffer() {
        let session = fake_session();
        let mut required = 0;
        // Safety: the output-size pointer is valid.
        let result =
            unsafe { fb_session_telemetry_json(session, ptr::null_mut(), 0, &mut required) };
        assert_eq!(result, FbResult::FbInvalidState);
        assert_eq!(required, 0);
        // Safety: this is a unique live allocation and is destroyed once.
        assert_eq!(unsafe { fb_session_destroy(session) }, FbResult::FbOk);
    }

    #[test]
    fn ffi_quality_tier_values_round_trip_and_unknown_values_are_rejected() {
        for (wire, expected) in [
            (FbQualityTier::FbQualityDesktop as u32, QualityTier::Desktop),
            (FbQualityTier::FbQualityMobile as u32, QualityTier::Mobile),
        ] {
            assert_eq!(quality_tier_from_ffi(wire), Some(expected));
        }
        assert_eq!(quality_tier_from_ffi(2), None);
        assert_eq!(
            FbSessionConfig::default().quality_tier,
            FbQualityTier::FbQualityDesktop as u32
        );
        let mobile_defaults = quality_tier_from_ffi(FbQualityTier::FbQualityMobile as u32)
            .unwrap()
            .simulation_defaults();
        assert_eq!(mobile_defaults.reflection_rays, 512);
        assert_eq!(mobile_defaults.reflection_duration_s, 0.5);
        assert_eq!(mobile_defaults.reflection_order, 0);
        assert_eq!(mobile_defaults.pathing_order, 1);
    }

    #[test]
    fn create_rejects_an_invalid_tier_before_reading_package_paths() {
        let config = FbSessionConfig {
            quality_tier: u32::MAX,
            ..FbSessionConfig::default()
        };
        let path = std::ffi::CString::new("/definitely/not/a/fightbox/path").unwrap();
        let mut session: *mut FbSession = ptr::null_mut();
        // Safety: all pointers are valid for the duration of the call.
        let result = unsafe {
            fb_session_create(
                &config,
                path.as_ptr(),
                path.as_ptr(),
                &mut session as *mut *mut FbSession,
            )
        };
        assert_eq!(result, FbResult::FbInvalidArgument);
        assert!(session.is_null());
    }

    fn synthetic_quality_telemetry(
        memory: fightbox_steam_audio::SessionMemoryTelemetry,
    ) -> QualityGovernorTelemetry {
        QualityGovernorTelemetry {
            quality_tier: QualityTier::Mobile,
            tier_source_cap: 4,
            sequence: 1,
            ladder_position: 3,
            reason: GovernorTransitionReason::Initial,
            p50_ns: 10,
            p95_ns: 20,
            p99_ns: 30,
            p99_9_ns: 40,
            callback_deadline_misses: 0,
            simulation_lateness_ns: [0; 3],
            reflections: fightbox_steam_audio::DeliveredReflectionQuality {
                level: ReflectionQualityLevel::Reduced,
                rays: 512,
                diffuse_samples: 16,
                diffuse_samples_target: 16,
                diffuse_samples_availability:
                    fightbox_steam_audio::ReflectionSettingAvailability::Implemented,
                bounces: 1,
                ir_duration_s: 0.5,
                cadence_divisor: 2,
            },
            pathing: PathQualityLevel::NoValidation,
            ambisonic_order: 0,
            reverb: ReverbStrategy::ShortIrLowerOrder,
            reflection_output_gain: 1.0,
            boot_reflection_level: ReflectionQualityLevel::Reduced,
            boot_predicted_cost_ns: 0,
            boot_p99_budget_ns: 0,
            boot_cost_limit_ns: 0,
            sources: [fightbox_steam_audio::SourceQualityTelemetry::default(); MAX_ACTIVE_SOURCES],
            source_count: 1,
            memory,
        }
    }

    #[test]
    fn telemetry_json_reports_tracked_memory_without_claiming_sdk_internal_total() {
        let telemetry = synthetic_quality_telemetry(fightbox_steam_audio::SessionMemoryTelemetry {
            tracked_at_create_bytes: 100,
            tracked_current_bytes: 90,
            tracked_peak_bytes: 110,
            snapshot_ring_payload_bytes: 10,
            reflection_ir_payload_capacity_bytes: 20,
            audio_buffer_payload_bytes: 15,
            render_scratch_bytes: 5,
            propagation_delay_line_bytes: 30,
            retained_bake_bytes: 10,
            steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
        });
        let value: serde_json::Value =
            serde_json::from_str(&telemetry_json(Some(telemetry), 8)).unwrap();

        assert_eq!(value["quality_tier"], "mobile");
        assert_eq!(value["memory"]["tracked_at_create_bytes"], 108);
        assert_eq!(value["memory"]["tracked_current_bytes"], 98);
        assert_eq!(value["memory"]["tracked_peak_bytes"], 118);
        assert!(value.get("preparation").is_none());
        assert_eq!(
            value["memory"]["untracked"][0]["category"],
            "steam_audio_sdk_internal"
        );
        assert_eq!(value["memory"]["untracked"][0]["status"], "untracked");
    }

    #[test]
    fn v2_telemetry_adds_runtime_banks_and_publications_without_double_counting_neutral() {
        let telemetry = synthetic_quality_telemetry(fightbox_steam_audio::SessionMemoryTelemetry {
            tracked_at_create_bytes: 1_000,
            tracked_current_bytes: 900,
            tracked_peak_bytes: 1_100,
            snapshot_ring_payload_bytes: 10,
            reflection_ir_payload_capacity_bytes: 20,
            audio_buffer_payload_bytes: 30,
            render_scratch_bytes: 44,
            propagation_delay_line_bytes: 30,
            retained_bake_bytes: 50,
            steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
        });
        let memory = SpatialBindingMemoryTelemetry {
            neutral_graph: SpatialRenderMemoryTelemetry {
                source_capacity: 16,
                configured_source_count: 2,
                configured_stereo_source_count: 1,
                stereo_indirect_suppressed_source_count: 1,
                program_delay_audio_history_payload_bytes: 10,
                program_delay_geometry_history_payload_bytes: 20,
                // A diagnostic subset of the audio history, never additive.
                additional_program_channel_payload_bytes: 3,
                steam_audio_buffer_payload_bytes: 30,
                // A diagnostic subset of Rust scratch, never additive.
                delayed_program_scratch_payload_bytes: 8,
                outer_vec_payload_bytes: 4,
                rust_scratch_payload_bytes: 40,
                total_tracked_payload_bytes: 104,
                steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
            },
            runtime_graph: RuntimeGraphMemoryTelemetry {
                source_node_capacity: 16,
                propagation_delay_payload_bytes: 50,
                block_scratch_payload_bytes: 40,
                spatial_scratch_payload_bytes: 20,
                total_payload_bytes: 110,
            },
            ffi_presentation_bank_payload_bytes: 192,
            ffi_environmental_bank_payload_bytes: 36,
            propagation_snapshot_publication_payload_bytes: 11,
            callback_timing_publication_payload_bytes: 13,
            macro_route_publication_payload_bytes: 17,
            macro_command_mailbox_payload_bytes: 19,
            macro_acknowledgement_mailbox_payload_bytes: 23,
            macro_render_graph_payload_bytes: 29,
        };
        let mut callback_timing_run = RunTimingHistogram::default();
        for _ in 0..128 {
            callback_timing_run.record(100);
        }
        callback_timing_run.record(2_000_000);
        let macro_ingress =
            MacroLocalIngress::new(CellIdentity::new("chi", "chi:e0:n0"), None).telemetry();
        let value: serde_json::Value = serde_json::from_str(&spatial_telemetry_json(
            Some(telemetry),
            memory,
            &callback_timing_run,
            Some(128),
            0,
            SpatialPreparationTelemetry {
                attempts: 2,
                successes: 1,
                failures: 1,
                latest_duration_ns: Some(77),
            },
            "prepared_not_started",
            SpatialControlScheduleTelemetry {
                cadence_advances: 17,
                batched_frame_advances: 12,
                granular_listener_advances: 3,
                granular_source_advances: 2,
            },
            &macro_ingress,
            FrozenAtmosphere::freeze(None),
            false,
            None,
            false,
            MacroEchoBridgeTelemetry::default(),
            0,
        ))
        .unwrap();

        // The neutral graph's 104 bytes are already present in the governor's
        // audio/scratch/delay categories. RuntimeGraph (110), the two FFI
        // banks (228), base publications (24), and the macro bridge's fixed
        // publications/mailboxes/scratch (88) are the external additions.
        assert_eq!(
            telemetry
                .memory
                .audio_buffer_payload_bytes
                .saturating_add(telemetry.memory.render_scratch_bytes)
                .saturating_add(telemetry.memory.propagation_delay_line_bytes),
            memory.neutral_graph.total_tracked_payload_bytes
        );
        assert_eq!(value["memory"]["tracked_at_create_bytes"], 1_450);
        assert_eq!(value["memory"]["tracked_current_bytes"], 1_350);
        assert_eq!(value["memory"]["tracked_peak_bytes"], 1_550);
        assert_eq!(value["memory"]["v2_external_payload_bytes"], 450);
        assert_eq!(value["macro_ingress"]["selected_cell"]["city"], "chi");
        assert!(value["macro_ingress"]["published_cell"].is_null());
        assert_eq!(value["echo_authority"]["loaded"], false);
        assert_eq!(value["echo_authority"]["adoption_pending"], false);
        assert_eq!(value["memory"]["v2_render_binding_payload_bytes"], 554);
        assert_eq!(value["timing_ns"]["p99"], 30);
        assert_eq!(value["timing_ns"]["p99_9"], 40);
        assert_eq!(value["preparation"]["status"], "prepared_not_started");
        assert_eq!(value["preparation"]["attempts"], 2);
        assert_eq!(value["preparation"]["successes"], 1);
        assert_eq!(value["preparation"]["failures"], 1);
        assert_eq!(value["preparation"]["latest_duration_ns"], 77);
        assert_eq!(value["control_schedule"]["cadence_advances"], 17);
        assert_eq!(value["control_schedule"]["batched_frame_advances"], 12);
        assert_eq!(value["control_schedule"]["granular_listener_advances"], 3);
        assert_eq!(value["control_schedule"]["granular_source_advances"], 2);
        assert_eq!(
            value["control_schedule"]["worker_busy_feedback"],
            "not_applicable"
        );
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["scope"],
            "ready_barrier_through_output_scatter"
        );
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["observations"],
            129
        );
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["coverage_complete"],
            true
        );
        assert_eq!(value["timing_ns"]["callback_local_run"]["max"], 2_000_000);
        assert_eq!(
            value["timing_ns"]["callback_local_run"]["max_observation_index"],
            128
        );
        assert!(
            value["timing_ns"]["callback_local_run"]["p99_9"]
                .as_u64()
                .unwrap()
                >= 2_000_000
        );
        assert_eq!(
            value["memory"]["categories"]["neutral_spatial_graph"]["total_tracked_payload_bytes"],
            104
        );
        assert_eq!(
            value["memory"]["categories"]["neutral_spatial_graph"]["additional_program_channel_payload_bytes"],
            3
        );
        assert_eq!(
            value["memory"]["categories"]["neutral_spatial_graph"]["outer_vec_payload_bytes"],
            4
        );
        assert_eq!(
            value["memory"]["categories"]["runtime_graph"]["total_payload_bytes"],
            110
        );
        assert_eq!(
            value["memory"]["categories"]["runtime_graph"]["spatial_scratch_payload_bytes"],
            20
        );
        assert_eq!(
            value["memory"]["categories"]["ffi_spatial_banks"]["presentation_bank_payload_bytes"],
            192
        );
        assert_eq!(
            value["memory"]["categories"]["ffi_spatial_banks"]["environmental_bank_payload_bytes"],
            36
        );
        assert_eq!(
            value["memory"]["categories"]["ffi_spatial_banks"]["total_payload_bytes"],
            228
        );
        assert_eq!(
            value["memory"]["categories"]["propagation_snapshot_publication"]["shared_payload_bytes"],
            11
        );
        assert_eq!(
            value["memory"]["categories"]["callback_timing_publication"]["shared_payload_bytes"],
            13
        );
        assert_eq!(
            value["memory"]["categories"]["macro_production_bridge"]["total_payload_bytes"],
            88
        );
        assert_eq!(
            value["memory"]["untracked"][1]["category"],
            "v2_publication_and_runtime_overhead"
        );
        assert_eq!(value["timing_ns"]["observations"], 129);
        assert_eq!(value["timing_ns"]["dropped_observations"], 0);
    }
}
