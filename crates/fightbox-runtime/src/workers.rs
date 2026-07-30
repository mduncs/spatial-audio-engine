//! Cadenced backend simulation on a non-audio thread.

use crate::backend::{SimulationError, SimulationRunner, SimulationUpdate};
use crate::{SnapshotPublication, SnapshotWriter, TimingHistory};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SimulationCadences {
    pub direct_hz: u32,
    pub pathing_hz: u32,
    pub reflections_hz: u32,
}

impl Default for SimulationCadences {
    fn default() -> Self {
        Self {
            direct_hz: 60,
            pathing_hz: 15,
            reflections_hz: 5,
        }
    }
}

impl SimulationCadences {
    fn periods(self) -> Result<[Duration; 3], SimulationWorkerError> {
        if self.direct_hz == 0 || self.pathing_hz == 0 || self.reflections_hz == 0 {
            return Err(SimulationWorkerError::InvalidCadence);
        }
        Ok([
            Duration::from_secs_f64(1.0 / f64::from(self.direct_hz)),
            Duration::from_secs_f64(1.0 / f64::from(self.pathing_hz)),
            Duration::from_secs_f64(1.0 / f64::from(self.reflections_hz)),
        ])
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
/// A single thread deliberately multiplexes all three initial cadences:
/// reflection p99 is approximately 1.3 ms against the direct pass's 16.7 ms
/// period, leaving safe cadence headroom without extra synchronization. The
/// runner trait already splits the passes, so a measured future need can move
/// them to separate threads without changing the backend seam.
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
        let periods = cadences.periods()?;
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
                while !thread_stop.load(Ordering::Acquire) {
                    let now = Instant::now();
                    let due = [
                        now >= deadlines[0],
                        now >= deadlines[1],
                        now >= deadlines[2],
                    ];
                    if due.iter().any(|is_due| *is_due) {
                        let update = update_reader.read();
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
                        record_pass(&thread_telemetry, 2, || runner.run_reflections());
                        advance_deadline(&mut deadlines[2], periods[2]);
                    }

                    let next = deadlines.into_iter().min().unwrap_or_else(Instant::now);
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
}
