use super::*;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};

#[derive(Clone, Copy)]
pub(super) struct ReflectionJobGroup {
    pub budget: SourceReflectionBudget,
    pub enabled: [bool; MAX_ACTIVE_SOURCES],
}

impl Default for ReflectionJobGroup {
    fn default() -> Self {
        Self { budget: SourceReflectionBudget::OFF, enabled: [false; MAX_ACTIVE_SOURCES] }
    }
}

pub(super) struct ReflectionJob {
    pub groups: [ReflectionJobGroup; MAX_REFLECTION_GROUPS],
    pub group_count: usize,
    pub frame: SimulationFrame,
    pub quality: GovernorRenderSnapshot,
    pub config: S3SimulationConfig,
    pub directivities: [Directivity; MAX_ACTIVE_SOURCES],
    pub occlusion_modes: [DirectOcclusionMode; MAX_ACTIVE_SOURCES],
    pub revisions: [u64; MAX_ACTIVE_SOURCES],
    pub interval_ns: u64,
}

pub(super) struct ReflectionCompletion {
    pub params: [Option<SteamReflectionParams>; MAX_ACTIVE_SOURCES],
    pub revisions: [u64; MAX_ACTIVE_SOURCES],
    pub quality: GovernorRenderSnapshot,
    pub elapsed_ns: u64,
    pub interval_ns: u64,
    pub vendor_runs: u64,
    pub output_queries: u64,
}

/// Only this thread writes reflection flags; the existing worker owns direct
/// and pathing flags and all Rust snapshots. Steam Audio 4.8.1 phonon.h permits
/// per-flag SetSharedInputs (4178-4182), SetInputs (4272-4276), and separate
/// RunDirect/RunReflections/RunPathing lanes (4200-4224). GetOutputs (4281-4287)
/// runs after its owning pass; api_simulator.cpp:401-437 reads disjoint outputs.
/// Scene/probe/Commit changes are forbidden during any run (4144/4156/4168/4192):
/// immutable WorldGeneration construction precedes both workers, and its Arc
/// cannot destroy/remove/commit native objects until this worker has joined.
pub(super) struct ReflectionWorker {
    jobs: Option<SyncSender<ReflectionJob>>,
    completed: Receiver<Result<ReflectionCompletion, SimulationError>>,
    thread: Option<JoinHandle<()>>,
}

impl ReflectionWorker {
    pub fn new(world: Arc<WorldGeneration>, audio: AudioConfig) -> Result<Self, SimulationError> {
        let (jobs, job_reader) = mpsc::sync_channel::<ReflectionJob>(1);
        let (completion_writer, completed) = mpsc::sync_channel(1);
        let profile = std::env::var_os("FIGHTBOX_SIMULATION_PROFILE").is_some();
        let thread = thread::Builder::new().name("fightbox-reflections".into()).spawn(move || {
            let mut samples = profile.then(|| Vec::with_capacity(2048));
            while let Ok(job) = job_reader.recv() {
                let started = Instant::now();
                let result = run_job(&world, audio, &job);
                let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                if let Some(samples) = &mut samples {
                    samples.push((job.quality.reflections.level, elapsed_ns, job.interval_ns));
                }
                let result = result.map(|mut completion| {
                    completion.elapsed_ns = elapsed_ns;
                    completion
                });
                if completion_writer.send(result).is_err() { break; }
            }
            if let Some(samples) = samples {
                for (level, elapsed_ns, interval_ns) in samples {
                    eprintln!("REFLECTION_JOB level={level:?} elapsed_ns={elapsed_ns} interval_ns={interval_ns}");
                }
            }
        }).map_err(|_| SimulationError::KernelFailure)?;
        Ok(Self { jobs: Some(jobs), completed, thread: Some(thread) })
    }

    pub fn submit(&self, job: ReflectionJob) -> Result<(), SimulationError> {
        self.jobs.as_ref().ok_or(SimulationError::KernelFailure)?
            .try_send(job).map_err(|_| SimulationError::KernelFailure)
    }

    pub fn poll(&self) -> Result<Option<ReflectionCompletion>, SimulationError> {
        match self.completed.try_recv() {
            Ok(result) => result.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(SimulationError::KernelFailure),
        }
    }
}

impl Drop for ReflectionWorker {
    fn drop(&mut self) {
        // The owner permits only one unharvested job, so its final completion
        // fits the bounded result slot even after polling has stopped.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_job(world: &WorldGeneration, audio: AudioConfig, job: &ReflectionJob)
    -> Result<ReflectionCompletion, SimulationError>
{
    let mut completion = ReflectionCompletion {
        params: [None; MAX_ACTIVE_SOURCES], revisions: job.revisions,
        quality: job.quality, elapsed_ns: 0, interval_ns: job.interval_ns,
        vendor_runs: 0, output_queries: 0,
    };
    for group in &job.groups[..job.group_count] {
        let mut shared = shared_inputs(job.frame.listener, job.quality)
            .ok_or(SimulationError::InvalidUpdate)?;
        shared.numRays = group.budget.rays;
        shared.numBounces = group.budget.bounces;
        shared.duration = group.budget.duration_s;
        shared.order = group.budget.order;
        ffi::simulator_set_shared_inputs(world.simulator(), ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut shared);
        for index in 0..world.source_count {
            let flags = if group.enabled[index] { ffi::IPL_SIMULATIONFLAGS_REFLECTIONS } else { 0 };
            let mut inputs = source_inputs(job.frame.sources[index], job.directivities[index],
                job.occlusion_modes[index], world.probe_batch(), job.config, job.quality, flags)
                .ok_or(SimulationError::InvalidUpdate)?;
            inputs.flags = flags;
            ffi::source_set_inputs(world.source(index), ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut inputs);
        }
        ffi::simulator_run_reflections(world.simulator());
        completion.vendor_runs += 1;
        for index in 0..world.source_count {
            if !group.enabled[index] { continue; }
            let mut outputs = ffi::IPLSimulationOutputs::zeroed();
            ffi::source_get_outputs(world.source(index), ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut outputs);
            completion.output_queries += 1;
            let expected_channels = ambisonics_channel_count(group.budget.order)
                .map_err(|_| SimulationError::KernelFailure)?;
            let maximum_ir_size = reflection_ir_size(group.budget.duration_s, audio.sample_rate_hz)
                .map_err(|_| SimulationError::KernelFailure)?;
            if reflection_effect_uses_ir(job.config.reflection_effect.effect_type)
                && (outputs.reflections.ir.is_null() || outputs.reflections.numChannels != expected_channels
                    || outputs.reflections.irSize <= 0 || outputs.reflections.irSize > maximum_ir_size)
            {
                return Err(SimulationError::KernelFailure);
            }
            if !outputs.reflections.reverbTimes.into_iter().chain(outputs.reflections.eq).all(f32::is_finite) {
                return Err(SimulationError::KernelFailure);
            }
            completion.params[index] = Some(SteamReflectionParams {
                ir: outputs.reflections.ir as usize, reverb_times: outputs.reflections.reverbTimes,
                eq: outputs.reflections.eq, delay: outputs.reflections.delay,
                num_channels: outputs.reflections.numChannels, ir_size: outputs.reflections.irSize,
                tan_slot: outputs.reflections.tanSlot,
            });
        }
    }
    Ok(completion)
}
