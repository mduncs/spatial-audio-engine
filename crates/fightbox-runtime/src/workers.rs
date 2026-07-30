//! Cadenced backend simulation on a non-audio thread.

use crate::backend::{SimulationError, SimulationRunner, SimulationUpdate};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SimulationWorkerError {
    InvalidCadence,
    ThreadSpawn,
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

        let thread = thread::Builder::new()
            .name("fightbox-simulation".into())
            .spawn(move || {
                let mut runner = runner;
                let mut deadlines = [Instant::now(); 3];
                let mut next_reflection_eligible = deadlines[2];
                let mut reflection_displacement = ReflectionDisplacement::new(initial_update);
                while !thread_stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    let update = update_reader.read();
                    reflection_displacement.observe(update);
                    let periodic_reflection_due = now >= deadlines[2];
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
                        record_pass(&thread_telemetry, 0, || runner.run_direct());
                        advance_deadline(&mut deadlines[0], periods[0]);
                    }
                    if due[1] {
                        record_pass(&thread_telemetry, 1, || runner.run_pathing());
                        advance_deadline(&mut deadlines[1], periods[1]);
                    }
                    if due[2] {
                        let reflection_started = Instant::now();
                        record_pass(&thread_telemetry, 2, || runner.run_reflections());
                        reflection_displacement.reset();
                        next_reflection_eligible = reflection_started + reflection_min_period;
                        if periodic_reflection_due {
                            advance_deadline(&mut deadlines[2], periods[2]);
                        }
                    }

                    let wake_now = Instant::now();
                    let reflection_requested = wake_now >= deadlines[2]
                        || reflection_displacement.exceeded(cadences.reflection_max_displacement_m);
                    let reflection_wake = if reflection_requested {
                        if next_reflection_eligible > wake_now {
                            next_reflection_eligible
                        } else {
                            wake_now
                        }
                    } else {
                        deadlines[2]
                    };
                    let next = deadlines[0].min(deadlines[1]).min(reflection_wake);
                    thread::park_timeout(next.saturating_duration_since(Instant::now()));
                }
            })
            .map_err(|_| SimulationWorkerError::ThreadSpawn)?;

        Ok(Self {
            updates,
            stop,
            telemetry,
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

fn record_pass(
    telemetry: &Mutex<SimulationWorkerTelemetry>,
    pass: usize,
    operation: impl FnOnce() -> Result<(), SimulationError>,
) {
    let started = Instant::now();
    let result = operation();
    let duration_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::SourceMotion;
    use fightbox_api::{EnuVector3, ListenerState, Pose};
    use std::sync::atomic::AtomicU64;

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
