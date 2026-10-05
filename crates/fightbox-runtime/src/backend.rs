//! The engine↔backend seam (authority note §ι).
//!
//! `fightbox-runtime` owns transport, calibration, delay, buses, capture, and
//! deadline policy. A propagation backend owns vendor effects and reads its own
//! published snapshot. The two are bound at graph construction: no vendor
//! handle crosses this boundary and no dynamic backend lookup occurs in the
//! audio callback.
//!
//! Dependency direction: the backend crate depends on `fightbox-runtime` and
//! implements these traits; the runtime's workers and block processor consume
//! them generically. Portable runtime tests use a mock implementation.

use fightbox_api::{EnuVector3, ListenerState, Pose};

/// Fixed engine-wide active-source capacity shared with the render graph.
pub use crate::render::MAX_ACTIVE_SOURCES;

/// Per-source motion state fed to simulation workers at their own cadence.
///
/// Orientation rides in `pose`; velocity exists for delay/Doppler targets and
/// motion-bounded validation, not for any vendor "Doppler effect" — the engine
/// owns the delay line (§λ).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceMotion {
    pub active: bool,
    pub pose: Pose,
    pub linear_velocity_mps: EnuVector3,
}

impl Default for SourceMotion {
    fn default() -> Self {
        Self {
            active: false,
            pose: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            linear_velocity_mps: EnuVector3::default(),
        }
    }
}

/// One coherent simulation input frame for all workers.
///
/// Copied by value into the backend; the backend never holds references into
/// runtime-owned state across calls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimulationUpdate {
    pub listener: ListenerState,
    pub sources: [SourceMotion; MAX_ACTIVE_SOURCES],
}

/// Backend simulation failure surface. Workers record these; they never panic
/// the engine and never reach the audio callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationError {
    /// The backend rejected the update (non-finite pose, inactive session).
    InvalidUpdate,
    /// The vendor kernel reported a failure for this pass.
    KernelFailure,
}

/// Simulation lane whose scheduler deadline was missed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationPass {
    Direct,
    Pathing,
    Reflections,
}

/// Minimum worker-attributable scheduling lateness reported to a backend.
///
/// Lateness already accumulated when a positive park returns is excluded by
/// [`crate::SimulationWorker`] before this threshold is applied. That parked
/// portion can include host wake delay, an intentional reflection max-rate
/// eligibility wait, or both.
pub const SIMULATION_LATENESS_TRIGGER_NS: u64 = 5_000_000;

/// Cadenced simulation entry points (§κ thread roles).
///
/// The runtime's current [`crate::SimulationWorker`] multiplexes `run_direct`,
/// `run_pathing`, and `run_reflections` on one simulation thread. Steam Audio
/// permits direct and reflection/path inputs to be updated by separate threads
/// when flagged separately, so a measured future need can split those lanes
/// without changing this backend seam. Each successful pass publishes a fresh
/// backend-internal snapshot that the paired render graph reads wait-free.
pub trait SimulationRunner: Send {
    fn update_inputs(&mut self, update: &SimulationUpdate);
    /// Reports scheduler lateness attributable to work on the multiplexed
    /// simulation worker after lateness accumulated during a positive park has
    /// been excluded.
    ///
    /// Backends without an adaptive quality policy may retain this no-op
    /// default. Decorators around an adaptive backend must forward it.
    fn observe_simulation_lateness(&mut self, _pass: SimulationPass, _lateness_ns: u64) {}
    fn run_direct(&mut self) -> Result<(), SimulationError>;
    fn run_pathing(&mut self) -> Result<(), SimulationError>;
    fn run_reflections(&mut self) -> Result<(), SimulationError>;
}

/// Per-source calibrated, delayed mono input for one block.
#[derive(Clone, Copy, Debug)]
pub struct BackendSourceBlock<'a> {
    pub source_index: usize,
    pub input_mono: &'a [f32],
}

/// Listener orientation late-bound at block rate for HRTF/Ambisonic decode
/// (§κ): position feeds the simulation workers, orientation feeds every block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ListenerOrientation {
    pub forward: EnuVector3,
    pub up: EnuVector3,
}

/// One block through the backend's vendor-effect graph.
pub struct PropagationRenderBlock<'a> {
    pub listener_orientation: ListenerOrientation,
    pub sources: &'a [BackendSourceBlock<'a>],
    /// Spatialized stereo accumulated INTO by the backend (callers pre-zero).
    pub output_left: &'a mut [f32],
    pub output_right: &'a mut [f32],
}

/// Render-graph failure surface. A failing block leaves the outputs untouched
/// beyond what was already accumulated; the caller records the fault and keeps
/// the callback alive (§κ failure behavior).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendRenderError {
    InvalidBlockLength,
    InvalidSourceIndex,
    InactiveGraph,
}

/// The backend half of the bound render pair (§ι `BackendRenderGraph`).
///
/// Implementations must be wait-free and allocation-free after construction:
/// fixed buffers, a fully published backend snapshot, no locks, no filesystem,
/// no vendor simulation calls.
pub trait BackendRenderGraph: Send {
    fn render_block(&mut self, block: PropagationRenderBlock<'_>)
    -> Result<(), BackendRenderError>;
}

/// Maximum decoded program planes carried by one logical source.
pub const MAX_SPATIAL_PROGRAM_PLANES: usize = 2;
/// Maximum pre-HRTF presentation components emitted for one logical source.
pub const MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE: usize = 3;
/// Fixed engine-wide capacity of caller-owned pre-HRTF presentation planes.
pub const MAX_SPATIAL_PRESENTATION_FEEDS: usize =
    MAX_ACTIVE_SOURCES * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE;
/// Highest environmental Ambisonic order admitted by the neutral seam.
pub const MAX_SPATIAL_ENVIRONMENT_ORDER: usize = 2;
/// Fixed caller-owned capacity of the ACN/N3D environmental plane bank.
pub const MAX_SPATIAL_ENVIRONMENT_PLANES: usize =
    ambisonic_channel_count(MAX_SPATIAL_ENVIRONMENT_ORDER);

const _: () = assert!(MAX_ACTIVE_SOURCES == 16);
const _: () = assert!(MAX_SPATIAL_PRESENTATION_FEEDS == 48);
const _: () = assert!(ambisonic_channel_count(0) == 1);
const _: () = assert!(ambisonic_channel_count(1) == 4);
const _: () = assert!(ambisonic_channel_count(2) == 9);
const _: () = assert!(MAX_SPATIAL_ENVIRONMENT_PLANES == 9);

/// Number of Ambisonic channels in a complete order: `(order + 1)^2`.
#[must_use]
pub const fn ambisonic_channel_count(order: usize) -> usize {
    (order + 1) * (order + 1)
}

/// Decoded planar program input for one logical source.
///
/// Active planes occupy the prefix named by `program_plane_count`. A mono
/// source therefore supplies its samples in plane zero and an empty plane one;
/// a stereo source supplies left then right. The runtime applies calibration
/// and source safety before constructing [`SpatialBackendSourceBlock`]. Every
/// source that is active in the callback's propagation snapshot must appear
/// exactly once. Snapshot-inactive sources may be omitted. Active silence and
/// tail draining use explicit all-zero planes; omission never means silence.
#[derive(Clone, Copy, Debug)]
pub struct SpatialProgramBlock<'a> {
    pub source_index: usize,
    pub program_plane_count: usize,
    pub program_planes: [&'a [f32]; MAX_SPATIAL_PROGRAM_PLANES],
}

/// Calibrated, source-safety-limited planar input for one backend block.
///
/// Plane order is unchanged from [`SpatialProgramBlock`]. Physical propagation
/// backends must advance one shared trajectory controller for all active planes
/// of this logical source; independent per-plane trajectory controllers are
/// outside this contract. The slice of these blocks is the callback-coherent,
/// complete active-source set validated by the runtime. Backends must not
/// reinterpret a separate control publication as a second activity authority.
#[derive(Clone, Copy, Debug)]
pub struct SpatialBackendSourceBlock<'a> {
    pub source_index: usize,
    pub program_plane_count: usize,
    pub program_planes: [&'a [f32]; MAX_SPATIAL_PROGRAM_PLANES],
}

/// Stable bit-valued identity of one neutral presentation component.
///
/// `DiscreteEcho` reserves its ABI bit but is not an admitted object feed in
/// Wave 0. Echo energy remains absent until a later backend can encode it into
/// the environmental field without reusing an existing component identity.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialPresentationComponent {
    #[default]
    DirectCenter = 1 << 0,
    WidthPositive = 1 << 1,
    WidthNegative = 1 << 2,
    DiscreteEcho = 1 << 3,
}

impl SpatialPresentationComponent {
    /// Fixed per-source object-plane slot. Discrete echoes deliberately have
    /// no object slot in Wave 0.
    #[must_use]
    pub const fn presentation_slot(self) -> Option<usize> {
        match self {
            Self::DirectCenter => Some(0),
            Self::WidthPositive => Some(1),
            Self::WidthNegative => Some(2),
            Self::DiscreteEcho => None,
        }
    }
}

/// Which fixed world-space descriptor locates a presentation feed.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialFeedPlacement {
    #[default]
    Pose = 0,
    Direction = 1,
}

/// Metadata paired by index with one plane in the presentation output bank.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialPresentationFeedMetadata {
    pub valid: bool,
    pub source_index: usize,
    pub component: SpatialPresentationComponent,
    pub placement: SpatialFeedPlacement,
    /// World-space engine ENU pose. Authoritative when `placement == Pose`;
    /// direction-based backends may retain it as correlated diagnostics.
    pub pose_enu: Pose,
    /// Unit ENU direction from the correlated listener position to this feed.
    /// Authoritative when `placement == Direction`; displacement with squared
    /// length at most `1e-12 m^2`, including coincidence, uses the
    /// deterministic `+north` direction `(0, 1, 0)`.
    pub direction_enu: EnuVector3,
    pub latency_frames: u32,
}

impl Default for SpatialPresentationFeedMetadata {
    fn default() -> Self {
        Self {
            valid: false,
            source_index: 0,
            component: SpatialPresentationComponent::DirectCenter,
            placement: SpatialFeedPlacement::Pose,
            pose_enu: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            direction_enu: EnuVector3::default(),
            latency_frames: 0,
        }
    }
}

/// Active environmental order. `None` distinguishes no field from order zero.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialAmbisonicOrder {
    #[default]
    None = 0,
    Zero = 1,
    One = 2,
    Two = 3,
}

impl SpatialAmbisonicOrder {
    #[must_use]
    pub const fn order(self) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Zero => Some(0),
            Self::One => Some(1),
            Self::Two => Some(2),
        }
    }

    #[must_use]
    pub const fn channel_count(self) -> usize {
        match self.order() {
            Some(order) => ambisonic_channel_count(order),
            None => 0,
        }
    }
}

/// Environmental channel order is fixed to Ambisonic Channel Number order.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialAmbisonicChannelOrder {
    #[default]
    Acn = 0,
}

/// Environmental normalization is fixed to N3D at this seam.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialAmbisonicNormalization {
    #[default]
    N3d = 0,
}

/// Explicit right-handed basis of the environmental ACN/N3D coefficients.
///
/// Presentation-feed pose and direction fields are always engine ENU. The
/// environmental field names its basis independently because ACN identifies
/// channel order, not world-axis meaning.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialEnvironmentalBasis {
    /// Engine domain axes: +X east, +Y north, +Z up.
    #[default]
    RightHandedEnu = 0,
    /// Steam Audio axes after the one domain rotation: +X right/east,
    /// +Y up, +Z back/south.
    RightHandedXRightYUpZBack = 1,
}

/// Whether a caller may consume the current neutral output block.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialOutputValidity {
    #[default]
    Invalid = 0,
    Valid = 1,
    /// The block is deliberately silent and advances the discontinuity
    /// sequence after a backend failure or rejected backend publication.
    SilentDiscontinuity = 2,
}

/// Fixed-layout description of one neutral backend output block.
///
/// Presentation plane `source_index * 3 + component_slot` has the matching
/// stable metadata entry; `active_presentation_feed_count` counts valid entries
/// across that fixed bank and inactive entries remain `Default`. Active
/// environmental planes occupy the ACN prefix named by
/// `active_environmental_plane_count`; all other caller-owned planes are zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatialOutputMetadata {
    pub sample_rate_hz: u32,
    pub block_size_frames: u32,
    pub block_start_frame: u64,
    pub validity: SpatialOutputValidity,
    pub generation: u64,
    pub discontinuity_sequence: u64,
    pub active_presentation_feed_count: usize,
    pub active_environmental_order: SpatialAmbisonicOrder,
    pub active_environmental_plane_count: usize,
    pub environmental_latency_frames: u32,
    pub environmental_channel_order: SpatialAmbisonicChannelOrder,
    pub environmental_normalization: SpatialAmbisonicNormalization,
    pub environmental_basis: SpatialEnvironmentalBasis,
    pub world_space_unrotated: bool,
    pub source_drive_applied: bool,
    pub source_safety_gain_applied: bool,
    pub monitor_gain_applied: bool,
    pub final_hrtf_applied: bool,
    pub output_limiter_applied: bool,
    pub presentation_feeds: [SpatialPresentationFeedMetadata; MAX_SPATIAL_PRESENTATION_FEEDS],
}

impl Default for SpatialOutputMetadata {
    fn default() -> Self {
        Self {
            sample_rate_hz: 0,
            block_size_frames: 0,
            block_start_frame: 0,
            validity: SpatialOutputValidity::Invalid,
            generation: 0,
            discontinuity_sequence: 0,
            active_presentation_feed_count: 0,
            active_environmental_order: SpatialAmbisonicOrder::None,
            active_environmental_plane_count: 0,
            environmental_latency_frames: 0,
            environmental_channel_order: SpatialAmbisonicChannelOrder::Acn,
            environmental_normalization: SpatialAmbisonicNormalization::N3d,
            environmental_basis: SpatialEnvironmentalBasis::RightHandedEnu,
            world_space_unrotated: false,
            source_drive_applied: false,
            source_safety_gain_applied: false,
            monitor_gain_applied: false,
            final_hrtf_applied: false,
            output_limiter_applied: false,
            presentation_feeds: [SpatialPresentationFeedMetadata::default();
                MAX_SPATIAL_PRESENTATION_FEEDS],
        }
    }
}

/// Channel-aware runtime call with caller-owned fixed-capacity output banks.
///
/// Both banks are plane-major and have exact lengths of respectively
/// `48 * block_size_frames` and `9 * block_size_frames` samples. Plane `p`
/// occupies `p * block_size_frames .. (p + 1) * block_size_frames`.
pub struct SpatialProcessBlock<'a> {
    pub now_ns: u64,
    pub block_start_frame: u64,
    pub sources: &'a [SpatialProgramBlock<'a>],
    pub presentation_bank: &'a mut [f32],
    pub environmental_bank: &'a mut [f32],
    pub metadata: &'a mut SpatialOutputMetadata,
}

/// One calibrated program block through the neutral backend graph.
pub struct SpatialPropagationRenderBlock<'a> {
    pub block_start_frame: u64,
    /// Sequence of the one Runtime propagation snapshot that selected this
    /// callback's complete active source set. A paired backend must reject the
    /// block unless its copied direct-acoustics generation carries this token.
    pub propagation_sequence: u64,
    pub sources: &'a [SpatialBackendSourceBlock<'a>],
    pub presentation_bank: &'a mut [f32],
    pub environmental_bank: &'a mut [f32],
    pub metadata: &'a mut SpatialOutputMetadata,
}

/// Neutral backend failure surface. Runtime converts every backend-side
/// failure into a zeroed [`SpatialOutputValidity::SilentDiscontinuity`] block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialBackendRenderError {
    InvalidBlockLength,
    InvalidSourceIndex,
    InvalidProgramPlaneCount,
    InvalidOutputMetadata,
    /// Runtime activity and backend direct acoustics came from different
    /// control generations. Runtime converts this into one advancing silent
    /// discontinuity instead of rendering a mixed-generation source set.
    PropagationSequenceMismatch,
    InactiveGraph,
}

/// Whether a world generation still owns already-admitted environmental tail.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SpatialTailRetirementState {
    TailRemaining,
    #[default]
    TailComplete,
}

/// Backend half of the bound neutral render pair.
///
/// Implementations are wait-free and allocation-free after construction. They
/// fill only valid fixed presentation slots and the active environmental
/// prefix; runtime pre-zeroes the complete fixed capacity before every call.
pub trait SpatialBackendRenderGraph: Send {
    /// Performs any vendor-specific lazy initialization on the control thread.
    ///
    /// The call happens after the paired simulation has published the exact
    /// listener/source state that will seed the first public render. It must
    /// leave audio history reset and may be repeated before rendering starts.
    /// Every backend must opt into this contract explicitly, including
    /// backends whose implementation is intentionally a no-op.
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError>;

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError>;

    /// Number of callback blocks to render into swap scratch before this graph
    /// may become audible. The default keeps existing backends on the original
    /// immediate-adoption path; backends with callback-side lazy state may
    /// request a bounded, fixed warmup.
    fn pre_crossfade_warmup_blocks(&self) -> u8 {
        0
    }

    /// Freezes the graph against new reflection/echo admission. Direct and path
    /// rendering may continue only for the bounded handoff crossfade.
    fn begin_tail_retirement(&mut self) {}

    /// Reports whether already-admitted environmental tail remains. The swap
    /// wrapper uses this after the direct/path crossfade to avoid an otherwise
    /// unnecessary extra callback for graphs with no tail-bearing effects.
    fn tail_retirement_state(&self) -> SpatialTailRetirementState {
        SpatialTailRetirementState::TailComplete
    }

    /// Advances only already-buffered environmental tail. Source program,
    /// geometry, simulation publications, and event admission are absent by
    /// construction. The bank is the same fixed nine-plane layout as a normal
    /// neutral render block.
    fn render_retiring_environmental_tail(
        &mut self,
        environmental_bank: &mut [f32],
    ) -> Result<SpatialTailRetirementState, SpatialBackendRenderError> {
        environmental_bank.fill(0.0);
        Ok(SpatialTailRetirementState::TailComplete)
    }
}

/// Invalid caller input for [`crate::RuntimeGraph::process_spatial_block`].
/// Backend failures are not returned here; they become silent discontinuities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpatialRenderError {
    TooManySources,
    InvalidSourceIndex,
    DuplicateSourceBlock,
    InvalidProgramPlaneCount,
    ConfiguredProgramShapeMismatch {
        source_index: usize,
        configured_plane_count: usize,
        supplied_plane_count: usize,
    },
    MissingActiveProgram {
        source_index: usize,
    },
    ActiveSourceNotConfigured {
        source_index: usize,
    },
    InactiveProgramPlaneNotEmpty,
    InvalidBlockLength,
    InvalidOutputBankLength,
    SpatialBackendUnavailable,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spatial_capacity_and_component_layout_are_frozen() {
        assert_eq!(MAX_ACTIVE_SOURCES, 16);
        assert_eq!(MAX_SPATIAL_PROGRAM_PLANES, 2);
        assert_eq!(MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE, 3);
        assert_eq!(MAX_SPATIAL_PRESENTATION_FEEDS, 48);
        assert_eq!(MAX_SPATIAL_ENVIRONMENT_ORDER, 2);
        assert_eq!(MAX_SPATIAL_ENVIRONMENT_PLANES, 9);
        assert_eq!(SpatialPresentationComponent::DirectCenter as u32, 1 << 0);
        assert_eq!(SpatialPresentationComponent::WidthPositive as u32, 1 << 1);
        assert_eq!(SpatialPresentationComponent::WidthNegative as u32, 1 << 2);
        assert_eq!(SpatialPresentationComponent::DiscreteEcho as u32, 1 << 3);
        assert_eq!(
            SpatialPresentationComponent::DirectCenter.presentation_slot(),
            Some(0)
        );
        assert_eq!(
            SpatialPresentationComponent::WidthPositive.presentation_slot(),
            Some(1)
        );
        assert_eq!(
            SpatialPresentationComponent::WidthNegative.presentation_slot(),
            Some(2)
        );
        assert_eq!(
            SpatialPresentationComponent::DiscreteEcho.presentation_slot(),
            None
        );
    }

    #[test]
    fn environmental_orders_are_exact_acn_prefix_sizes() {
        assert_eq!(SpatialAmbisonicOrder::None.channel_count(), 0);
        assert_eq!(SpatialAmbisonicOrder::Zero.channel_count(), 1);
        assert_eq!(SpatialAmbisonicOrder::One.channel_count(), 4);
        assert_eq!(SpatialAmbisonicOrder::Two.channel_count(), 9);
    }

    #[test]
    fn default_spatial_metadata_has_no_active_or_applied_claims() {
        let metadata = SpatialOutputMetadata::default();
        assert_eq!(metadata.validity, SpatialOutputValidity::Invalid);
        assert_eq!(metadata.active_presentation_feed_count, 0);
        assert_eq!(
            metadata.active_environmental_order,
            SpatialAmbisonicOrder::None
        );
        assert_eq!(metadata.active_environmental_plane_count, 0);
        assert!(!metadata.world_space_unrotated);
        assert!(!metadata.source_drive_applied);
        assert!(!metadata.source_safety_gain_applied);
        assert!(!metadata.monitor_gain_applied);
        assert!(!metadata.final_hrtf_applied);
        assert!(!metadata.output_limiter_applied);
        assert!(
            metadata
                .presentation_feeds
                .iter()
                .all(|feed| !feed.valid && *feed == SpatialPresentationFeedMetadata::default())
        );
    }
}
