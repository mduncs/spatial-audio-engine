//! The backend-neutral real-time block-processing spine.
//!
//! Simulation workers publish immutable propagation state, and offline or live
//! device wrappers call the same block processor. This crate depends only on
//! `fightbox-api`: propagation backends depend on it (for the engine-owned
//! snapshot primitive and the §ι seam traits in [`backend`]) and implement its
//! contracts; the capability/status facade lives with the backend crate.

#![deny(unsafe_op_in_unsafe_fn)]

pub mod backend;
mod delay;
#[cfg(feature = "live-output")]
pub mod live;
mod render;
mod safety;
mod snapshot;
mod soak;
mod workers;

pub use delay::FractionalDelayLine;
pub use render::{
    BlockProcessor, FaultCounters, MAX_ACTIVE_SOURCES, MAX_TIMING_RECORDS, OfflineDriver,
    ProcessBlock, PropagationSnapshot, RUN_TIMING_HISTOGRAM_BUCKETS, RenderError,
    RunTimingHistogram, RuntimeGraph, SourceBlock, SourcePropagation, Telemetry, TimingHistory,
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
pub use workers::{
    SimulationCadences, SimulationPassTelemetry, SimulationWorker, SimulationWorkerError,
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
