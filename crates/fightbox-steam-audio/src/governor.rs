//! Control-side quality governor and its allocation-free render snapshot.
//!
//! Timing observations and decisions live with the simulation runner. The
//! audio graph receives only a complete immutable snapshot through the same
//! bounded SPSC channel used by the propagation and stage-gain paths.

#[cfg(any(feature = "linked-sdk", test))]
use crate::{AudioConfig, MultiSourceDescriptor, S3SimulationConfig};
use crate::{QualityTier, SessionMemoryTelemetry};
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
// A miss suspends every recovery rung, not just the rung that happened to be
// under probation. The first miss waits another full recovery interval; the
// exponential formula is retained even though the second miss locks recovery
// for the rest of the run.
#[cfg(any(feature = "linked-sdk", test))]
const GLOBAL_RECOVERY_LOCKOUT_EVALUATIONS: u32 = 8;
#[cfg(any(feature = "linked-sdk", test))]
const MAX_GLOBAL_RECOVERY_FAILURES: u8 = 2;
// A rung that overloads twice during probation is not a viable operating
// point for this run. Locking that exact rung bounds recovery-induced misses
// while leaving successful probation free to clear stale failure history.
#[cfg(any(feature = "linked-sdk", test))]
const MAX_RECOVERY_FAILURES: u8 = 2;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_P99_NUMERATOR: u64 = 7;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_P99_DENOMINATOR: u64 = 10;
// Predicted post-climb cost must fit below half of the callback period. This
// is the existing p99 budget and leaves the other half for scheduler jitter
// and estimator error.
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_DEADLINE_MARGIN_NUMERATOR: u64 = 1;
#[cfg(any(feature = "linked-sdk", test))]
const RECOVERY_DEADLINE_MARGIN_DENOMINATOR: u64 = 2;
#[cfg(any(feature = "linked-sdk", test))]
const SIMULATION_LATENESS_TRIGGER_NS: u64 = 5_000_000;
#[cfg(any(feature = "linked-sdk", test))]
const HEARING_THRESHOLD_DB_SPL: f32 = 0.0;
// Reflection budget study §γ measured a 173.750 us observed maximum for
// one 1 s, first-order (four-channel) source. Normalized, that is 43.4375 us
// per source-channel-second. Duration was close to linear in the same matrix;
// round upward here so the boot prior never understates that measured anchor.
#[cfg(any(feature = "linked-sdk", test))]
const PREDICTED_REFLECTION_NS_PER_SOURCE_CHANNEL_SECOND: u64 = 43_438;
#[cfg(any(feature = "linked-sdk", test))]
const BOOT_COST_LIMIT_NUMERATOR: u64 = 1;
#[cfg(any(feature = "linked-sdk", test))]
const BOOT_COST_LIMIT_DENOMINATOR: u64 = 2;
// A transient event is protected for three seconds of rendered blocks. The
// upcoming ballistic lane can re-arm this window at every event onset; three
// seconds covers the workbench artillery's complete trigger interval and
// prevents a governor transition from cutting its audible tail in half.
#[cfg(any(feature = "linked-sdk", test))]
const TRANSIENT_PROTECTION_WINDOW_NS: u64 = 3_000_000_000;

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
    /// Direct HRTF, occlusion, and baked pathing remain; reflections are faded
    /// out. Retaining pathing prevents an occluded source from losing the
    /// indirect transport that makes it audible around a corner.
    DirectOnly,
    /// Reserved for a physically calibrated prediction below the hearing threshold.
    Virtualized,
}

/// Scheduling importance attached to one stable source index.
///
/// `TransientEvent` alone does not reserve quality indefinitely. Its caller
/// re-arms the governor's documented three-second protection window at each
/// event onset; outside that window the source participates in the unchanged
/// audibility-ranked degradation policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SourcePriorityClass {
    #[default]
    Steady,
    TransientEvent,
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
    pub priority_class: SourcePriorityClass,
    /// Render blocks remaining in the current three-second transient window.
    pub transient_protection_remaining_blocks: u32,
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
            priority_class: SourcePriorityClass::Steady,
            transient_protection_remaining_blocks: 0,
            transport_advances: true,
        }
    }
}

/// Copyable delivered-quality and timing surface suitable for later CLI serialization.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QualityGovernorTelemetry {
    pub quality_tier: QualityTier,
    pub tier_source_cap: u8,
    pub sequence: u64,
    pub ladder_position: u16,
    pub reason: GovernorTransitionReason,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub p99_9_ns: u64,
    pub callback_deadline_misses: u64,
    pub simulation_lateness_ns: [u64; 3],
    /// Reflection rung selected once by the construction-time cost prior.
    pub boot_reflection_level: ReflectionQualityLevel,
    /// Predicted reflection render cost for `boot_reflection_level`.
    pub boot_predicted_cost_ns: u64,
    /// Ordinary p99 governor budget (half of one render-block period).
    pub boot_p99_budget_ns: u64,
    /// Boot admission limit, fixed at 50% of `boot_p99_budget_ns`.
    pub boot_cost_limit_ns: u64,
    pub reflections: DeliveredReflectionQuality,
    pub pathing: PathQualityLevel,
    pub ambisonic_order: i32,
    pub reverb: ReverbStrategy,
    pub reflection_output_gain: f32,
    pub sources: [SourceQualityTelemetry; MAX_ACTIVE_SOURCES],
    pub source_count: u8,
    pub memory: SessionMemoryTelemetry,
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
    baseline_window_max_ns: u64,
    observed_window_max_ns: u64,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RecoveryCostHistory {
    observed_increment_ns: u64,
    has_observation: bool,
}

#[cfg(any(feature = "linked-sdk", test))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GlobalRecoveryLockout {
    failures: u8,
    remaining_evaluations: u32,
    locked: bool,
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
    quality_tier: QualityTier,
    requested: S3SimulationConfig,
    source_count: usize,
    p99_budget_ns: u64,
    p99_9_ceiling_ns: u64,
    block_period_ns: u64,
    boot_reflection_level: ReflectionQualityLevel,
    boot_predicted_cost_ns: u64,
    boot_cost_limit_ns: u64,
    timings: [u64; TIMING_WINDOW],
    timing_next: usize,
    timing_len: usize,
    observations_since_evaluation: u32,
    headroom_evaluations: u32,
    deadline_misses: u64,
    simulation_lateness_ns: [u64; 3],
    simulation_late_since_evaluation: bool,
    reason: GovernorTransitionReason,
    render: GovernorRenderSnapshot,
    audibility: [SourceAudibility; MAX_ACTIVE_SOURCES],
    source_priorities: [SourcePriorityClass; MAX_ACTIVE_SOURCES],
    transient_protection_remaining_blocks: [u32; MAX_ACTIVE_SOURCES],
    writer: fightbox_runtime::SnapshotWriter<GovernorRenderSnapshot>,
    pending_render_change: Option<PendingRenderChange>,
    pending_render_phase: u8,
    recovery_memory: [RecoveryRungMemory; RECOVERY_RUNG_COUNT],
    recovery_cost_history: [RecoveryCostHistory; RECOVERY_RUNG_COUNT],
    recovery_probation: Option<RecoveryProbation>,
    global_recovery_lockout: GlobalRecoveryLockout,
    memory: SessionMemoryTelemetry,
}

#[cfg(any(feature = "linked-sdk", test))]
impl QualityGovernor {
    pub(crate) fn new(
        audio: AudioConfig,
        requested: S3SimulationConfig,
        descriptors: &[MultiSourceDescriptor],
        quality_tier: QualityTier,
        memory: SessionMemoryTelemetry,
    ) -> (
        Self,
        fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    ) {
        let block_period_ns = u64::try_from(audio.frame_size)
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            / u64::try_from(audio.sample_rate_hz).unwrap_or(1);
        let p99_budget_ns = block_period_ns / 2;
        let boot_cost_limit_ns =
            p99_budget_ns.saturating_mul(BOOT_COST_LIMIT_NUMERATOR) / BOOT_COST_LIMIT_DENOMINATOR;
        let (boot_reflection_level, boot_predicted_cost_ns) = predicted_cost_boot(
            requested,
            quality_tier,
            descriptors.len(),
            boot_cost_limit_ns,
        );
        let mut audibility = [SourceAudibility::default(); MAX_ACTIVE_SOURCES];
        for (index, descriptor) in descriptors.iter().enumerate() {
            audibility[index] = SourceAudibility {
                declared_level_db: descriptor.declared_level_db(),
                physically_calibrated: descriptor.is_physically_calibrated(),
                predicted_db: descriptor.declared_level_db(),
            };
        }
        let reflections =
            delivered_reflections(requested, quality_tier, boot_reflection_level, false);
        let mut sources = [SourceQualityLevel::Full; MAX_ACTIVE_SOURCES];
        for index in 0..descriptors.len() {
            sources[index] = boot_source_quality(audibility[index]);
        }
        let (validate_paths, find_alternate_paths, ambisonic_order, reverb) = match quality_tier {
            QualityTier::Desktop => (
                requested.validate_paths,
                requested.find_alternate_paths,
                requested.reflection_order,
                ReverbStrategy::SdkMixerConvolution,
            ),
            QualityTier::Mobile => (
                false,
                requested.find_alternate_paths,
                0,
                ReverbStrategy::ShortIrLowerOrder,
            ),
        };
        let initial = GovernorRenderSnapshot {
            sequence: 1,
            reflections,
            validate_paths,
            find_alternate_paths,
            ambisonic_order,
            reverb,
            reflection_output_gain: 1.0,
            sources,
            listener_centric_source: 0,
        };
        let (writer, reader) = SnapshotPublication::new(initial);
        (
            Self {
                quality_tier,
                requested,
                source_count: descriptors.len(),
                p99_budget_ns,
                p99_9_ceiling_ns: block_period_ns.saturating_mul(4) / 5,
                block_period_ns,
                boot_reflection_level,
                boot_predicted_cost_ns,
                boot_cost_limit_ns,
                timings: [0; TIMING_WINDOW],
                timing_next: 0,
                timing_len: 0,
                observations_since_evaluation: 0,
                headroom_evaluations: 0,
                deadline_misses: 0,
                simulation_lateness_ns: [0; 3],
                simulation_late_since_evaluation: false,
                reason: GovernorTransitionReason::Initial,
                render: initial,
                audibility,
                source_priorities: [SourcePriorityClass::Steady; MAX_ACTIVE_SOURCES],
                transient_protection_remaining_blocks: [0; MAX_ACTIVE_SOURCES],
                writer,
                pending_render_change: None,
                pending_render_phase: 0,
                recovery_memory: [RecoveryRungMemory::default(); RECOVERY_RUNG_COUNT],
                recovery_cost_history: [RecoveryCostHistory::default(); RECOVERY_RUNG_COUNT],
                recovery_probation: None,
                global_recovery_lockout: GlobalRecoveryLockout::default(),
                memory,
            },
            reader,
        )
    }

    #[cfg(feature = "linked-sdk")]
    pub(crate) const fn render_quality(&self) -> GovernorRenderSnapshot {
        self.render
    }

    /// Changes the persistent scheduling class for one source. Selecting
    /// `Steady` also cancels any outstanding transient protection.
    pub(crate) fn set_source_priority(
        &mut self,
        source_index: usize,
        priority: SourcePriorityClass,
    ) -> bool {
        if source_index >= self.source_count {
            return false;
        }
        self.source_priorities[source_index] = priority;
        if priority == SourcePriorityClass::Steady {
            self.transient_protection_remaining_blocks[source_index] = 0;
        }
        true
    }

    /// Re-arms the fixed three-second protection window at an event onset.
    ///
    /// The caller must first classify the source as `TransientEvent`. Protected
    /// sources are skipped by audibility-ranked degradation and are restored
    /// if an already-staged source demotion has not reached the next block.
    pub(crate) fn begin_source_transient(&mut self, source_index: usize) -> bool {
        if source_index >= self.source_count
            || self.source_priorities[source_index] != SourcePriorityClass::TransientEvent
        {
            return false;
        }
        self.transient_protection_remaining_blocks[source_index] =
            u32::try_from(TRANSIENT_PROTECTION_WINDOW_NS.div_ceil(self.block_period_ns.max(1)))
                .unwrap_or(u32::MAX);

        let pending_for_source = matches!(
            self.pending_render_change,
            Some(PendingRenderChange::SourceQuality {
                source_index: pending_index,
                ..
            }) if pending_index == source_index
        );
        let needs_restore = self.render.sources[source_index] != SourceQualityLevel::Full;
        if pending_for_source {
            self.pending_render_change = None;
            self.pending_render_phase = 0;
            self.render.reflection_output_gain = 1.0;
            if self
                .recovery_probation
                .is_some_and(|probation| probation.rung == RecoveryRung::Source(source_index))
            {
                self.recovery_probation = None;
            }
        }
        if needs_restore {
            self.render.sources[source_index] = SourceQualityLevel::Full;
        }
        if pending_for_source || needs_restore {
            self.publish();
        }
        true
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
        self.advance_transient_protection_windows();
        self.timings[self.timing_next] = elapsed_ns;
        self.timing_next = (self.timing_next + 1) % TIMING_WINDOW;
        self.timing_len = self.timing_len.saturating_add(1).min(TIMING_WINDOW);
        self.observations_since_evaluation += 1;
        if let Some(probation) = self
            .recovery_probation
            .as_mut()
            .filter(|probation| probation.adopted)
        {
            probation.observed_window_max_ns = probation.observed_window_max_ns.max(elapsed_ns);
        }
        if elapsed_ns >= self.block_period_ns {
            self.deadline_misses = self.deadline_misses.saturating_add(1);
            self.handle_deadline_miss(elapsed_ns);
            return;
        }

        if self.advance_pending_render_transition() == PendingTransitionAdvance::AdoptedQuality {
            // The elapsed sample belongs to the pre-adoption snapshot. Begin
            // the new rung's evidence window at the next rendered block.
            self.reset_timing_window();
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
                priority_class: self.source_priorities[index],
                transient_protection_remaining_blocks: self.transient_protection_remaining_blocks
                    [index],
                transport_advances: true,
            };
        }
        QualityGovernorTelemetry {
            quality_tier: self.quality_tier,
            tier_source_cap: self.quality_tier.active_source_cap() as u8,
            sequence: self.render.sequence,
            ladder_position: self.ladder_position(),
            reason: self.reason,
            p50_ns,
            p95_ns,
            p99_ns,
            p99_9_ns,
            callback_deadline_misses: self.deadline_misses,
            simulation_lateness_ns: self.simulation_lateness_ns,
            boot_reflection_level: self.boot_reflection_level,
            boot_predicted_cost_ns: self.boot_predicted_cost_ns,
            boot_p99_budget_ns: self.p99_budget_ns,
            boot_cost_limit_ns: self.boot_cost_limit_ns,
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
            memory: self.memory,
        }
    }

    fn evaluate(&mut self) {
        let (_, _, p99_ns, p99_9_ns) = self.percentiles();
        let reason = if p99_9_ns >= self.p99_9_ceiling_ns {
            Some(GovernorTransitionReason::RenderP999OverCeiling)
        } else if p99_ns >= self.p99_budget_ns {
            Some(GovernorTransitionReason::RenderP99OverBudget)
        } else if self.simulation_late_since_evaluation {
            Some(GovernorTransitionReason::SimulationLate)
        } else {
            None
        };
        self.simulation_late_since_evaluation = false;

        if let Some(reason) = reason {
            self.headroom_evaluations = 0;
            let degraded = if let Some(probation) = self.recovery_probation.take() {
                self.record_recovery_cost(probation);
                self.record_recovery_failure(probation.rung);
                self.rollback_recovery(probation)
            } else {
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

        if self.global_recovery_lockout.locked {
            self.headroom_evaluations = 0;
            return;
        }
        if self.global_recovery_lockout.remaining_evaluations > 0 {
            self.global_recovery_lockout.remaining_evaluations -= 1;
            self.headroom_evaluations = 0;
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
                    self.record_recovery_cost(probation);
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
            let current_window_max_ns = self.current_window_max_ns();
            if self.timing_len < TIMING_WINDOW
                || !self.recovery_margin_allows(rung, current_window_max_ns)
            {
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
                    baseline_window_max_ns: current_window_max_ns,
                    observed_window_max_ns: 0,
                });
                self.reason = GovernorTransitionReason::SustainedHeadroom;
                self.reset_timing_window();
                self.publish();
            }
        } else {
            self.headroom_evaluations = 0;
        }
    }

    fn handle_deadline_miss(&mut self, elapsed_ns: u64) {
        self.headroom_evaluations = 0;
        self.record_global_recovery_failure();
        let changed = if let Some(mut probation) = self.recovery_probation.take() {
            if probation.adopted {
                probation.observed_window_max_ns = probation.observed_window_max_ns.max(elapsed_ns);
                self.record_recovery_cost(probation);
                self.record_recovery_failure(probation.rung);
            }
            self.rollback_recovery(probation)
        } else if self.pending_render_change.is_none() {
            // Once probation has completed, the most recently earned rung is
            // the first rung in the frozen degradation order. Roll it back
            // immediately rather than waiting for the next evaluation.
            self.degrade_one()
        } else {
            // A prior rollback is already staged. Do not replace its bounded
            // fade transition, but still count this miss toward the global lock.
            false
        };
        self.reason = GovernorTransitionReason::RenderDeadlineMiss;
        self.reset_timing_window();
        if changed {
            self.publish();
        }
    }

    fn record_global_recovery_failure(&mut self) {
        let lockout = &mut self.global_recovery_lockout;
        lockout.failures = lockout.failures.saturating_add(1);
        lockout.locked = lockout.failures >= MAX_GLOBAL_RECOVERY_FAILURES;
        lockout.remaining_evaluations = if lockout.locked {
            0
        } else {
            GLOBAL_RECOVERY_LOCKOUT_EVALUATIONS
                .saturating_mul(1_u32 << u32::from(lockout.failures.saturating_sub(1)))
        };
    }

    fn record_recovery_cost(&mut self, probation: RecoveryProbation) {
        if !probation.adopted {
            return;
        }
        let observed_increment_ns = probation
            .observed_window_max_ns
            .saturating_sub(probation.baseline_window_max_ns);
        let history = &mut self.recovery_cost_history[probation.rung.memory_index()];
        history.observed_increment_ns = history.observed_increment_ns.max(observed_increment_ns);
        history.has_observation = true;
    }

    fn record_recovery_failure(&mut self, rung: RecoveryRung) {
        let memory = &mut self.recovery_memory[rung.memory_index()];
        memory.failures = memory.failures.saturating_add(1);
        memory.locked = memory.failures >= MAX_RECOVERY_FAILURES;
    }

    fn rollback_recovery(&mut self, probation: RecoveryProbation) -> bool {
        if probation.adopted {
            self.degrade_recovered_rung(probation.rung)
        } else {
            // The expensive state has not reached a rendered block. Cancelling
            // the staged climb restores the already-delivered lower rung.
            self.pending_render_change = None;
            self.pending_render_phase = 0;
            self.render.reflection_output_gain = 1.0;
            true
        }
    }

    fn recovery_margin_allows(&self, rung: RecoveryRung, current_window_max_ns: u64) -> bool {
        let increment_ns = self.recovery_increment_estimate_ns(rung, current_window_max_ns);
        let predicted_ns = current_window_max_ns.saturating_add(increment_ns);
        let limit_ns = self
            .block_period_ns
            .saturating_mul(RECOVERY_DEADLINE_MARGIN_NUMERATOR)
            / RECOVERY_DEADLINE_MARGIN_DENOMINATOR;
        predicted_ns <= limit_ns
    }

    fn recovery_increment_estimate_ns(
        &self,
        rung: RecoveryRung,
        current_window_max_ns: u64,
    ) -> u64 {
        let static_estimate_ns = self.static_recovery_increment_ns(rung, current_window_max_ns);
        let history = self.recovery_cost_history_for_estimate(rung);
        if history.has_observation {
            // A measured higher-rung delta receives a 50% error allowance.
            // Never let a noisy or negative window delta undercut the static
            // model used before the first observation.
            static_estimate_ns.max(history.observed_increment_ns.saturating_mul(3).div_ceil(2))
        } else {
            static_estimate_ns
        }
    }

    fn recovery_cost_history_for_estimate(&self, rung: RecoveryRung) -> RecoveryCostHistory {
        match rung {
            RecoveryRung::Source(_) => (0..self.source_count)
                .map(|index| self.recovery_cost_history[RecoveryRung::Source(index).memory_index()])
                .fold(RecoveryCostHistory::default(), |aggregate, history| {
                    RecoveryCostHistory {
                        observed_increment_ns: aggregate
                            .observed_increment_ns
                            .max(history.observed_increment_ns),
                        has_observation: aggregate.has_observation || history.has_observation,
                    }
                }),
            RecoveryRung::AmbisonicOrder(_) => (1..=self.requested.reflection_order)
                .map(|order| {
                    self.recovery_cost_history[RecoveryRung::AmbisonicOrder(order).memory_index()]
                })
                .fold(RecoveryCostHistory::default(), |aggregate, history| {
                    RecoveryCostHistory {
                        observed_increment_ns: aggregate
                            .observed_increment_ns
                            .max(history.observed_increment_ns),
                        has_observation: aggregate.has_observation || history.has_observation,
                    }
                }),
            _ => self.recovery_cost_history[rung.memory_index()],
        }
    }

    fn static_recovery_increment_ns(&self, rung: RecoveryRung, current_window_max_ns: u64) -> u64 {
        // These ratios estimate incremental whole-callback cost, not isolated
        // kernel cost. Source/path rungs affect a fraction of the graph;
        // reflection length and order touch broader mixing work. The absolute
        // floors keep an unusually quiet window from producing a zero estimate.
        let (ratio_numerator, ratio_denominator, minimum_deadline_divisor) = match rung {
            RecoveryRung::FullLengthReverb => (1, 1, 12),
            RecoveryRung::AmbisonicOrder(_) => (1, 2, 16),
            RecoveryRung::Source(_) => (1, 2, 12),
            RecoveryRung::AlternatePaths | RecoveryRung::PathValidation => (1, 8, 32),
            RecoveryRung::ReflectionReduced => (1, 2, 12),
            RecoveryRung::ReflectionFull => (1, 1, 8),
        };
        current_window_max_ns
            .saturating_mul(ratio_numerator)
            .div_ceil(ratio_denominator)
            .max(self.block_period_ns / minimum_deadline_divisor)
    }

    fn degrade_one(&mut self) -> bool {
        match self.render.reflections.level {
            ReflectionQualityLevel::Full => {
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Reduced,
                    false,
                );
                return true;
            }
            ReflectionQualityLevel::Reduced => {
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Minimum,
                    false,
                );
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
            self.begin_render_transition(PendingRenderChange::SourceQuality {
                source_index: index,
                quality: degraded_source_quality(self.audibility[index]),
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
                if self.quality_tier == QualityTier::Desktop {
                    return Some(RecoveryRung::FullLengthReverb);
                }
            }
            ReverbStrategy::SdkMixerConvolution
            | ReverbStrategy::Hybrid
            | ReverbStrategy::Baked
            | ReverbStrategy::ListenerCentric => {}
        }
        let ambisonic_ceiling = match self.quality_tier {
            QualityTier::Desktop => self.requested.reflection_order,
            QualityTier::Mobile => 0,
        };
        if self.render.ambisonic_order < ambisonic_ceiling {
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
        if self.quality_tier == QualityTier::Desktop
            && !self.render.validate_paths
            && self.requested.validate_paths
        {
            return Some(RecoveryRung::PathValidation);
        }
        match self.render.reflections.level {
            ReflectionQualityLevel::Minimum => Some(RecoveryRung::ReflectionReduced),
            ReflectionQualityLevel::Reduced if self.quality_tier == QualityTier::Desktop => {
                Some(RecoveryRung::ReflectionFull)
            }
            ReflectionQualityLevel::Reduced => None,
            ReflectionQualityLevel::Full => None,
        }
    }

    fn apply_recovery(&mut self, rung: RecoveryRung) {
        match rung {
            RecoveryRung::ReflectionFull => {
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Full,
                    false,
                );
            }
            RecoveryRung::ReflectionReduced => {
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Reduced,
                    false,
                );
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
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Reduced,
                    false,
                );
            }
            RecoveryRung::ReflectionReduced => {
                self.render.reflections = delivered_reflections(
                    self.requested,
                    self.quality_tier,
                    ReflectionQualityLevel::Minimum,
                    false,
                );
            }
            RecoveryRung::PathValidation => self.render.validate_paths = false,
            RecoveryRung::AlternatePaths => self.render.find_alternate_paths = false,
            RecoveryRung::Source(source_index) => {
                if self.source_is_transient_protected(source_index) {
                    // The earned source rung remains adopted for the event's
                    // audible window. Continue down the frozen ladder using
                    // the next degradable resource instead of cutting it.
                    return self.degrade_one();
                }
                self.begin_render_transition(PendingRenderChange::SourceQuality {
                    source_index,
                    quality: degraded_source_quality(self.audibility[source_index]),
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
            .filter(|index| {
                self.render.sources[*index] == SourceQualityLevel::Full
                    && !self.source_is_transient_protected(*index)
            })
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

    fn source_is_transient_protected(&self, source_index: usize) -> bool {
        self.source_priorities[source_index] == SourcePriorityClass::TransientEvent
            && self.transient_protection_remaining_blocks[source_index] > 0
    }

    fn advance_transient_protection_windows(&mut self) {
        for remaining in self.transient_protection_remaining_blocks[..self.source_count].iter_mut()
        {
            *remaining = remaining.saturating_sub(1);
        }
    }

    fn publish(&mut self) {
        self.render.sequence = self.render.sequence.wrapping_add(1);
        self.writer.publish(self.render);
    }

    fn begin_render_transition(&mut self, change: PendingRenderChange) {
        // Source demotions use the retained graph's existing reflection gain
        // seam: one block ramps to silence, the quality state changes at zero,
        // and the following block ramps back. Priority changes do not invent a
        // second smoother; they delay this same transition until protection
        // expires.
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
                        self.quality_tier,
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

    fn current_window_max_ns(&self) -> u64 {
        self.timings[..self.timing_len]
            .iter()
            .copied()
            .max()
            .unwrap_or(0)
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
fn predicted_cost_boot(
    requested: S3SimulationConfig,
    quality_tier: QualityTier,
    source_count: usize,
    cost_limit_ns: u64,
) -> (ReflectionQualityLevel, u64) {
    let candidates: &[ReflectionQualityLevel] = match quality_tier {
        QualityTier::Desktop => &[
            ReflectionQualityLevel::Full,
            ReflectionQualityLevel::Reduced,
            ReflectionQualityLevel::Minimum,
        ],
        QualityTier::Mobile => &[
            ReflectionQualityLevel::Reduced,
            ReflectionQualityLevel::Minimum,
        ],
    };
    let ambisonic_order = match quality_tier {
        QualityTier::Desktop => requested.reflection_order,
        QualityTier::Mobile => 0,
    };
    for &level in candidates {
        let delivered = delivered_reflections(requested, quality_tier, level, false);
        let predicted_cost_ns = predicted_reflection_render_cost_ns(
            source_count,
            delivered.bounces,
            delivered.ir_duration_s,
            ambisonic_order,
        );
        if predicted_cost_ns <= cost_limit_ns {
            return (level, predicted_cost_ns);
        }
    }
    // Minimum has zero bounces and therefore predicts zero convolution work.
    unreachable!("the minimum reflection rung must fit every boot budget")
}

#[cfg(any(feature = "linked-sdk", test))]
fn predicted_reflection_render_cost_ns(
    source_count: usize,
    bounces: i32,
    ir_duration_s: f32,
    ambisonic_order: i32,
) -> u64 {
    if source_count == 0 || bounces <= 0 || !ir_duration_s.is_finite() || ir_duration_s <= 0.0 {
        return 0;
    }
    // Reflection budget study §γ: cost = sources × IR seconds ×
    // (order + 1)^2 channels × 43,438 ns/source/channel-second.
    let order_extent = u64::try_from(ambisonic_order.max(0)).unwrap_or(0) + 1;
    let channels = order_extent.saturating_mul(order_extent);
    let duration_micros = (f64::from(ir_duration_s) * 1_000_000.0).ceil() as u64;
    u64::try_from(source_count)
        .unwrap_or(u64::MAX)
        .saturating_mul(channels)
        .saturating_mul(duration_micros)
        .saturating_mul(PREDICTED_REFLECTION_NS_PER_SOURCE_CHANNEL_SECOND)
        .div_ceil(1_000_000)
}

#[cfg(any(feature = "linked-sdk", test))]
fn boot_source_quality(audibility: SourceAudibility) -> SourceQualityLevel {
    if audibility.physically_calibrated && audibility.predicted_db < HEARING_THRESHOLD_DB_SPL {
        SourceQualityLevel::Virtualized
    } else {
        // Creative-relative and audible calibrated sources both start Full.
        // Calibration affects honest virtualization/ranking, never whether
        // startup reflections exist.
        SourceQualityLevel::Full
    }
}

#[cfg(any(feature = "linked-sdk", test))]
fn degraded_source_quality(audibility: SourceAudibility) -> SourceQualityLevel {
    if audibility.physically_calibrated && audibility.predicted_db < HEARING_THRESHOLD_DB_SPL {
        SourceQualityLevel::Virtualized
    } else {
        SourceQualityLevel::DirectOnly
    }
}

#[cfg(any(feature = "linked-sdk", test))]
fn delivered_reflections(
    requested: S3SimulationConfig,
    quality_tier: QualityTier,
    level: ReflectionQualityLevel,
    final_short_ir: bool,
) -> DeliveredReflectionQuality {
    let (ray_divisor, diffuse_divisor, bounce_reduction, duration_divisor, cadence_divisor) =
        match (quality_tier, level) {
            (QualityTier::Desktop, ReflectionQualityLevel::Full) => (1, 1, 0, 1.0, 1),
            (QualityTier::Desktop, ReflectionQualityLevel::Reduced) => (2, 2, 1, 2.0, 2),
            (QualityTier::Desktop, ReflectionQualityLevel::Minimum) => (4, 4, i32::MAX, 4.0, 4),
            // The Mobile construction defaults are already its maximum
            // resource envelope. "Reduced" names its tier ceiling and
            // therefore delivers those defaults without dividing them again.
            (QualityTier::Mobile, ReflectionQualityLevel::Reduced) => (1, 1, 0, 1.0, 2),
            (QualityTier::Mobile, ReflectionQualityLevel::Minimum) => (2, 2, i32::MAX, 2.0, 4),
            (QualityTier::Mobile, ReflectionQualityLevel::Full) => {
                debug_assert!(
                    false,
                    "mobile governor cannot reach desktop full reflections"
                );
                (1, 1, 0, 1.0, 2)
            }
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
            QualityTier::Desktop,
            SessionMemoryTelemetry::default(),
        )
        .0
    }

    fn reference_governor() -> QualityGovernor {
        let descriptors = (0..4)
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
                reflection_bounces: 3,
                reflection_duration_s: 1.5,
                reflection_order: 1,
                validate_paths: true,
                find_alternate_paths: true,
                ..S3SimulationConfig::default()
            },
            &descriptors,
            QualityTier::Desktop,
            SessionMemoryTelemetry::default(),
        )
        .0
    }

    fn force_conservative_floor(governor: &mut QualityGovernor) {
        governor.render.reflections = delivered_reflections(
            governor.requested,
            governor.quality_tier,
            ReflectionQualityLevel::Minimum,
            true,
        );
        governor.render.validate_paths = false;
        governor.render.find_alternate_paths = false;
        governor.render.ambisonic_order = 0;
        governor.render.reverb = ReverbStrategy::ShortIrLowerOrder;
        governor.render.reflection_output_gain = 1.0;
        for index in 0..governor.source_count {
            governor.render.sources[index] = degraded_source_quality(governor.audibility[index]);
        }
        governor.pending_render_change = None;
        governor.pending_render_phase = 0;
        governor.recovery_probation = None;
        governor.reset_timing_window();
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

    fn reach_probation(governor: &mut QualityGovernor, rung: RecoveryRung) {
        for _ in 0..10_000 {
            governor.observe_block_timing(100_000);
            if governor
                .recovery_probation
                .is_some_and(|probation| probation.rung == rung && probation.adopted)
                && governor.pending_render_change.is_none()
            {
                return;
            }
        }
        panic!(
            "did not reach probation for {rung:?}: candidate={:?}, reflections={:?}, memory={:?}, history={:?}, headroom={}, timing_len={}",
            governor.recovery_candidate(),
            governor.render.reflections.level,
            governor.recovery_memory[rung.memory_index()],
            governor.recovery_cost_history[rung.memory_index()],
            governor.headroom_evaluations,
            governor.timing_len,
        );
    }

    fn adopt_next_candidate(governor: &mut QualityGovernor) -> RecoveryRung {
        let rung = governor.recovery_candidate().expect("recovery candidate");
        governor.apply_recovery(rung);
        while governor.pending_render_change.is_some() {
            governor.advance_pending_render_transition();
        }
        rung
    }

    #[test]
    fn predicted_cost_boot_selects_reduced_for_the_reference_workbench() {
        let governor = reference_governor();
        let telemetry = governor.telemetry();

        assert_eq!(
            telemetry.boot_reflection_level,
            ReflectionQualityLevel::Reduced
        );
        assert_eq!(telemetry.reflections.level, ReflectionQualityLevel::Reduced);
        assert_ne!(
            telemetry.boot_reflection_level,
            ReflectionQualityLevel::Minimum
        );
        assert_ne!(
            telemetry.boot_reflection_level,
            ReflectionQualityLevel::Full
        );
        assert_eq!(telemetry.boot_predicted_cost_ns, 521_256);
        assert_eq!(telemetry.boot_p99_budget_ns, 1_333_333);
        assert_eq!(telemetry.boot_cost_limit_ns, 666_666);
        assert!(telemetry.boot_predicted_cost_ns <= telemetry.boot_cost_limit_ns);
        assert_eq!(
            predicted_reflection_render_cost_ns(4, 3, 1.5, 1),
            1_042_512,
            "the next higher rung must be rejected by the measured prior"
        );
        assert_eq!(telemetry.reflections.rays, 2_048);
        assert_eq!(telemetry.reflections.bounces, 2);
        assert_eq!(telemetry.reflections.ir_duration_s, 0.75);
        assert_eq!(telemetry.reflections.cadence_divisor, 2);
        assert_eq!(telemetry.ambisonic_order, 1);
        assert!(
            telemetry.sources[..4]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::Full)
        );
    }

    #[test]
    fn recovery_order_remains_frozen_from_the_conservative_floor() {
        let mut governor = governor(2);
        force_conservative_floor(&mut governor);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Minimum
        );
        assert_eq!(governor.telemetry().pathing, PathQualityLevel::PrimaryOnly);
        assert_eq!(governor.telemetry().ambisonic_order, 0);
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::ShortIrLowerOrder
        );
        assert!(
            governor.telemetry().sources[..2]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::DirectOnly)
        );

        let expected = [
            RecoveryRung::FullLengthReverb,
            RecoveryRung::AmbisonicOrder(1),
            RecoveryRung::Source(1),
            RecoveryRung::Source(0),
            RecoveryRung::AlternatePaths,
            RecoveryRung::PathValidation,
            RecoveryRung::ReflectionReduced,
            RecoveryRung::ReflectionFull,
        ];
        for rung in expected {
            assert_eq!(adopt_next_candidate(&mut governor), rung);
        }
        assert_eq!(governor.recovery_candidate(), None);
        assert_eq!(governor.telemetry().ladder_position, 0);
    }

    #[test]
    fn cold_start_under_stationary_overload_never_adopts_full_or_misses() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        let initial = governor.render;
        for _ in 0..256 {
            evaluation(&mut governor, 700_000);
        }
        assert_eq!(
            governor.render, initial,
            "the half-deadline margin must reject the first climb"
        );
        assert_eq!(governor.telemetry().callback_deadline_misses, 0);
        assert!(governor.recovery_probation.is_none());
    }

    #[test]
    fn measured_source_increment_is_reused_for_later_source_climbs() {
        let mut governor = governor(2);
        governor.recovery_cost_history[RecoveryRung::Source(1).memory_index()] =
            RecoveryCostHistory {
                observed_increment_ns: 400_000,
                has_observation: true,
            };
        assert_eq!(
            governor.recovery_increment_estimate_ns(RecoveryRung::Source(0), 100_000),
            600_000,
            "the 50% measurement allowance must dominate the static fallback"
        );
    }

    #[test]
    fn cold_start_under_genuine_headroom_climbs_to_full_without_misses() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        for _ in 0..20_000 {
            governor.observe_block_timing(100_000);
            if governor.telemetry().ladder_position == 0 && governor.recovery_probation.is_none() {
                break;
            }
        }
        assert_eq!(governor.telemetry().ladder_position, 0);
        assert_eq!(governor.telemetry().callback_deadline_misses, 0);
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::Full
        );
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Full
        );
    }

    #[test]
    fn miss_during_climb_rolls_back_and_globally_locks_out_recovery() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        reach_probation(&mut governor, RecoveryRung::FullLengthReverb);
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::SdkMixerConvolution
        );
        governor.observe_block_timing(3_000_000);
        assert_eq!(
            governor.telemetry().callback_deadline_misses,
            1,
            "the failing probation block must remain visible"
        );
        assert_eq!(
            governor.telemetry().reason,
            GovernorTransitionReason::RenderDeadlineMiss
        );
        assert!(governor.recovery_probation.is_none());
        assert_eq!(governor.global_recovery_lockout.failures, 1);
        assert_eq!(
            governor.global_recovery_lockout.remaining_evaluations,
            GLOBAL_RECOVERY_LOCKOUT_EVALUATIONS
        );
        assert!(!governor.global_recovery_lockout.locked);
        settle_render_transition(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::ShortIrLowerOrder
        );

        let sequence_during_lockout = governor.telemetry().sequence;
        while governor.global_recovery_lockout.remaining_evaluations > 0 {
            evaluation(&mut governor, 100_000);
        }
        assert_eq!(governor.telemetry().sequence, sequence_during_lockout);

        governor.observe_block_timing(3_000_000);
        assert!(governor.global_recovery_lockout.locked);
        assert_eq!(governor.global_recovery_lockout.failures, 2);
        for _ in 0..2_000 {
            governor.observe_block_timing(100_000);
        }
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::ShortIrLowerOrder
        );
    }

    #[test]
    fn per_rung_and_global_locks_compose() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        let rung = RecoveryRung::FullLengthReverb;
        let memory_index = rung.memory_index();

        for expected_failures in 1..=MAX_RECOVERY_FAILURES {
            reach_probation(&mut governor, rung);
            governor.observe_simulation_lateness(
                GovernorSimulationPass::Reflections,
                SIMULATION_LATENESS_TRIGGER_NS,
            );
            evaluation(&mut governor, 100_000);
            settle_render_transition(&mut governor, 100_000);
            assert_eq!(
                governor.recovery_memory[memory_index].failures,
                expected_failures
            );
        }
        assert!(governor.recovery_memory[memory_index].locked);
        assert_eq!(governor.global_recovery_lockout.failures, 0);

        governor.observe_block_timing(3_000_000);
        assert_eq!(governor.global_recovery_lockout.failures, 1);
        while governor.global_recovery_lockout.remaining_evaluations > 0 {
            evaluation(&mut governor, 100_000);
        }
        for _ in 0..2_000 {
            governor.observe_block_timing(100_000);
        }
        assert!(governor.recovery_memory[memory_index].locked);
        assert_eq!(
            governor.telemetry().reverb,
            ReverbStrategy::ShortIrLowerOrder,
            "global expiry must not erase the retained per-rung lock"
        );
    }

    #[test]
    fn predicted_boot_keeps_uncalibrated_and_audible_calibrated_sources_full() {
        let descriptors = [
            MultiSourceDescriptor::at(EnuVector3::default())
                .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: -20.0 }),
            MultiSourceDescriptor::at(EnuVector3::default())
                .with_reference_level(ReferenceLevel::CreativeDb { db: -80.0 }),
            MultiSourceDescriptor::at(EnuVector3::default())
                .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 85.0 }),
        ];
        let governor = QualityGovernor::new(
            AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            },
            S3SimulationConfig::default(),
            &descriptors,
            QualityTier::Desktop,
            SessionMemoryTelemetry::default(),
        )
        .0;
        assert_eq!(
            governor.telemetry().sources[1].quality,
            SourceQualityLevel::Full,
            "creative-relative level cannot justify either virtualization or a startup hole"
        );
        assert_eq!(
            governor.telemetry().sources[0].quality,
            SourceQualityLevel::Virtualized
        );
        assert_eq!(
            governor.telemetry().sources[2].quality,
            SourceQualityLevel::Full,
            "an audible calibrated source must carry reflections from boot"
        );
    }

    #[test]
    fn falsified_boot_demotes_and_failed_probation_locks_the_rung() {
        let mut governor = reference_governor();
        evaluation(&mut governor, 1_400_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Minimum,
            "measured p99 must override a wrong Reduced prediction"
        );
        assert_eq!(
            governor.telemetry().reason,
            GovernorTransitionReason::RenderP99OverBudget
        );

        let rung = RecoveryRung::ReflectionReduced;
        let memory_index = rung.memory_index();
        for expected_failures in 1..=MAX_RECOVERY_FAILURES {
            reach_probation(&mut governor, rung);
            evaluation(&mut governor, 1_400_000);
            assert_eq!(
                governor.telemetry().reflections.level,
                ReflectionQualityLevel::Minimum
            );
            assert_eq!(
                governor.recovery_memory[memory_index].failures,
                expected_failures
            );
            if expected_failures < MAX_RECOVERY_FAILURES {
                // Forget the measured delta to exercise the retained
                // probation-failure lock itself. In production the cost
                // history already rejects this falsified rung after one miss.
                governor.recovery_cost_history[memory_index] = RecoveryCostHistory::default();
            }
        }
        assert!(governor.recovery_memory[memory_index].locked);
        for _ in 0..4_000 {
            governor.observe_block_timing(100_000);
        }
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Minimum,
            "the rejected rung must not oscillate back into service"
        );
        assert!(governor.recovery_probation.is_none());
    }

    #[test]
    fn transient_priority_skips_source_demotion_under_budget_pressure() {
        let mut governor = governor(2);
        assert!(governor.set_source_priority(0, SourcePriorityClass::TransientEvent));
        assert!(governor.begin_source_transient(0));
        let initial_window = governor.telemetry().sources[0].transient_protection_remaining_blocks;
        assert!(initial_window >= 1_125);

        for _ in 0..5 {
            evaluation(&mut governor, 1_400_000);
        }
        assert!(matches!(
            governor.pending_render_change,
            Some(PendingRenderChange::SourceQuality {
                source_index: 1,
                quality: SourceQualityLevel::DirectOnly,
            })
        ));
        settle_render_transition(&mut governor, 1_400_000);
        assert_eq!(
            governor.telemetry().sources[1].quality,
            SourceQualityLevel::DirectOnly
        );

        for _ in 0..200 {
            governor.observe_block_timing(1_400_000);
        }
        let protected = governor.telemetry().sources[0];
        assert_eq!(
            protected.priority_class,
            SourcePriorityClass::TransientEvent
        );
        assert!(protected.transient_protection_remaining_blocks > 0);
        assert_eq!(protected.quality, SourceQualityLevel::Full);
    }

    #[test]
    fn expired_transient_priority_demotion_uses_existing_reflection_fade() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        governor.render.sources[0] = SourceQualityLevel::Full;
        assert!(governor.set_source_priority(0, SourcePriorityClass::TransientEvent));

        assert!(governor.degrade_one());
        assert_eq!(governor.render.reflection_output_gain, 0.0);
        assert_eq!(governor.render.sources[0], SourceQualityLevel::Full);
        assert!(matches!(
            governor.pending_render_change,
            Some(PendingRenderChange::SourceQuality {
                source_index: 0,
                ..
            })
        ));

        assert_eq!(
            governor.advance_pending_render_transition(),
            PendingTransitionAdvance::AdoptedQuality
        );
        assert_eq!(governor.render.reflection_output_gain, 0.0);
        assert_eq!(
            governor.render.sources[0],
            SourceQualityLevel::DirectOnly,
            "quality changes only while the reflection output is silent"
        );
        assert_eq!(
            governor.advance_pending_render_transition(),
            PendingTransitionAdvance::CompletedFade
        );
        assert_eq!(governor.render.reflection_output_gain, 1.0);
    }

    #[test]
    fn explicitly_steady_population_is_decision_identical_to_default() {
        let mut default_governor = governor(2);
        let mut explicit_steady = governor(2);
        for source_index in 0..2 {
            assert!(explicit_steady.set_source_priority(source_index, SourcePriorityClass::Steady));
        }

        for observation in 0..768 {
            let elapsed_ns = if observation < 160 {
                1_400_000
            } else {
                100_000
            };
            default_governor.observe_block_timing(elapsed_ns);
            explicit_steady.observe_block_timing(elapsed_ns);
            assert_eq!(explicit_steady.render, default_governor.render);
            assert_eq!(explicit_steady.telemetry(), default_governor.telemetry());
        }
    }

    #[test]
    fn simulation_lateness_cannot_descend_below_the_conservative_floor() {
        let mut governor = governor(1);
        force_conservative_floor(&mut governor);
        governor.observe_simulation_lateness(
            GovernorSimulationPass::Reflections,
            SIMULATION_LATENESS_TRIGGER_NS,
        );
        evaluation(&mut governor, 100_000);
        assert_eq!(
            governor.telemetry().reflections.level,
            ReflectionQualityLevel::Minimum
        );
        assert_eq!(
            governor.telemetry().reason,
            GovernorTransitionReason::AtMinimumQuality
        );
        assert_eq!(
            governor.telemetry().simulation_lateness_ns[2],
            SIMULATION_LATENESS_TRIGGER_NS
        );
    }

    #[test]
    fn sustained_headroom_cannot_recover_above_mobile_tier_ceiling() {
        let descriptors = (0..QualityTier::Mobile.active_source_cap())
            .map(|index| {
                MultiSourceDescriptor::at(EnuVector3::new(index as f32, 0.0, 0.0))
                    .with_reference_level(ReferenceLevel::CreativeDb { db: index as f32 })
            })
            .collect::<Vec<_>>();
        let mut governor = QualityGovernor::new(
            AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            },
            QualityTier::Mobile.simulation_defaults(),
            &descriptors,
            QualityTier::Mobile,
            SessionMemoryTelemetry::default(),
        )
        .0;

        for _ in 0..20_000 {
            governor.observe_block_timing(100_000);
            if governor.recovery_candidate().is_none()
                && governor.recovery_probation.is_none()
                && governor.pending_render_change.is_none()
            {
                break;
            }
        }

        let telemetry = governor.telemetry();
        assert_eq!(telemetry.quality_tier, QualityTier::Mobile);
        assert_eq!(telemetry.tier_source_cap, 4);
        assert_eq!(telemetry.reflections.level, ReflectionQualityLevel::Reduced);
        assert_eq!(telemetry.reflections.rays, 512);
        assert_eq!(telemetry.reflections.ir_duration_s, 0.5);
        assert_eq!(telemetry.reflections.cadence_divisor, 2);
        assert_eq!(telemetry.pathing, PathQualityLevel::NoValidation);
        assert_eq!(telemetry.ambisonic_order, 0);
        assert_eq!(telemetry.reverb, ReverbStrategy::ShortIrLowerOrder);
        assert!(
            telemetry.sources[..descriptors.len()]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::Full)
        );
        assert_eq!(governor.recovery_candidate(), None);
    }
}
