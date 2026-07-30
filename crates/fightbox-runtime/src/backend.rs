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

/// Cadenced simulation entry points (§κ thread roles).
///
/// The runtime calls `run_direct` from the direct worker, `run_pathing` from
/// the path worker, and `run_reflections` from the reflection worker. Steam
/// Audio permits direct and reflection/path inputs to be updated by separate
/// simulation threads when flagged separately; the implementation owns that
/// mapping. Each successful pass publishes a fresh backend-internal snapshot
/// that the paired render graph reads wait-free.
pub trait SimulationRunner: Send {
    fn update_inputs(&mut self, update: &SimulationUpdate);
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
