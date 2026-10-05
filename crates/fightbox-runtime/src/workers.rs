//! Cadenced backend simulation on a non-audio thread.

use crate::backend::{
    SIMULATION_LATENESS_TRIGGER_NS, SimulationError, SimulationPass, SimulationRunner,
    SimulationUpdate,
};
use crate::{SnapshotPublication, SnapshotWriter, TimingHistory};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SimulationCadences {
    pub direct_hz: u32,
    pub pathing_hz: u32,
    pub reflections_hz: u32,
    /// Maximum listener or active-source travel between reflection IRs.
    pub reflection_max_displacement_m: f32,
    /// Hard ceiling for motion-triggered and periodic reflection passes.
    pub reflection_max_hz: u32,
}

impl Default for SimulationCadences {
    fn default() -> Self {
        Self {
            direct_hz: 60,
            pathing_hz: 15,
            reflections_hz: 5,
            reflection_max_displacement_m: Self::DEFAULT_REFLECTION_MAX_DISPLACEMENT_M,
            reflection_max_hz: Self::DEFAULT_REFLECTION_MAX_HZ,
        }
    }
}

impl SimulationCadences {
    /// Default spatial bound between published reflection IRs.
    pub const DEFAULT_REFLECTION_MAX_DISPLACEMENT_M: f32 = 1.0;
    /// Default CPU bound for reflection simulation, even under fast motion.
    pub const DEFAULT_REFLECTION_MAX_HZ: u32 = 25;

    fn periods(self) -> Result<([Duration; 3], Duration), SimulationWorkerError> {
        if self.direct_hz == 0
            || self.pathing_hz == 0
            || self.reflections_hz == 0
            || self.reflection_max_hz < self.reflections_hz
            || !self.reflection_max_displacement_m.is_finite()
            || self.reflection_max_displacement_m <= 0.0
        {
            return Err(SimulationWorkerError::InvalidCadence);
        }
        Ok((
            [
                Duration::from_secs_f64(1.0 / f64::from(self.direct_hz)),
                Duration::from_secs_f64(1.0 / f64::from(self.pathing_hz)),
                Duration::from_secs_f64(1.0 / f64::from(self.reflections_hz)),
            ],
            Duration::from_secs_f64(1.0 / f64::from(self.reflection_max_hz)),
        ))
    }
}

#[derive(Clone, Debug, Default)]
pub struct SimulationPassTelemetry {
    pub timings: TimingHistory,
    pub failures: u64,
}

#[derive(Clone, Debug, Default)]
pub struct SimulationWorkerTelemetry {
    pub direct: SimulationPassTelemetry,
    pub pathing: SimulationPassTelemetry,
    pub reflections: SimulationPassTelemetry,
}

/// Deadline provenance for one simulation pass lane.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimulationSchedulerPassTelemetry {
    /// Largest lateness already accumulated when a positive park returned.
    ///
    /// This may include operating-system wake delay, an intentional reflection
    /// max-rate eligibility wait, or both. It is excluded from worker-busy
    /// pressure.
    pub parked_lateness_max_ns: u64,
    /// Scheduled passes with positive parked lateness. Startup and reflection
    /// passes that begin before their periodic deadline are excluded.
    pub parked_lateness_count: u64,
    /// Largest worker-busy spill which met the actionable lateness threshold.
    pub actionable_worker_lateness_max_ns: u64,
    /// Worker-busy spill observations which met the actionable threshold.
    pub actionable_worker_lateness_count: u64,
}

/// Scheduler-deadline provenance kept separately from legacy pass telemetry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimulationSchedulerTelemetry {
    pub direct: SimulationSchedulerPassTelemetry,
    pub pathing: SimulationSchedulerPassTelemetry,
    pub reflections: SimulationSchedulerPassTelemetry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationWorkerError {
    InvalidCadence,
    ThreadSpawn,
}

#[derive(Clone, Copy, Debug)]
enum PreviousWorkerWait {
    Startup,
    Parked,
    NoPark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BatchLatenessAttribution {
    Startup,
    Parked { batch_started: Instant },
    WorkerAlreadyBehind,
}

impl PreviousWorkerWait {
    fn attribution(self, batch_started: Instant) -> BatchLatenessAttribution {
        match self {
            Self::Startup => BatchLatenessAttribution::Startup,
            Self::Parked => BatchLatenessAttribution::Parked { batch_started },
            Self::NoPark => BatchLatenessAttribution::WorkerAlreadyBehind,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PassStartLateness {
    parked_lateness_ns: u64,
    worker_busy_ns: u64,
}

struct SimulationDiagnosticSample {
    pass: SimulationPass,
    started_ns: u64,
    elapsed_ns: u64,
    lateness: PassStartLateness,
}

impl BatchLatenessAttribution {
    fn pass_start_lateness(
        self,
        deadline: Instant,
        pass_started: Instant,
    ) -> Option<PassStartLateness> {
        let pass_start_lateness_ns = duration_ns(pass_started.saturating_duration_since(deadline));
        match self {
            Self::Startup => None,
            Self::Parked { batch_started } => {
                let parked_lateness_ns =
                    duration_ns(batch_started.saturating_duration_since(deadline));
                Some(PassStartLateness {
                    parked_lateness_ns,
                    worker_busy_ns: pass_start_lateness_ns.saturating_sub(parked_lateness_ns),
                })
            }
            Self::WorkerAlreadyBehind => Some(PassStartLateness {
                parked_lateness_ns: 0,
                worker_busy_ns: pass_start_lateness_ns,
            }),
        }
    }
}

/// Owns one backend runner on one dedicated simulation thread.
///
/// A single thread deliberately multiplexes all three passes. Reflection
/// motion updates are capped separately, and per-pass timing telemetry keeps
/// their headroom against the direct period measurable without adding
/// synchronization. The runner trait already splits the passes, so a measured
/// future need can move them to separate threads without changing the backend
/// seam.
pub struct SimulationWorker {
    updates: SnapshotWriter<SimulationUpdate>,
    stop: Arc<AtomicBool>,
    telemetry: Arc<Mutex<SimulationWorkerTelemetry>>,
    scheduler_telemetry: Arc<Mutex<SimulationSchedulerTelemetry>>,
    thread: Option<JoinHandle<()>>,
}

impl SimulationWorker {
    pub fn new(
        runner: Box<dyn SimulationRunner>,
        initial_update: SimulationUpdate,
        cadences: SimulationCadences,
    ) -> Result<Self, SimulationWorkerError> {
        let (periods, reflection_min_period) = cadences.periods()?;
        let (updates, mut update_reader) = SnapshotPublication::new(initial_update);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let telemetry = Arc::new(Mutex::new(SimulationWorkerTelemetry::default()));
        let thread_telemetry = Arc::clone(&telemetry);
        let scheduler_telemetry = Arc::new(Mutex::new(SimulationSchedulerTelemetry::default()));
        let thread_scheduler_telemetry = Arc::clone(&scheduler_telemetry);
        let diagnostic_profile = std::env::var_os("FIGHTBOX_SIMULATION_PROFILE").is_some();

        let thread = thread::Builder::new()
            .name("fightbox-simulation".into())
            .spawn(move || {
                let mut runner = runner;
                let mut deadlines = [Instant::now(); 3];
                let mut next_reflection_eligible = deadlines[2];
                let mut reflection_displacement = ReflectionDisplacement::new(initial_update);
                let mut reflection_costs = [Duration::ZERO; 8];
                let mut reflection_cost_next = 0;
                let mut previous_wait = PreviousWorkerWait::Startup;
                let diagnostic_started = Instant::now();
                let mut diagnostic_samples = diagnostic_profile.then(|| Vec::with_capacity(4096));
                while !thread_stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    let batch_attribution = previous_wait.attribution(now);
                    let update = update_reader.read();
                    reflection_displacement.observe(update);
                    let due = [
                        now >= deadlines[0],
                        now >= deadlines[1],
                        reflection_tick_due(
                            now,
                            deadlines[2],
                            next_reflection_eligible,
                            reflection_displacement
                                .exceeded(cadences.reflection_max_displacement_m),
                        ),
                    ];
                    if due.iter().any(|is_due| *is_due) {
                        runner.update_inputs(&update);
                    }

                    if due[0] {
                        let pass_started = Instant::now();
                        observe_pass_start_lateness(
                            runner.as_mut(),
                            &thread_scheduler_telemetry,
                            SimulationPass::Direct,
                            deadlines[0],
                            pass_started,
                            batch_attribution,
                            true,
                        );
                        let elapsed = record_pass(&thread_telemetry, 0, || runner.run_direct());
                        record_diagnostic_pass(&mut diagnostic_samples, diagnostic_started,
                            SimulationPass::Direct, deadlines[0], pass_started,
                            batch_attribution, elapsed, true);
                        advance_deadline(&mut deadlines[0], periods[0]);
                    }
                    if due[1] {
                        let pass_started = Instant::now();
                        observe_pass_start_lateness(
                            runner.as_mut(),
                            &thread_scheduler_telemetry,
                            SimulationPass::Pathing,
                            deadlines[1],
                            pass_started,
                            batch_attribution,
                            true,
                        );
                        let elapsed = record_pass(&thread_telemetry, 1, || runner.run_pathing());
                        record_diagnostic_pass(&mut diagnostic_samples, diagnostic_started,
                            SimulationPass::Pathing, deadlines[1], pass_started,
                            batch_attribution, elapsed, true);
                        advance_deadline(&mut deadlines[1], periods[1]);
                    }
                    // Start an uninterruptible reflection job after the next
                    // direct tick when its measured work will not fit before
                    // that tick. A fresh direct batch always permits the job;
                    // real overruns therefore still reach the governor.
                    let reflection_deferred = due[2] && reflection_job_should_wait(
                        due[0], Instant::now(), deadlines[0],
                        reflection_costs.iter().copied().max().unwrap_or_default(),
                    );
                    if due[2] && !reflection_deferred {
                        let reflection_started = Instant::now();
                        let periodic_reflection_due_at_start =
                            periodic_reflection_due(reflection_started, deadlines[2]);
                        observe_pass_start_lateness(
                            runner.as_mut(),
                            &thread_scheduler_telemetry,
                            SimulationPass::Reflections,
                            deadlines[2],
                            reflection_started,
                            batch_attribution,
                            periodic_reflection_due_at_start,
                        );
                        reflection_costs[reflection_cost_next] =
                            record_pass(&thread_telemetry, 2, || runner.run_reflections());
                        record_diagnostic_pass(&mut diagnostic_samples, diagnostic_started,
                            SimulationPass::Reflections, deadlines[2], reflection_started,
                            batch_attribution, reflection_costs[reflection_cost_next],
                            periodic_reflection_due_at_start);
                        reflection_cost_next = (reflection_cost_next + 1) % reflection_costs.len();
                        reflection_displacement.reset();
                        next_reflection_eligible = reflection_started + reflection_min_period;
                        if periodic_reflection_due_at_start {
                            advance_deadline(&mut deadlines[2], periods[2]);
                        }
                    }

                    let wake_now = Instant::now();
                    let reflection_requested = wake_now >= deadlines[2]
                        || reflection_displacement.exceeded(cadences.reflection_max_displacement_m);
                    let reflection_wake = if reflection_deferred {
                        deadlines[0]
                    } else if reflection_requested {
                        if next_reflection_eligible > wake_now {
                            next_reflection_eligible
                        } else {
                            wake_now
                        }
                    } else {
                        deadlines[2]
                    };
                    let next = deadlines[0].min(deadlines[1]).min(reflection_wake);
                    let before_park = Instant::now();
                    let park_duration = next.saturating_duration_since(before_park);
                    previous_wait = if park_duration.is_zero() {
                        PreviousWorkerWait::NoPark
                    } else {
                        PreviousWorkerWait::Parked
                    };
                    thread::park_timeout(park_duration);
                }
                if let Some(samples) = diagnostic_samples {
                    for sample in samples {
                        eprintln!("SIMULATION_PASS pass={:?} started_ns={} elapsed_ns={} parked_lateness_ns={} busy_lateness_ns={}",
                            sample.pass, sample.started_ns, sample.elapsed_ns,
                            sample.lateness.parked_lateness_ns, sample.lateness.worker_busy_ns);
                    }
                }
            })
            .map_err(|_| SimulationWorkerError::ThreadSpawn)?;

        Ok(Self {
            updates,
            stop,
            telemetry,
            scheduler_telemetry,
            thread: Some(thread),
        })
    }

    /// Publishes the latest complete motion frame from the control side.
    pub fn publish_update(&mut self, update: SimulationUpdate) {
        self.updates.publish(update);
        if let Some(thread) = &self.thread {
            thread.thread().unpark();
        }
    }

    /// Takes a control-side telemetry snapshot. This mutex is never observed
    /// by the audio callback.
    #[must_use]
    pub fn telemetry(&self) -> SimulationWorkerTelemetry {
        match self.telemetry.lock() {
            Ok(telemetry) => telemetry.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Takes a control-side snapshot of scheduler deadline provenance.
    ///
    /// This remains separate from [`Self::telemetry`] so existing users can
    /// construct and destructure the legacy pass telemetry types unchanged.
    #[must_use]
    pub fn scheduler_telemetry(&self) -> SimulationSchedulerTelemetry {
        match self.scheduler_telemetry.lock() {
            Ok(telemetry) => *telemetry,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

impl Drop for SimulationWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn advance_deadline(deadline: &mut Instant, period: Duration) {
    let now = Instant::now();
    while *deadline <= now {
        *deadline += period;
    }
}

fn reflection_tick_due(
    now: Instant,
    periodic_deadline: Instant,
    next_eligible: Instant,
    displacement_exceeded: bool,
) -> bool {
    now >= next_eligible && (now >= periodic_deadline || displacement_exceeded)
}

fn periodic_reflection_due(reflection_started: Instant, periodic_deadline: Instant) -> bool {
    reflection_started >= periodic_deadline
}

fn observe_pass_start_lateness(
    runner: &mut dyn SimulationRunner,
    telemetry: &Mutex<SimulationSchedulerTelemetry>,
    pass: SimulationPass,
    deadline: Instant,
    pass_started: Instant,
    batch_attribution: BatchLatenessAttribution,
    scheduled_deadline_due: bool,
) {
    if !scheduled_deadline_due {
        return;
    }
    let Some(lateness) = batch_attribution.pass_start_lateness(deadline, pass_started) else {
        return;
    };
    record_scheduler_lateness(telemetry, pass, lateness);
    if lateness.worker_busy_ns >= SIMULATION_LATENESS_TRIGGER_NS {
        runner.observe_simulation_lateness(pass, lateness.worker_busy_ns);
    }
}

fn record_scheduler_lateness(
    telemetry: &Mutex<SimulationSchedulerTelemetry>,
    pass: SimulationPass,
    lateness: PassStartLateness,
) {
    let mut telemetry = match telemetry.lock() {
        Ok(telemetry) => telemetry,
        Err(poisoned) => poisoned.into_inner(),
    };
    let pass = match pass {
        SimulationPass::Direct => &mut telemetry.direct,
        SimulationPass::Pathing => &mut telemetry.pathing,
        SimulationPass::Reflections => &mut telemetry.reflections,
    };
    if lateness.parked_lateness_ns > 0 {
        pass.parked_lateness_max_ns = pass.parked_lateness_max_ns.max(lateness.parked_lateness_ns);
        pass.parked_lateness_count = pass.parked_lateness_count.saturating_add(1);
    }
    if lateness.worker_busy_ns >= SIMULATION_LATENESS_TRIGGER_NS {
        pass.actionable_worker_lateness_max_ns = pass
            .actionable_worker_lateness_max_ns
            .max(lateness.worker_busy_ns);
        pass.actionable_worker_lateness_count =
            pass.actionable_worker_lateness_count.saturating_add(1);
    }
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

struct ReflectionDisplacement {
    previous: SimulationUpdate,
    listener_m: f64,
    sources_m: [f64; crate::MAX_ACTIVE_SOURCES],
}

impl ReflectionDisplacement {
    fn new(initial: SimulationUpdate) -> Self {
        Self {
            previous: initial,
            listener_m: 0.0,
            sources_m: [0.0; crate::MAX_ACTIVE_SOURCES],
        }
    }

    fn observe(&mut self, current: SimulationUpdate) {
        self.listener_m += position_distance(
            self.previous.listener.pose.position,
            current.listener.pose.position,
        );
        for ((distance_m, previous), current) in self
            .sources_m
            .iter_mut()
            .zip(&self.previous.sources)
            .zip(&current.sources)
        {
            if previous.active && current.active {
                *distance_m += position_distance(previous.pose.position, current.pose.position);
            } else {
                *distance_m = 0.0;
            }
        }
        self.previous = current;
    }

    fn exceeded(&self, max_displacement_m: f32) -> bool {
        let max_displacement_m = f64::from(max_displacement_m);
        self.listener_m >= max_displacement_m
            || self
                .sources_m
                .iter()
                .any(|distance_m| *distance_m >= max_displacement_m)
    }

    fn reset(&mut self) {
        self.listener_m = 0.0;
        self.sources_m.fill(0.0);
    }
}

fn position_distance(left: fightbox_api::EnuVector3, right: fightbox_api::EnuVector3) -> f64 {
    let east = f64::from(right.east_m) - f64::from(left.east_m);
    let north = f64::from(right.north_m) - f64::from(left.north_m);
    let up = f64::from(right.up_m) - f64::from(left.up_m);
    (east * east + north * north + up * up).sqrt()
}

fn reflection_job_should_wait(
    direct_due: bool,
    now: Instant,
    next_direct: Instant,
    measured_cost: Duration,
) -> bool {
    !direct_due && !measured_cost.is_zero()
        && measured_cost + Duration::from_millis(1) > next_direct.saturating_duration_since(now)
}

fn record_pass(
    telemetry: &Mutex<SimulationWorkerTelemetry>,
    pass: usize,
    operation: impl FnOnce() -> Result<(), SimulationError>,
) -> Duration {
    let started = Instant::now();
    let result = operation();
    let elapsed = started.elapsed();
    let duration_ns = duration_ns(elapsed);
    let mut telemetry = match telemetry.lock() {
        Ok(telemetry) => telemetry,
        Err(poisoned) => poisoned.into_inner(),
    };
    let pass = match pass {
        0 => &mut telemetry.direct,
        1 => &mut telemetry.pathing,
        _ => &mut telemetry.reflections,
    };
    pass.timings.record(duration_ns);
    if result.is_err() {
        pass.failures = pass.failures.saturating_add(1);
    }
    elapsed
}

fn record_diagnostic_pass(
    samples: &mut Option<Vec<SimulationDiagnosticSample>>,
    diagnostic_started: Instant,
    pass: SimulationPass,
    deadline: Instant,
    pass_started: Instant,
    batch_attribution: BatchLatenessAttribution,
    elapsed: Duration,
    scheduled_deadline_due: bool,
) {
    let Some(samples) = samples.as_mut().filter(|samples| samples.len() < 4096) else {
        return;
    };
    samples.push(SimulationDiagnosticSample {
        pass,
        started_ns: duration_ns(pass_started.saturating_duration_since(diagnostic_started)),
        elapsed_ns: duration_ns(elapsed),
        lateness: if scheduled_deadline_due {
            batch_attribution.pass_start_lateness(deadline, pass_started).unwrap_or_default()
        } else {
            PassStartLateness::default()
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SourceMotion;
    use fightbox_api::{EnuVector3, ListenerState, Pose};
    use std::sync::atomic::AtomicU64;

    #[test]
    fn reflection_jobs_fit_between_direct_ticks_without_starving_overload() {
        let now = Instant::now();
        let cost = Duration::from_millis(10);
        assert!(reflection_job_should_wait(false, now, now + Duration::from_millis(3), cost));
        assert!(!reflection_job_should_wait(false, now, now + Duration::from_millis(16), cost));
        assert!(!reflection_job_should_wait(true, now, now + Duration::from_millis(16), Duration::from_millis(30)));
    }

    struct CountingRunner {
        direct: Arc<AtomicU64>,
        pathing: Arc<AtomicU64>,
        reflections: Arc<AtomicU64>,
    }

    impl SimulationRunner for CountingRunner {
        fn update_inputs(&mut self, _update: &SimulationUpdate) {}

        fn run_direct(&mut self) -> Result<(), SimulationError> {
            self.direct.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        fn run_pathing(&mut self) -> Result<(), SimulationError> {
            self.pathing.fetch_add(1, Ordering::Relaxed);
            Err(SimulationError::KernelFailure)
        }

        fn run_reflections(&mut self) -> Result<(), SimulationError> {
            self.reflections.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[derive(Default)]
    struct LatenessRecordingRunner {
        observations: Vec<(SimulationPass, u64)>,
    }

    impl SimulationRunner for LatenessRecordingRunner {
        fn update_inputs(&mut self, _update: &SimulationUpdate) {}

        fn observe_simulation_lateness(&mut self, pass: SimulationPass, lateness_ns: u64) {
            self.observations.push((pass, lateness_ns));
        }

        fn run_direct(&mut self) -> Result<(), SimulationError> {
            Ok(())
        }

        fn run_pathing(&mut self) -> Result<(), SimulationError> {
            Ok(())
        }

        fn run_reflections(&mut self) -> Result<(), SimulationError> {
            Ok(())
        }
    }

    fn update() -> SimulationUpdate {
        SimulationUpdate {
            listener: ListenerState {
                pose: Pose {
                    position: EnuVector3::default(),
                    forward: EnuVector3::new(0.0, 1.0, 0.0),
                    up: EnuVector3::new(0.0, 0.0, 1.0),
                },
                linear_velocity_mps: EnuVector3::default(),
            },
            sources: [SourceMotion::default(); crate::MAX_ACTIVE_SOURCES],
        }
    }

    #[test]
    fn legacy_worker_telemetry_struct_literals_keep_their_exact_field_shapes() {
        let pass = || SimulationPassTelemetry {
            timings: TimingHistory::default(),
            failures: 0,
        };
        let telemetry = SimulationWorkerTelemetry {
            direct: pass(),
            pathing: pass(),
            reflections: pass(),
        };

        assert_eq!(telemetry.direct.failures, 0);
        assert!(telemetry.pathing.timings.is_empty());
    }

    #[test]
    fn startup_and_early_displacement_reflection_emit_no_lateness() {
        let origin = Instant::now();
        let mut runner = LatenessRecordingRunner::default();
        let telemetry = Mutex::new(SimulationSchedulerTelemetry::default());

        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Direct,
            origin,
            origin + Duration::from_millis(20),
            BatchLatenessAttribution::Startup,
            true,
        );
        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Reflections,
            origin,
            origin + Duration::from_millis(20),
            BatchLatenessAttribution::WorkerAlreadyBehind,
            false,
        );

        assert!(runner.observations.is_empty());
        assert_eq!(
            telemetry.into_inner().unwrap().direct.parked_lateness_count,
            0
        );
    }

    #[test]
    fn parked_batch_excludes_per_lane_parked_lateness_and_keeps_order() {
        let origin = Instant::now();
        let batch_started = origin + Duration::from_millis(10);
        let attribution = PreviousWorkerWait::Parked.attribution(batch_started);
        assert_eq!(
            attribution,
            BatchLatenessAttribution::Parked { batch_started }
        );
        let mut runner = LatenessRecordingRunner::default();
        let telemetry = Mutex::new(SimulationSchedulerTelemetry::default());

        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Direct,
            origin,
            batch_started,
            attribution,
            true,
        );
        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Pathing,
            origin + Duration::from_millis(5),
            origin + Duration::from_millis(15),
            attribution,
            true,
        );
        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Reflections,
            origin + Duration::from_millis(7),
            origin + Duration::from_millis(18),
            attribution,
            true,
        );

        assert_eq!(
            runner.observations,
            vec![
                (SimulationPass::Pathing, SIMULATION_LATENESS_TRIGGER_NS),
                (SimulationPass::Reflections, 8_000_000),
            ]
        );
        let telemetry = telemetry.into_inner().unwrap();
        assert_eq!(telemetry.direct.parked_lateness_max_ns, 10_000_000);
        assert_eq!(telemetry.direct.parked_lateness_count, 1);
        assert_eq!(telemetry.direct.actionable_worker_lateness_count, 0);
        assert_eq!(telemetry.pathing.parked_lateness_max_ns, 5_000_000);
        assert_eq!(
            telemetry.pathing.actionable_worker_lateness_max_ns,
            5_000_000
        );
        assert_eq!(telemetry.pathing.actionable_worker_lateness_count, 1);
        assert_eq!(telemetry.reflections.parked_lateness_max_ns, 3_000_000);
        assert_eq!(
            telemetry.reflections.actionable_worker_lateness_max_ns,
            8_000_000
        );
    }

    #[test]
    fn no_park_uses_full_lateness_and_threshold_is_inclusive() {
        let origin = Instant::now();
        let attribution = PreviousWorkerWait::NoPark.attribution(origin);
        assert_eq!(attribution, BatchLatenessAttribution::WorkerAlreadyBehind);
        let mut runner = LatenessRecordingRunner::default();
        let telemetry = Mutex::new(SimulationSchedulerTelemetry::default());

        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Direct,
            origin,
            origin + Duration::from_nanos(SIMULATION_LATENESS_TRIGGER_NS - 1),
            attribution,
            true,
        );
        observe_pass_start_lateness(
            &mut runner,
            &telemetry,
            SimulationPass::Direct,
            origin,
            origin + Duration::from_nanos(SIMULATION_LATENESS_TRIGGER_NS),
            attribution,
            true,
        );

        assert_eq!(
            runner.observations,
            vec![(SimulationPass::Direct, SIMULATION_LATENESS_TRIGGER_NS)]
        );
        let telemetry = telemetry.into_inner().unwrap();
        assert_eq!(telemetry.direct.parked_lateness_count, 0);
        assert_eq!(telemetry.direct.actionable_worker_lateness_count, 1);
        assert_eq!(
            telemetry.direct.actionable_worker_lateness_max_ns,
            SIMULATION_LATENESS_TRIGGER_NS
        );
    }

    #[test]
    fn displacement_reflection_uses_actual_pass_start_for_periodic_due_state() {
        let origin = Instant::now();
        let periodic_deadline = origin + Duration::from_millis(10);

        assert!(!periodic_reflection_due(
            origin + Duration::from_millis(9),
            periodic_deadline
        ));
        assert!(periodic_reflection_due(
            periodic_deadline,
            periodic_deadline
        ));
        assert!(periodic_reflection_due(
            origin + Duration::from_millis(15),
            periodic_deadline
        ));
    }

    #[test]
    fn one_worker_multiplexes_passes_and_records_failures() {
        let direct = Arc::new(AtomicU64::new(0));
        let pathing = Arc::new(AtomicU64::new(0));
        let reflections = Arc::new(AtomicU64::new(0));
        let runner = CountingRunner {
            direct: Arc::clone(&direct),
            pathing: Arc::clone(&pathing),
            reflections: Arc::clone(&reflections),
        };
        let mut worker = SimulationWorker::new(
            Box::new(runner),
            update(),
            SimulationCadences {
                direct_hz: 200,
                pathing_hz: 100,
                reflections_hz: 50,
                reflection_max_hz: 100,
                ..SimulationCadences::default()
            },
        )
        .unwrap();
        worker.publish_update(update());
        thread::sleep(Duration::from_millis(60));
        worker.stop();

        let telemetry = worker.telemetry();
        assert!(direct.load(Ordering::Relaxed) >= 5);
        assert!(pathing.load(Ordering::Relaxed) >= 3);
        assert!(reflections.load(Ordering::Relaxed) >= 2);
        assert_eq!(telemetry.pathing.failures, pathing.load(Ordering::Relaxed));
        assert!(!telemetry.direct.timings.is_empty());
        assert!(!telemetry.reflections.timings.is_empty());
    }

    #[test]
    fn zero_cadence_is_rejected() {
        let counter = Arc::new(AtomicU64::new(0));
        let runner = CountingRunner {
            direct: Arc::clone(&counter),
            pathing: Arc::clone(&counter),
            reflections: counter,
        };
        assert_eq!(
            SimulationWorker::new(
                Box::new(runner),
                update(),
                SimulationCadences {
                    direct_hz: 0,
                    ..SimulationCadences::default()
                },
            )
            .err(),
            Some(SimulationWorkerError::InvalidCadence)
        );
    }

    #[test]
    fn listener_and_active_source_travel_accumulate_between_reflections() {
        let initial = update();
        let mut displacement = ReflectionDisplacement::new(initial);
        let mut current = initial;
        current.listener.pose.position.north_m = 0.6;
        displacement.observe(current);
        assert!(!displacement.exceeded(1.0));

        current.listener.pose.position.north_m = 0.0;
        displacement.observe(current);
        assert!(
            displacement.exceeded(1.0),
            "a reversal must not cancel distance already traveled"
        );
        displacement.reset();
        assert!(!displacement.exceeded(1.0));

        current.sources[0].active = true;
        displacement.observe(current);
        current.sources[0].pose.position.east_m = 1.0;
        displacement.observe(current);
        assert!(displacement.exceeded(1.0));

        displacement.reset();
        current.sources[0].active = false;
        current.sources[0].pose.position.east_m = 3.0;
        displacement.observe(current);
        assert!(!displacement.exceeded(1.0));
    }

    #[test]
    fn reflection_rate_cap_cannot_be_lower_than_periodic_cadence() {
        let counter = Arc::new(AtomicU64::new(0));
        let runner = CountingRunner {
            direct: Arc::clone(&counter),
            pathing: Arc::clone(&counter),
            reflections: counter,
        };
        assert_eq!(
            SimulationWorker::new(
                Box::new(runner),
                update(),
                SimulationCadences {
                    reflections_hz: 25,
                    reflection_max_hz: 20,
                    ..SimulationCadences::default()
                },
            )
            .err(),
            Some(SimulationWorkerError::InvalidCadence)
        );
    }

    #[test]
    fn displacement_request_waits_for_rate_cap_but_periodic_floor_still_fires() {
        let started = Instant::now();
        let periodic_deadline = started + Duration::from_millis(200);
        let next_eligible = started + Duration::from_millis(40);

        assert!(!reflection_tick_due(
            started + Duration::from_millis(39),
            periodic_deadline,
            next_eligible,
            true,
        ));
        assert!(reflection_tick_due(
            next_eligible,
            periodic_deadline,
            next_eligible,
            true,
        ));
        assert!(reflection_tick_due(
            periodic_deadline,
            periodic_deadline,
            next_eligible,
            false,
        ));
    }
}
