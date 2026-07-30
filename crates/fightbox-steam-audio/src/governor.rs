//! Control-side quality governor and its allocation-free render snapshot.
//!
//! Timing observations and decisions live with the simulation runner. The
//! audio graph receives only a complete immutable snapshot through the same
//! bounded SPSC channel used by the propagation and stage-gain paths.

#[cfg(any(feature = "linked-sdk", test))]
use crate::{AudioConfig, MultiSourceDescriptor, S3SimulationConfig};
#[cfg(any(feature = "linked-sdk", test))]
use fightbox_runtime::SnapshotPublication;
use fightbox_runtime::backend::MAX_ACTIVE_SOURCES;

#[cfg(any(feature = "linked-sdk", test))]
const TIMING_WINDOW: usize = 128;
#[cfg(any(feature = "linked-sdk", test))]
const EVALUATION_INTERVAL: u32 = 16;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_EVALUATIONS: u32 = 8;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_PROBATION_EVALUATIONS: u32 = 8;
// A rung that overloads twice during probation is not a viable operating
// point for this run. Locking that exact rung bounds recovery-induced misses
// while leaving successful probation free to clear stale failure history.
#[cfg(any(feature = "linked-sdk", test))]
const MAX_RECOVERY_FAILURES: u8 = 2;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_P99_NUMERATOR: u64 = 7;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_P99_DENOMINATOR: u64 = 10;
#[cfg(any(feature = "linked-sdk", test))]
const SIMULATION_LATENESS_TRIGGER_NS: u64 = 5_000_000;
#[cfg(any(feature = "linked-sdk", test))]
const HEARING_THRESHOLD_DB_SPL: f32 = 0.0;

/// Reflection simulation quality selected by the governor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectionQualityLevel {
    Full,
    Reduced,
    Minimum,
}

/// Path-simulation work retained at the current quality level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathQualityLevel {
    Full,
    NoValidation,
    PrimaryOnly,
}

/// Per-source render work selected from predicted audibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceQualityLevel {
    Full,
    /// Direct HRTF and occlusion remain; pathing and reflections are faded out.
    DirectOnly,
    /// Reserved for a physically calibrated prediction below the hearing threshold.
    Virtualized,
}

/// Reflection delivery strategies in the authority-note stop-rule order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReverbStrategy {
    SdkMixerConvolution,
    Hybrid,
    Baked,
    ListenerCentric,
    ShortIrLowerOrder,
}

/// Whether this retained-session implementation can activate a reverb rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReverbRungAvailability {
    Implemented,
    StubRequiresGraphRebuild,
    StubRequiresBakedReflectionData,
    StubRequiresListenerReverbGraph,
}

/// Whether a reflection setting can change in the retained simulator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReflectionSettingAvailability {
    Implemented,
    StubRequiresSimulatorRebuild,
}

/// Static capability declaration for one stop-rule rung.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReverbRungCapability {
    pub strategy: ReverbStrategy,
    pub availability: ReverbRungAvailability,
}

/// Complete stop-rule capability surface, including intentionally unavailable rungs.
pub const REVERB_RUNG_CAPABILITIES: [ReverbRungCapability; 5] = [
    ReverbRungCapability {
        strategy: ReverbStrategy::SdkMixerConvolution,
        availability: ReverbRungAvailability::Implemented,
    },
    ReverbRungCapability {
        strategy: ReverbStrategy::Hybrid,
        availability: ReverbRungAvailability::StubRequiresGraphRebuild,
    },
    ReverbRungCapability {
        strategy: ReverbStrategy::Baked,
        availability: ReverbRungAvailability::StubRequiresBakedReflectionData,
    },
    ReverbRungCapability {
        strategy: ReverbStrategy::ListenerCentric,
        availability: ReverbRungAvailability::StubRequiresListenerReverbGraph,
    },
    ReverbRungCapability {
        strategy: ReverbStrategy::ShortIrLowerOrder,
        availability: ReverbRungAvailability::Implemented,
    },
];

/// Why the last delivered-quality transition happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GovernorTransitionReason {
    Initial,
    RenderP99OverBudget,
    RenderP999OverCeiling,
    RenderDeadlineMiss,
    SimulationLate,
    SustainedHeadroom,
    AtMinimumQuality,
    AtFullQuality,
}

/// Simulation lane whose scheduling lateness was observed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GovernorSimulationPass {
    Direct,
    Pathing,
    Reflections,
}

impl GovernorSimulationPass {
    #[cfg(any(feature = "linked-sdk", test))]
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::Direct => 0,
            Self::Pathing => 1,
            Self::Reflections => 2,
        }
    }
}

/// Delivered reflection settings, including the effective control-side cadence.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeliveredReflectionQuality {
    pub level: ReflectionQualityLevel,
    pub rays: i32,
    /// Delivered construction-time value. Steam Audio 4.8.1 does not expose
    /// diffuse samples in per-run shared inputs.
    pub diffuse_samples: i32,
    /// Desired rung value, exposed without pretending the retained simulator adopted it.
    pub diffuse_samples_target: i32,
    pub diffuse_samples_availability: ReflectionSettingAvailability,
    pub bounces: i32,
    pub ir_duration_s: f32,
    /// Run one reflection pass for each N calls made at the caller's base cadence.
    pub cadence_divisor: u8,
}

/// Audibility basis and quality decision for one stable source index.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceQualityTelemetry {
    pub source_index: u8,
    pub quality: SourceQualityLevel,
    /// Predicted level at the listener. Creative sources use relative dB;
    /// physically calibrated sources use dB SPL.
    pub predicted_audibility_db: f32,
    pub physically_calibrated: bool,
    pub below_hearing_threshold: bool,
    /// Runtime decoding/transport remains outside this backend and advances
    /// even while the backend DSP is suspended.
    pub transport_advances: bool,
}

impl Default for SourceQualityTelemetry {
    fn default() -> Self {
        Self {
            source_index: 0,
            quality: SourceQualityLevel::Full,
            predicted_audibility_db: 0.0,
            physically_calibrated: false,
            below_hearing_threshold: false,
            transport_advances: true,
        }
    }
}

/// Copyable delivered-quality and timing surface suitable for later CLI serialization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QualityGovernorTelemetry {
    pub sequence: u64,
    pub ladder_position: u16,
    pub reason: GovernorTransitionReason,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p99_9_ns: u64,
    pub callback_deadline_misses: u64,
    pub simulation_lateness_ns: [u64; 3],
    pub reflections: DeliveredReflectionQuality,
    pub pathing: PathQualityLevel,
    pub ambisonic_order: i32,
    pub reverb: ReverbStrategy,
    pub reflection_output_gain: f32,
    pub sources: [SourceQualityTelemetry; MAX_ACTIVE_SOURCES],
    pub source_count: u8,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GovernorRenderSnapshot {
    pub sequence: u64,
    pub reflections: DeliveredReflectionQuality,
    pub validate_paths: bool,
    pub find_alternate_paths: bool,
    pub ambisonic_order: i32,
    pub reverb: ReverbStrategy,
    pub reflection_output_gain: f32,
    pub sources: [SourceQualityLevel; MAX_ACTIVE_SOURCES],
    pub listener_centric_source: u8,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug)]
struct SourceAudibility {
    declared_level_db: f32,
    physically_calibrated: bool,
    predicted_db: f32,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug)]
enum PendingRenderChange {
    AmbisonicOrder(i32),
    SourceQuality {
        source_index: usize,
        quality: SourceQualityLevel,
    },
    Reverb {
        strategy: ReverbStrategy,
        final_short_ir: bool,
    },
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RecoveryRung {
    ReflectionFull,
    ReflectionReduced,
    PathValidation,
    AlternatePaths,
    Source(usize),
    AmbisonicOrder(i32),
    FullLengthReverb,
}

#[cfg(any(feature = "linked-sdk", test))]
impl RecoveryRung {
    const fn memory_index(self) -> usize {
        const SOURCE_BASE: usize = 4;
        const ORDER_BASE: usize = SOURCE_BASE + MAX_ACTIVE_SOURCES;
        match self {
            Self::ReflectionFull => 0,
            Self::ReflectionReduced => 1,
            Self::PathValidation => 2,
            Self::AlternatePaths => 3,
            Self::Source(index) => SOURCE_BASE + index,
            Self::AmbisonicOrder(order) => ORDER_BASE + (order as usize - 1),
            Self::FullLengthReverb => ORDER_BASE + 3,
        }
    }
}

#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_RUNG_COUNT: usize = 4 + MAX_ACTIVE_SOURCES + 3 + 1;

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecoveryRungMemory {
    failures: u8,
    locked: bool,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug)]
struct RecoveryProbation {
    rung: RecoveryRung,
    remaining_evaluations: u32,
    adopted: bool,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingTransitionAdvance {
    None,
    AdoptedQuality,
    CompletedFade,
}

#[cfg(any(feature = "linked-sdk", test))]
impl Default for SourceAudibility {
    fn default() -> Self {
        Self {
            declared_level_db: 0.0,
            physically_calibrated: false,
            predicted_db: 0.0,
        }
    }
}

#[cfg(any(feature = "linked-sdk", test))]
pub(crate) struct QualityGovernor {
    requested: S3SimulationConfig,
    source_count: usize,
    p99_budget_ns: u64,
    p99_9_ceiling_ns: u64,
    block_period_ns: u64,
    timings: [u64; TIMING_WINDOW],
    timing_next: usize,
    timing_len: usize,
    observations_since_evaluation: u32,
    headroom_evaluations: u32,
    deadline_misses: u64,
    deadline_miss_since_evaluation: bool,
    simulation_lateness_ns: [u64; 3],
    simulation_late_since_evaluation: bool,
    reason: GovernorTransitionReason,
    render: GovernorRenderSnapshot,
    audibility: [SourceAudibility; MAX_ACTIVE_SOURCES],
    writer: fightbox_runtime::SnapshotWriter<GovernorRenderSnapshot>,
    pending_render_change: Option<PendingRenderChange>,
    pending_render_phase: u8,
    recovery_memory: [RecoveryRungMemory; RECOVERY_RUNG_COUNT],
    recovery_probation: Option<RecoveryProbation>,
}

#[cfg(any(feature = "linked-sdk", test))]
impl QualityGovernor {
    pub(crate) fn new(
        audio: AudioConfig,
        requested: S3SimulationConfig,
        descriptors: &[MultiSourceDescriptor],
    ) -> (
        Self,
        fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    ) {
        let block_period_ns = u64::try_from(audio.frame_size)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            / u64::try_from(audio.sample_rate_hz).unwrap_or(1);
        let mut audibility = [SourceAudibility::default(); MAX_ACTIVE_SOURCES];
        for (index, descriptor) in descriptors.iter().enumerate() {
            audibility[index] = SourceAudibility {
                declared_level_db: descriptor.declared_level_db(),
                physically_calibrated: descriptor.is_physically_calibrated(),
                predicted_db: descriptor.declared_level_db(),
            };
        }
        let reflections = delivered_reflections(requested, ReflectionQualityLevel::Full, false);
        let initial = GovernorRenderSnapshot {
            sequence: 1,
            reflections,
            validate_paths: requested.validate_paths,
            find_alternate_paths: requested.find_alternate_paths,
            ambisonic_order: requested.reflection_order,
            reverb: ReverbStrategy::SdkMixerConvolution,
            reflection_output_gain: 1.0,
            sources: [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES],
            listener_centric_source: 0,
        };
        let (writer, reader) = SnapshotPublication::new(initial);
        (
            Self {
                requested,
                source_count: descriptors.len(),
                p99_budget_ns: block_period_ns / 2,
                p99_9_ceiling_ns: block_period_ns.saturating_mul(4) / 5,
                block_period_ns,
                timings: [0; TIMING_WINDOW],
                timing_next: 0,
                timing_len: 0,
                observations_since_evaluation: 0,
                headroom_evaluations: 0,
                deadline_misses: 0,
                deadline_miss_since_evaluation: false,
                simulation_lateness_ns: [0; 3],
                simulation_late_since_evaluation: false,
                reason: GovernorTransitionReason::Initial,
                render: initial,
                audibility,
                writer,
                pending_render_change: None,
                pending_render_phase: 0,
                recovery_memory: [RecoveryRungMemory::default(); RECOVERY_RUNG_COUNT],
                recovery_probation: None,
            },
            reader,
        )
    }

    #[cfg(feature = "linked-sdk")]
    pub(crate) const fn render_quality(&self) -> GovernorRenderSnapshot {
        self.render
    }

    pub(crate) fn observe_source_gain(&mut self, source_index: usize, linear_gain: f32) {
        if source_index >= self.source_count || !linear_gain.is_finite() || linear_gain < 0.0 {
            return;
        }
        let gain_db = if linear_gain > 0.0 {
            20.0 * linear_gain.log10()
        } else {
            -160.0
        };
        self.audibility[source_index].predicted_db =
            self.audibility[source_index].declared_level_db + gain_db;
        if self.render.reverb == ReverbStrategy::ListenerCentric {
            self.render.listener_centric_source = self.most_audible_source() as u8;
            self.publish();
        }
    }

    pub(crate) fn observe_simulation_lateness(
        &mut self,
        pass: GovernorSimulationPass,
        lateness_ns: u64,
    ) {
        let index = pass.index();
        self.simulation_lateness_ns[index] = self.simulation_lateness_ns[index].max(lateness_ns);
        self.simulation_late_since_evaluation |= lateness_ns >= SIMULATION_LATENESS_TRIGGER_NS;
    }

    pub(crate) fn observe_block_timing(&mut self, elapsed_ns: u64) {
        self.timings[self.timing_next] = elapsed_ns;
        self.timing_next = (self.timing_next + 1) % TIMING_WINDOW;
        self.timing_len = self.timing_len.saturating_add(1).min(TIMING_WINDOW);
        self.observations_since_evaluation += 1;
        if elapsed_ns >= self.block_period_ns {
            self.deadline_misses = self.deadline_misses.saturating_add(1);
            self.deadline_miss_since_evaluation = true;
        }

        if self.advance_pending_render_transition() == PendingTransitionAdvance::AdoptedQuality {
            // The elapsed sample belongs to the pre-adoption snapshot. Begin
            // the new rung's evidence window at the next rendered block.
            self.reset_timing_window();
            self.deadline_miss_since_evaluation = false;
            if let Some(probation) = self.recovery_probation.as_mut() {
                probation.adopted = true;
            }
            return;
        }
        if self.observations_since_evaluation < EVALUATION_INTERVAL {
            return;
        }
        self.observations_since_evaluation = 0;
        self.evaluate();
    }

    pub(crate) fn telemetry(&self) -> QualityGovernorTelemetry {
        let (p50_ns, p95_ns, p99_ns, p99_9_ns) = self.percentiles();
        let mut sources = [SourceQualityTelemetry::default(); MAX_ACTIVE_SOURCES];
        for (index, source) in sources.iter_mut().enumerate().take(self.source_count) {
            let audibility = self.audibility[index];
            *source = SourceQualityTelemetry {
                source_index: index as u8,
                quality: self.render.sources[index],
                predicted_audibility_db: audibility.predicted_db,
                physically_calibrated: audibility.physically_calibrated,
                below_hearing_threshold: audibility.physically_calibrated
                    && audibility.predicted_db < HEARING_THRESHOLD_DB_SPL,
                transport_advances: true,
            };
        }
        QualityGovernorTelemetry {
            sequence: self.render.sequence,
            ladder_position: self.ladder_position(),
            reason: self.reason,
            p50_ns,
            p95_ns,
            p99_ns,
            p99_9_ns,
            callback_deadline_misses: self.deadline_misses,
            simulation_lateness_ns: self.simulation_lateness_ns,
            reflections: self.render.reflections,
            pathing: if self.render.validate_paths {
                PathQualityLevel::Full
            } else if self.render.find_alternate_paths {
                PathQualityLevel::NoValidation
            } else {
                PathQualityLevel::PrimaryOnly
            },
            ambisonic_order: self.render.ambisonic_order,
            reverb: self.render.reverb,
            reflection_output_gain: self.render.reflection_output_gain,
            sources,
            source_count: self.source_count as u8,
        }
    }

    fn evaluate(&mut self) {
        let (_, _, p99_ns, p99_9_ns) = self.percentiles();
        let reason = if self.deadline_miss_since_evaluation {
            Some(GovernorTransitionReason::RenderDeadlineMiss)
        } else if p99_9_ns >= self.p99_9_ceiling_ns {
            Some(GovernorTransitionReason::RenderP999OverCeiling)
        } else if p99_ns >= self.p99_budget_ns {
            Some(GovernorTransitionReason::RenderP99OverBudget)
        } else if self.simulation_late_since_evaluation {
            Some(GovernorTransitionReason::SimulationLate)
        } else {
            None
        };
        self.simulation_late_since_evaluation = false;
        self.deadline_miss_since_evaluation = false;

        if let Some(reason) = reason {
            self.headroom_evaluations = 0;
            let degraded = if let Some(probation) = self
                .recovery_probation
                .filter(|probation| probation.adopted)
            {
                self.record_recovery_failure(probation.rung);
                self.recovery_probation = None;
                self.degrade_recovered_rung(probation.rung)
            } else {
                self.recovery_probation = None;
                self.degrade_one()
            };
            if degraded {
                self.reason = reason;
                self.reset_timing_window();
                self.publish();
            } else {
                self.reason = GovernorTransitionReason::AtMinimumQuality;
            }
            return;
        }

        let recovery_p99 =
            self.p99_budget_ns.saturating_mul(RECOVERY_P99_NUMERATOR) / RECOVERY_P99_DENOMINATOR;
        let recovery_p99_9 =
            self.p99_9_ceiling_ns.saturating_mul(RECOVERY_P99_NUMERATOR) / RECOVERY_P99_DENOMINATOR;
        let has_headroom = p99_ns <= recovery_p99 && p99_9_ns <= recovery_p99_9;

        if let Some(mut probation) = self.recovery_probation {
            self.headroom_evaluations = 0;
            if probation.adopted && has_headroom {
                probation.remaining_evaluations = probation.remaining_evaluations.saturating_sub(1);
                if probation.remaining_evaluations == 0 {
                    self.recovery_memory[probation.rung.memory_index()] =
                        RecoveryRungMemory::default();
                    self.recovery_probation = None;
                } else {
                    self.recovery_probation = Some(probation);
                }
            }
            return;
        }

        if has_headroom {
            self.headroom_evaluations += 1;
            let Some(rung) = self.recovery_candidate() else {
                if self.headroom_evaluations >= RECOVERY_EVALUATIONS {
                    self.headroom_evaluations = 0;
                    self.reason = GovernorTransitionReason::AtFullQuality;
                }
                return;
            };
            let memory = self.recovery_memory[rung.memory_index()];
            if memory.locked {
                self.headroom_evaluations = 0;
                return;
            }
            let required_evaluations =
                RECOVERY_EVALUATIONS.saturating_mul(1_u32 << u32::from(memory.failures));
            if self.headroom_evaluations >= required_evaluations {
                self.headroom_evaluations = 0;
                self.apply_recovery(rung);
                self.recovery_probation = Some(RecoveryProbation {
                    rung,
                    remaining_evaluations: RECOVERY_PROBATION_EVALUATIONS,
                    adopted: self.pending_render_change.is_none(),
                });
                self.reason = GovernorTransitionReason::SustainedHeadroom;
                self.reset_timing_window();
                self.publish();
            }
        } else {
            self.headroom_evaluations = 0;
        }
    }

    fn record_recovery_failure(&mut self, rung: RecoveryRung) {
        let memory = &mut self.recovery_memory[rung.memory_index()];
        memory.failures = memory.failures.saturating_add(1);
        memory.locked = memory.failures >= MAX_RECOVERY_FAILURES;
    }

    fn degrade_one(&mut self) -> bool {
        match self.render.reflections.level {
            ReflectionQualityLevel::Full => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Reduced, false);
                return true;
            }
            ReflectionQualityLevel::Reduced => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Minimum, false);
                return true;
            }
            ReflectionQualityLevel::Minimum => {}
        }
        if self.render.validate_paths {
            self.render.validate_paths = false;
            return true;
        }
        if self.render.find_alternate_paths {
            self.render.find_alternate_paths = false;
            return true;
        }
        if let Some(index) = self.least_audible_full_source() {
            let audibility = self.audibility[index];
            let quality = if audibility.physically_calibrated
                && audibility.predicted_db < HEARING_THRESHOLD_DB_SPL
            {
                SourceQualityLevel::Virtualized
            } else {
                SourceQualityLevel::DirectOnly
            };
            self.begin_render_transition(PendingRenderChange::SourceQuality {
                source_index: index,
                quality,
            });
            return true;
        }
        if self.render.ambisonic_order > 0 {
            self.begin_render_transition(PendingRenderChange::AmbisonicOrder(
                self.render.ambisonic_order - 1,
            ));
            return true;
        }
        match self.render.reverb {
            ReverbStrategy::SdkMixerConvolution => {
                // Hybrid needs different effect objects and baked needs an
                // absent data layer, and listener-centric reverb needs a
                // dedicated graph, so select the final feasible retained-graph rung.
                self.begin_render_transition(PendingRenderChange::Reverb {
                    strategy: ReverbStrategy::ShortIrLowerOrder,
                    final_short_ir: true,
                });
                true
            }
            ReverbStrategy::ShortIrLowerOrder
            | ReverbStrategy::Hybrid
            | ReverbStrategy::Baked
            | ReverbStrategy::ListenerCentric => false,
        }
    }

    fn recovery_candidate(&self) -> Option<RecoveryRung> {
        match self.render.reverb {
            ReverbStrategy::ShortIrLowerOrder => {
                return Some(RecoveryRung::FullLengthReverb);
            }
            ReverbStrategy::SdkMixerConvolution
            | ReverbStrategy::Hybrid
            | ReverbStrategy::Baked
            | ReverbStrategy::ListenerCentric => {}
        }
        if self.render.ambisonic_order < self.requested.reflection_order {
            return Some(RecoveryRung::AmbisonicOrder(
                self.render.ambisonic_order + 1,
            ));
        }
        if let Some(index) = self.most_audible_degraded_source() {
            return Some(RecoveryRung::Source(index));
        }
        if !self.render.find_alternate_paths && self.requested.find_alternate_paths {
            return Some(RecoveryRung::AlternatePaths);
        }
        if !self.render.validate_paths && self.requested.validate_paths {
            return Some(RecoveryRung::PathValidation);
        }
        match self.render.reflections.level {
            ReflectionQualityLevel::Minimum => Some(RecoveryRung::ReflectionReduced),
            ReflectionQualityLevel::Reduced => Some(RecoveryRung::ReflectionFull),
            ReflectionQualityLevel::Full => None,
        }
    }

    fn apply_recovery(&mut self, rung: RecoveryRung) {
        match rung {
            RecoveryRung::ReflectionFull => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Full, false);
            }
            RecoveryRung::ReflectionReduced => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Reduced, false);
            }
            RecoveryRung::PathValidation => self.render.validate_paths = true,
            RecoveryRung::AlternatePaths => self.render.find_alternate_paths = true,
            RecoveryRung::Source(source_index) => {
                self.begin_render_transition(PendingRenderChange::SourceQuality {
                    source_index,
                    quality: SourceQualityLevel::Full,
                });
            }
            RecoveryRung::AmbisonicOrder(order) => {
                self.begin_render_transition(PendingRenderChange::AmbisonicOrder(order));
            }
            RecoveryRung::FullLengthReverb => {
                self.begin_render_transition(PendingRenderChange::Reverb {
                    strategy: ReverbStrategy::SdkMixerConvolution,
                    final_short_ir: false,
                });
            }
        }
    }

    fn degrade_recovered_rung(&mut self, rung: RecoveryRung) -> bool {
        match rung {
            RecoveryRung::ReflectionFull => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Reduced, false);
            }
            RecoveryRung::ReflectionReduced => {
                self.render.reflections =
                    delivered_reflections(self.requested, ReflectionQualityLevel::Minimum, false);
            }
            RecoveryRung::PathValidation => self.render.validate_paths = false,
            RecoveryRung::AlternatePaths => self.render.find_alternate_paths = false,
            RecoveryRung::Source(source_index) => {
                let audibility = self.audibility[source_index];
                let quality = if audibility.physically_calibrated
                    && audibility.predicted_db < HEARING_THRESHOLD_DB_SPL
                {
                    SourceQualityLevel::Virtualized
                } else {
                    SourceQualityLevel::DirectOnly
                };
                self.begin_render_transition(PendingRenderChange::SourceQuality {
                    source_index,
                    quality,
                });
            }
            RecoveryRung::AmbisonicOrder(order) => {
                self.begin_render_transition(PendingRenderChange::AmbisonicOrder(order - 1));
            }
            RecoveryRung::FullLengthReverb => {
                self.begin_render_transition(PendingRenderChange::Reverb {
                    strategy: ReverbStrategy::ShortIrLowerOrder,
                    final_short_ir: true,
                });
            }
        }
        true
    }

    fn least_audible_full_source(&self) -> Option<usize> {
        (0..self.source_count)
            .filter(|index| self.render.sources[*index] == SourceQualityLevel::Full)
            .min_by(|left, right| {
                self.audibility[*left]
                    .predicted_db
                    .total_cmp(&self.audibility[*right].predicted_db)
                    .then(left.cmp(right))
            })
    }

    fn most_audible_degraded_source(&self) -> Option<usize> {
        (0..self.source_count)
            .filter(|index| self.render.sources[*index] != SourceQualityLevel::Full)
            .max_by(|left, right| {
                self.audibility[*left]
                    .predicted_db
                    .total_cmp(&self.audibility[*right].predicted_db)
                    .then(left.cmp(right))
            })
    }

    fn most_audible_source(&self) -> usize {
        (0..self.source_count)
            .max_by(|left, right| {
                self.audibility[*left]
                    .predicted_db
                    .total_cmp(&self.audibility[*right].predicted_db)
                    .then(left.cmp(right))
            })
            .unwrap_or(0)
    }

    fn publish(&mut self) {
        self.render.sequence = self.render.sequence.wrapping_add(1);
        self.writer.publish(self.render);
    }

    fn begin_render_transition(&mut self, change: PendingRenderChange) {
        self.render.reflection_output_gain = 0.0;
        self.pending_render_change = Some(change);
        self.pending_render_phase = 0;
    }

    fn advance_pending_render_transition(&mut self) -> PendingTransitionAdvance {
        let Some(change) = self.pending_render_change else {
            return PendingTransitionAdvance::None;
        };
        let advance = if self.pending_render_phase == 0 {
            match change {
                PendingRenderChange::AmbisonicOrder(order) => {
                    self.render.ambisonic_order = order;
                }
                PendingRenderChange::SourceQuality {
                    source_index,
                    quality,
                } => {
                    self.render.sources[source_index] = quality;
                }
                PendingRenderChange::Reverb {
                    strategy,
                    final_short_ir,
                } => {
                    self.render.reverb = strategy;
                    self.render.reflections = delivered_reflections(
                        self.requested,
                        ReflectionQualityLevel::Minimum,
                        final_short_ir,
                    );
                }
            }
            self.pending_render_phase = 1;
            PendingTransitionAdvance::AdoptedQuality
        } else {
            self.render.reflection_output_gain = 1.0;
            self.pending_render_change = None;
            self.pending_render_phase = 0;
            PendingTransitionAdvance::CompletedFade
        };
        self.publish();
        advance
    }

    fn reset_timing_window(&mut self) {
        self.timings = [0; TIMING_WINDOW];
        self.timing_next = 0;
        self.timing_len = 0;
        self.observations_since_evaluation = 0;
    }

    fn percentiles(&self) -> (u64, u64, u64, u64) {
        if self.timing_len == 0 {
            return (0, 0, 0, 0);
        }
        let mut sorted = [0_u64; TIMING_WINDOW];
        sorted[..self.timing_len].copy_from_slice(&self.timings[..self.timing_len]);
        sorted[..self.timing_len].sort_unstable();
        let percentile = |value: f64| {
            let rank = ((value * self.timing_len as f64).ceil() as usize)
                .max(1)
                .min(self.timing_len)
                - 1;
            sorted[rank]
        };
        (
            percentile(0.50),
            percentile(0.95),
            percentile(0.99),
            percentile(0.999),
        )
    }

    fn ladder_position(&self) -> u16 {
        let reflection = match self.render.reflections.level {
            ReflectionQualityLevel::Full => 0,
            ReflectionQualityLevel::Reduced => 1,
            ReflectionQualityLevel::Minimum => 2,
        };
        let path =
            u16::from(!self.render.validate_paths) + u16::from(!self.render.find_alternate_paths);
        let sources = self.render.sources[..self.source_count]
            .iter()
            .filter(|quality| **quality != SourceQualityLevel::Full)
            .count() as u16;
        let order = (self.requested.reflection_order - self.render.ambisonic_order).max(0) as u16;
        let reverb = match self.render.reverb {
            ReverbStrategy::SdkMixerConvolution => 0,
            ReverbStrategy::ListenerCentric => 0,
            ReverbStrategy::ShortIrLowerOrder => 1,
            ReverbStrategy::Hybrid | ReverbStrategy::Baked => 0,
        };
        reflection + path + sources + order + reverb
    }
}

#[cfg(any(feature = "linked-sdk", test))]
fn delivered_reflections(
    requested: S3SimulationConfig,
    level: ReflectionQualityLevel,
    final_short_ir: bool,
) -> DeliveredReflectionQuality {
    let (ray_divisor, diffuse_divisor, bounce_reduction, duration_divisor, cadence_divisor) =
        match level {
            ReflectionQualityLevel::Full => (1, 1, 0, 1.0, 1),
            ReflectionQualityLevel::Reduced => (2, 2, 1, 2.0, 2),
            ReflectionQualityLevel::Minimum => (4, 4, i32::MAX, 4.0, 4),
        };
    let duration_divisor = if final_short_ir {
        duration_divisor * 2.0
    } else {
        duration_divisor
    };
    DeliveredReflectionQuality {
        level,
        rays: (requested.reflection_rays / ray_divisor).max(requested.reflection_rays.min(128)),
        diffuse_samples: requested.diffuse_samples,
        diffuse_samples_target: (requested.diffuse_samples / diffuse_divisor)
            .max(requested.diffuse_samples.min(2)),
        diffuse_samples_availability: ReflectionSettingAvailability::StubRequiresSimulatorRebuild,
        bounces: requested
            .reflection_bounces
            .saturating_sub(bounce_reduction)
            .max(0),
        ir_duration_s: (requested.reflection_duration_s / duration_divisor).max(0.05),
        cadence_divisor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_api::{EnuVector3, ReferenceLevel};

    fn governor(source_count: usize) -> QualityGovernor {
        let descriptors = (0..source_count)
            .map(|index| {
                MultiSourceDescriptor::at(EnuVector3::new(index as f32, 0.0, 0.0))
                    .with_reference_level(ReferenceLevel::CreativeDb { db: index as f32 })
            })
            .collect::<Vec<_>>();
        QualityGovernor::new(
            AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            },
            S3SimulationConfig {
                reflection_rays: 4_096,
                diffuse_samples: 32,
                reflection_bounces: 2,
                reflection_duration_s: 1.0,
                reflection_order: 1,
                validate_paths: true,
                find_alternate_paths: true,
                ..S3SimulationConfig::default()
            },
            &descriptors,
        )
        .0
    }

    fn evaluation(governor: &mut QualityGovernor, duration_ns: u64) {
        for _ in 0..EVALUATION_INTERVAL {
            governor.observe_block_timing(duration_ns);
        }
    }

    fn settle_render_transition(governor: &mut QualityGovernor, duration_ns: u64) {
        governor.observe_block_timing(duration_ns);
        governor.observe_block_timing(duration_ns);
    }

    #[test]
    fn overload_descends_in_frozen_order() {
        let mut governor = governor(2);
        evaluation(&mut governor, 3_000_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced
        );
        evaluation(&mut governor, 3_000_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Minimum
        );
        evaluation(&mut governor, 3_000_000);
        assert_eq!(governor.telemetry().pathing, PathQualityLevel::NoValidation);
        evaluation(&mut governor, 3_000_000);
        assert_eq!(governor.telemetry().pathing, PathQualityLevel::PrimaryOnly);
        evaluation(&mut governor, 3_000_000);
        assert_eq!(governor.telemetry().reflection_output_gain, 0.0);
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::DirectOnly
        );
        evaluation(&mut governor, 3_000_000);
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().sources[1].quality,
            SourceQualityLevel::DirectOnly
        );
        evaluation(&mut governor, 3_000_000);
        assert_eq!(governor.telemetry().ambisonic_order, 0);
        evaluation(&mut governor, 3_000_000);
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::ShortIrLowerOrder
        );
    }

    #[test]
    fn recovery_requires_sustained_headroom_and_does_not_oscillate() {
        let mut governor = governor(1);
        evaluation(&mut governor, 3_000_000);
        assert_eq!(governor.telemetry().ladder_position, 1);

        for _ in 0..(RECOVERY_EVALUATIONS - 1) {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(governor.telemetry().ladder_position, 1);

        // One borderline interval clears the recovery streak.
        evaluation(&mut governor, 1_200_000);
        for _ in 0..(RECOVERY_EVALUATIONS - 1) {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(governor.telemetry().ladder_position, 1);

        // The rolling window must itself drain before sustained headroom can
        // begin; keep feeding quiet observations until the rung recovers.
        for _ in 0..(RECOVERY_EVALUATIONS + TIMING_WINDOW as u32 / EVALUATION_INTERVAL as u32) {
            evaluation(&mut governor, 100_000);
            if governor.telemetry().ladder_position == 0 {
                break;
            }
        }
        assert_eq!(governor.telemetry().ladder_position, 0);
        assert_eq!(
            governor.telemetry().reason,
            GovernorTransitionReason::SustainedHeadroom
        );
    }

    #[test]
    fn oscillating_load_locks_the_failing_rung_and_stops_reintroducing_misses() {
        let mut governor = governor(1);

        // The full reflection rung misses its deadline, while the reduced
        // rung has ample headroom.
        evaluation(&mut governor, 3_000_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced
        );

        for expected_failures in 1..=MAX_RECOVERY_FAILURES {
            let required = RECOVERY_EVALUATIONS * (1_u32 << u32::from(expected_failures - 1));
            for _ in 0..required {
                evaluation(&mut governor, 100_000);
            }
            assert_eq!(
                governor.telemetry().reflections.level,
                ReflectionQualityLevel::Full
            );

            evaluation(&mut governor, 3_000_000);
            assert_eq!(
                governor.telemetry().reflections.level,
                ReflectionQualityLevel::Reduced
            );
            let memory = governor.recovery_memory[RecoveryRung::ReflectionFull.memory_index()];
            assert_eq!(memory.failures, expected_failures);
        }

        let converged_sequence = governor.telemetry().sequence;
        let converged_misses = governor.telemetry().callback_deadline_misses;
        for _ in 0..64 {
            let duration = if governor.telemetry().reflections.level == ReflectionQualityLevel::Full
            {
                3_000_000
            } else {
                100_000
            };
            evaluation(&mut governor, duration);
        }

        let memory = governor.recovery_memory[RecoveryRung::ReflectionFull.memory_index()];
        assert!(memory.locked);
        assert_eq!(governor.telemetry().sequence, converged_sequence);
        assert_eq!(
            governor.telemetry().callback_deadline_misses,
            converged_misses,
            "the converged reduced rung must not reintroduce deadline misses"
        );
    }

    #[test]
    fn sustained_load_drop_recovers_after_backoff_and_resets_rung_memory() {
        let mut governor = governor(1);
        evaluation(&mut governor, 3_000_000);

        for _ in 0..RECOVERY_EVALUATIONS {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Full
        );
        evaluation(&mut governor, 3_000_000);

        let memory_index = RecoveryRung::ReflectionFull.memory_index();
        assert_eq!(governor.recovery_memory[memory_index].failures, 1);
        for _ in 0..(RECOVERY_EVALUATIONS * 2 - 1) {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced,
            "one failed climb must double the sustained-headroom requirement"
        );
        evaluation(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Full
        );

        for _ in 0..RECOVERY_PROBATION_EVALUATIONS {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(
            governor.recovery_memory[memory_index],
            RecoveryRungMemory::default(),
            "a genuinely sustainable recovered rung must clear its old failure history"
        );

        // A later independent load increase degrades normally. Once load
        // drops again, recovery uses the base delay rather than stale backoff.
        evaluation(&mut governor, 3_000_000);
        for _ in 0..(RECOVERY_EVALUATIONS - 1) {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced
        );
        evaluation(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Full
        );
    }

    #[test]
    fn staged_transition_timings_are_counted_once_the_new_rung_is_adopted() {
        let mut governor = governor(1);
        for _ in 0..5 {
            evaluation(&mut governor, 3_000_000);
        }
        assert_eq!(governor.telemetry().reflection_output_gain, 0.0);
        let misses_before = governor.telemetry().callback_deadline_misses;

        governor.observe_block_timing(3_000_000);
        governor.observe_block_timing(3_000_000);

        assert_eq!(
            governor.telemetry().callback_deadline_misses,
            misses_before + 2,
            "transition advancement must not hide callback deadline misses"
        );
        assert_eq!(governor.timing_len, 1);
        assert_eq!(governor.timings[0], 3_000_000);
    }

    #[test]
    fn first_adopted_staged_rung_miss_is_charged_to_that_rung() {
        let mut governor = governor(1);
        for _ in 0..5 {
            evaluation(&mut governor, 3_000_000);
        }
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::DirectOnly
        );

        for _ in 0..RECOVERY_EVALUATIONS {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::Full
        );
        assert_eq!(governor.pending_render_phase, 1);

        // This is the first callback rendered with the recovered source DSP.
        // Its miss survives fade completion and fails Source(0) probation.
        governor.observe_block_timing(3_000_000);
        for _ in 1..EVALUATION_INTERVAL {
            governor.observe_block_timing(100_000);
        }
        let memory = governor.recovery_memory[RecoveryRung::Source(0).memory_index()];
        assert_eq!(memory.failures, 1);
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::DirectOnly
        );
    }

    #[test]
    fn virtualization_requires_physical_below_threshold_prediction() {
        let descriptors = [
            MultiSourceDescriptor::at(EnuVector3::default())
                .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 20.0 }),
            MultiSourceDescriptor::at(EnuVector3::default())
                .with_reference_level(ReferenceLevel::CreativeDb { db: -80.0 }),
        ];
        let (mut governor, _) = QualityGovernor::new(
            AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            },
            S3SimulationConfig::default(),
            &descriptors,
        );
        governor.observe_source_gain(0, 0.01);
        governor.observe_source_gain(1, 0.000_001);
        for _ in 0..5 {
            evaluation(&mut governor, 3_000_000);
        }
        settle_render_transition(&mut governor, 3_000_000);
        assert_eq!(
            governor.telemetry().sources[1].quality,
            SourceQualityLevel::DirectOnly,
            "creative-relative level cannot justify virtualization"
        );
        evaluation(&mut governor, 3_000_000);
        settle_render_transition(&mut governor, 3_000_000);
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::Virtualized
        );
    }

    #[test]
    fn sustained_simulation_lateness_enters_the_same_ordered_ladder() {
        let mut governor = governor(1);
        governor.observe_simulation_lateness(
            GovernorSimulationPass::Reflections,
            SIMULATION_LATENESS_TRIGGER_NS,
        );
        evaluation(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Reduced
        );
        assert_eq!(
            governor.telemetry().reason,
            GovernorTransitionReason::SimulationLate
        );
        assert_eq!(
            governor.telemetry().simulation_lateness_ns[2],
            SIMULATION_LATENESS_TRIGGER_NS
        );
    }
}
