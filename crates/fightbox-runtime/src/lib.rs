//! The backend-neutral real-time block-processing spine.
//!
//! Simulation workers publish immutable propagation state, and offline or live
//! device wrappers call the same block processor. This crate depends only on
//! `fightbox-api`: propagation backends depend on it (for the engine-owned
//! snapshot primitive and the §ι seam traits in [`backend`]) and implement its
//! contracts; the capability/status facade lives with the backend crate.

#![deny(unsafe_op_in_unsafe_fn)]

mod atmosphere;
pub mod backend;
mod cell_streaming;
mod delay;
mod diffuse;
mod directivity;
mod enclosure;
mod ground;
mod ingress;
#[cfg(feature = "live-output")]
pub mod live;
#[cfg(feature = "live-output")]
pub mod live_input;
mod macro_transport;
mod monitor;
mod realtime_clock;
mod render;
mod safety;
mod snapshot;
mod soak;
mod spectral;
mod workers;

pub use atmosphere::{
    AtmosphereTransferError, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M, FrozenAtmosphere,
    THREE_BAND_AIR_REFERENCE_HZ,
};
pub use cell_streaming::{
    CellIdentity, CellPrepareEstimate, CellStreamManager, CellStreamTelemetry, CellStreamingError,
    CellStreamingLimits, CompletePreparation, DEFAULT_ACTIVE_PREPARED_TARGET_BYTES,
    DEFAULT_ADVISORY_RESERVE_BYTES, DEFAULT_PREPARATION_PEAK_BYTES,
    DEFAULT_RAW_CELL_HARD_LIMIT_BYTES, FreshMemorySample, MIB, NeighborStateTelemetry,
    NeighborTelemetry, PrepareAdmission, PrepareCancellationReason, PrepareFailureReason,
    PrepareJob, PrepareRefusalReason, PrepareTicket, RouteCellCandidate, RouteDirection,
    choose_route_candidate,
};
pub use delay::FractionalDelayLine;
pub use diffuse::{SharedDiffuseError, SharedDiffuseField, SharedDiffuseMemoryTelemetry};
pub use directivity::{
    AxisymmetricDirectivityEvaluation, DirectivityPublication, DirectivityTransferError,
    evaluate_axisymmetric_directivity, publish_source_directivity,
};
pub use enclosure::{
    EnclosureAuthority, EnclosureEvaluation, EnclosureEvaluationError, EnclosureScene,
};
pub use fightbox_api::atmosphere::{
    AtmosphereObservation, AtmosphereObservationError, AtmosphereProvenance,
    FALLBACK_ATMOSPHERE_OBSERVATION, MAX_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
    MAX_ATMOSPHERE_TEMPERATURE_C, MIN_ATMOSPHERE_RELATIVE_HUMIDITY_PERCENT,
    MIN_ATMOSPHERE_TEMPERATURE_C,
};
pub use fightbox_api::diffuse::{DiffuseFieldProfile, DiffuseFieldProfileError};
pub use fightbox_api::directivity::{
    AXISYMMETRIC_DIRECTIVITY_ANGLE_COUNT, AXISYMMETRIC_DIRECTIVITY_ANGLE_STEP_DEGREES,
    AXISYMMETRIC_DIRECTIVITY_ANGLES_DEGREES, AxisymmetricDirectivityAngleError,
    AxisymmetricDirectivityTable, AxisymmetricDirectivityTableError,
};
pub use fightbox_api::enclosure::{
    AcousticZone, AcousticZoneId, AcousticZoneKind, AxisAlignedZoneBounds, EXTERIOR_ZONE_ID,
    EnclosureAuthoringError, EnclosureProvenance, StaticPortal, StaticPortalId, StaticPortalState,
};
pub use fightbox_api::ground::{
    GroundAuthoringPolicy, GroundSourceKind, StatisticalGroundRequest,
    StatisticalGroundRequestError,
};
pub use fightbox_api::macro_transport::{
    EventPropagationEligibility, EventRole, MacroAssetTransport, MacroEmitter, MacroEventId,
    MacroListener, MacroTransportConfig,
};
pub use fightbox_api::spectral::{
    MAX_COMBINED_SPECTRAL_GAIN_DB, MIN_COMBINED_SPECTRAL_GAIN_DB, SPECTRAL_BAND_CENTERS_HZ,
    SPECTRAL_BAND_COUNT, SpectralStage, SpectralTransfer, SpectralTransferError,
};
pub use ground::{
    FULL_POROUS_GROUND_GAIN_DB, GroundApplication, StatisticalGroundModel, StatisticalGroundResult,
    StatisticalGroundTransferError,
};
pub use ingress::{
    AdmittedIngressTail, CellArtifactIdentity, FinalLocalLeg, IngressArrivalContext,
    IngressEventFamily, IngressFallbackReason, IngressRenderAuthority, LocalCellAuthority,
    LocalIngressActivation, LocalIngressActivationBatch, MacroIngressConditioning,
    MacroIngressTelemetry, MacroLocalIngress,
};
pub use macro_transport::{
    EventActivationBatch, EventAdmissionError, EventEchoBindingError, EventReleaseError,
    EventReservationBatch, EventReservationError, MACRO_EVENT_QUEUE_CAPACITY,
    MACRO_SPEED_OF_SOUND_MPS, MAX_ATOMIC_EVENT_GROUP, MacroEventQueue, MacroEventScheduleRequest,
    MacroEventScheduler, MacroMotionPresentation, MacroPropagationSegment, MacroTransportError,
    MacroTransportPlan, ScheduledMacroEvent, plan_macro_transport,
};
pub use monitor::{
    MonitorRoute, MonitorRouteController, MonitorRouteError, MonitorRoutePublication,
    MonitorRouteReader, RAW_MONITOR_PAD_DB, RAW_MONITOR_PAD_GAIN,
};
pub use realtime_clock::{RealtimeClock, RealtimeClockError, RealtimeTimestamp};
pub use render::{
    BinauralProgramBackend, BlockProcessor, CallbackTimingPublication, CallbackTimingReader,
    CallbackTimingWriter, FaultCounters, MAX_ACTIVE_SOURCES, MAX_TIMING_RECORDS, OfflineDriver,
    ProcessBlock, ProgramProcessBlock, ProgramRenderBlock, PropagationSnapshot,
    RUN_TIMING_HISTOGRAM_BUCKETS, RenderError, RunTimingHistogram, RuntimeGraph,
    RuntimeGraphMemoryTelemetry, SourceBlock, SourcePropagation, Telemetry, TimingHistory,
    run_timing_bucket_upper_bound_ns,
};
pub use safety::{
    OutputSafetyController, OutputSafetyPublication, OutputSafetyReader, SafetyTelemetry,
    TRUE_PEAK_LIMITER_CEILING_DBTP, TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES,
    TRUE_PEAK_LIMITER_RELEASE_SECONDS, proximity_ceiling_gain_db, soft_knee_ceiling_output_db,
};
pub use snapshot::{SnapshotPublication, SnapshotReader, SnapshotWriter};
pub use soak::{
    SoakReport, TimingPercentiles, run_offline_soak, run_offline_soak_with_timing_observer,
};
pub use spectral::{SpectralFilterError, SpectralTransferFilter};
pub use workers::{
    SimulationCadences, SimulationPassTelemetry, SimulationSchedulerPassTelemetry,
    SimulationSchedulerTelemetry, SimulationWorker, SimulationWorkerError,
    SimulationWorkerTelemetry,
};

use fightbox_api::{ConfigError, EngineConfig};

/// Minimal validated runtime shell. SDK handles remain private to backend crates.
#[derive(Clone, Copy, Debug)]
pub struct Runtime {
    config: EngineConfig,
}

impl Runtime {
    pub fn new(config: EngineConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self { config })
    }
    #[must_use]
    pub const fn config(&self) -> EngineConfig {
        self.config
    }
}
