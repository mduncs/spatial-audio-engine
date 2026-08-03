//! Safe RAII and owned Phase A operations over the private 4.8.1 FFI module.

use core::{marker::PhantomData, mem::size_of, ptr::NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::elevated_probes;
use crate::ffi;
use crate::{
    AudioConfig, BackendError, BakedProbeBatch, DirectOcclusionMode, DirectSnapshot,
    ElevatedProbeLayer, EnuVector3, ListenerPose, OwnedStereoPcm, PROBE_BATCH_METADATA_SCHEMA,
    PathSnapshot, PathValidationSegment, ProbeBatchMetadata, ProbeVolume, ReflectionEffectType,
    ReflectionSnapshot, S0RenderOutput, S0RenderRequest, S3_BENCHMARK_MAX_DIFFUSE_SAMPLES,
    S3_BENCHMARK_MAX_OCCLUSION_SAMPLES, S3_BENCHMARK_MAX_RAY_BATCH_SIZE,
    S3_BENCHMARK_MAX_REFLECTION_BOUNCES, S3_BENCHMARK_MAX_REFLECTION_IR_SAMPLES,
    S3_BENCHMARK_MAX_REFLECTION_ITERATIONS, S3_BENCHMARK_MAX_REFLECTION_RAYS,
    S3_BENCHMARK_MAX_SIMULATION_THREADS, S3_BENCHMARK_MAX_STANDARD_ITERATIONS,
    S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD, S3_CONTINUITY_WINDOW_FRAMES, S3BakeRequest,
    S3BenchmarkFiniteChecks, S3BenchmarkOutput, S3BenchmarkRequest, S3RenderOutput,
    S3RenderRequest, S3RetainedSessionStats, S3SimulationConfig, S3SimulationSnapshot,
    S3StageTimingSamples, S3Stems, S3TrajectoryBlock, S3TrajectoryRenderOutput,
    S3TrajectoryRenderRequest, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh,
    SteamVector3, decode_path_direction_enu, enu_to_steam, measure_s3_summed_boundary_continuity,
    sha256_hex, steam_to_enu, validate_direct_snapshot,
};

#[path = "multi_source.rs"]
mod multi_source;
use multi_source::{
    MultiSourceRenderGraph as GenerationRenderGraph, MultiSourceSimulation as GenerationSimulation,
    build_anomaly_query_simulation as build_generation_anomaly_query_simulation,
    build_multi_source_generation,
};

use crate::world_swap;
use crate::{
    DeliveredWorldState, MultiSourceDescriptor, PreparedWorldCapabilities, PreparedWorldSwapError,
    QualityTier, StageOutputGains, WorldReflectionState,
};
use fightbox_runtime::backend::{
    BackendRenderError, PropagationRenderBlock, SimulationError, SimulationUpdate,
};

const WORLD_SWAP_FADE_BLOCKS: u8 = 8;
const GENERATION_MASK: u64 = (1_u64 << 48) - 1;
const BAKED_PATHING_BIT: u64 = 1_u64 << 48;
const REFLECTION_SHIFT: u32 = 49;
const TRANSITION_SHIFT: u32 = 56;
static NEXT_WORLD_GENERATION: AtomicU64 = AtomicU64::new(1);

fn next_world_generation() -> u64 {
    NEXT_WORLD_GENERATION.fetch_add(1, Ordering::Relaxed) & GENERATION_MASK
}

fn reflection_code(reflections: WorldReflectionState) -> u64 {
    match reflections {
        WorldReflectionState::RealtimeConvolution => 0,
        WorldReflectionState::RealtimeParametric => 1,
        WorldReflectionState::RealtimeHybrid => 2,
        WorldReflectionState::UnsupportedTrueAudioNext => 3,
    }
}

fn encode_delivery(capabilities: PreparedWorldCapabilities, transition_blocks: u8) -> u64 {
    (capabilities.generation & GENERATION_MASK)
        | if capabilities.baked_pathing {
            BAKED_PATHING_BIT
        } else {
            0
        }
        | (reflection_code(capabilities.reflections) << REFLECTION_SHIFT)
        | (u64::from(transition_blocks) << TRANSITION_SHIFT)
}

fn decode_delivery(encoded: u64) -> DeliveredWorldState {
    let reflections = match (encoded >> REFLECTION_SHIFT) & 0b11 {
        1 => WorldReflectionState::RealtimeParametric,
        2 => WorldReflectionState::RealtimeHybrid,
        3 => WorldReflectionState::UnsupportedTrueAudioNext,
        _ => WorldReflectionState::RealtimeConvolution,
    };
    DeliveredWorldState {
        capabilities: PreparedWorldCapabilities {
            generation: encoded & GENERATION_MASK,
            baked_pathing: encoded & BAKED_PATHING_BIT != 0,
            reflections,
        },
        transition_blocks_remaining: (encoded >> TRANSITION_SHIFT) as u8,
    }
}

/// Thin linked wrapper for the simulation-only anomaly field query path.
pub(crate) struct AnomalyQuerySimulation {
    inner: GenerationSimulation,
}

impl AnomalyQuerySimulation {
    pub(crate) fn sample(
        &mut self,
        listener: EnuVector3,
    ) -> Result<crate::SourceAcousticDiagnostics, BackendError> {
        let listener = fightbox_api::EnuVector3::new(listener.x, listener.y, listener.z);
        // Preserve the immutable source pose already seeded into the generation:
        // a query update must move only the listener. `update_listener` exists to
        // avoid manufacturing or exposing the private source frame here.
        self.inner.update_listener(listener);
        self.inner.run_direct().map_err(simulation_query_error)?;
        self.inner.run_pathing().map_err(simulation_query_error)?;
        self.inner
            .source_diagnostics(0)
            .ok_or(BackendError::InvalidInput(
                "anomaly query session lost its only source",
            ))
    }
}

fn simulation_query_error(_error: SimulationError) -> BackendError {
    BackendError::SdkCall {
        function: "anomaly direct/path query",
        status: -1,
    }
}

pub(crate) fn build_anomaly_query_simulation(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptor: MultiSourceDescriptor,
) -> Result<AnomalyQuerySimulation, BackendError> {
    Ok(AnomalyQuerySimulation {
        inner: build_generation_anomaly_query_simulation(mesh, baked, audio, config, descriptor)?,
    })
}

pub(crate) struct PreparedMultiSourceWorld {
    simulation: GenerationSimulation,
    render: GenerationRenderGraph,
}

impl PreparedMultiSourceWorld {
    pub(crate) fn capabilities(&self) -> PreparedWorldCapabilities {
        self.simulation.capabilities()
    }

    pub(crate) fn take_stage_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<StageOutputGains>> {
        self.render.take_stage_output_gain_writer()
    }

    pub(crate) fn take_echo_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<f32>> {
        self.render.take_echo_output_gain_writer()
    }

    pub(crate) fn diagnostics(&self) -> crate::WorldGenerationDiagnostics {
        self.simulation.diagnostics()
    }

    pub(crate) fn observe_render_timing(&mut self, elapsed_ns: u64) {
        self.simulation.observe_render_timing(elapsed_ns);
    }

    pub(crate) fn quality_governor_telemetry(&self) -> Option<crate::QualityGovernorTelemetry> {
        self.simulation
            .capabilities()
            .baked_pathing
            .then(|| self.simulation.quality_governor_telemetry())
    }

    pub(crate) fn update_inputs(&mut self, update: &SimulationUpdate) {
        self.simulation.update_inputs(update);
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.simulation.run_direct()
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.simulation.run_pathing()
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        self.simulation.run_reflections()
    }
}

pub(crate) struct MultiSourceSimulation {
    active: GenerationSimulation,
    prepared: world_swap::Producer<GenerationRenderGraph>,
    retired: world_swap::Consumer<GenerationRenderGraph>,
    delivered: Arc<AtomicU64>,
    session_fixed_memory_bytes: u64,
    tracked_memory_at_create_bytes: u64,
    tracked_memory_peak_bytes: u64,
}

impl MultiSourceSimulation {
    fn collect_retired(&mut self) {
        while self.retired.try_pop().is_some() {}
    }

    pub(crate) fn observe_render_timing(&mut self, elapsed_ns: u64) {
        self.collect_retired();
        self.active.observe_render_timing(elapsed_ns);
    }

    pub(crate) fn observe_simulation_lateness(
        &mut self,
        pass: crate::GovernorSimulationPass,
        lateness_ns: u64,
    ) {
        self.collect_retired();
        self.active.observe_simulation_lateness(pass, lateness_ns);
    }

    pub(crate) fn quality_governor_telemetry(&self) -> Option<crate::QualityGovernorTelemetry> {
        self.active.capabilities().baked_pathing.then(|| {
            let mut telemetry = self.active.quality_governor_telemetry();
            telemetry.memory.render_scratch_bytes = telemetry
                .memory
                .render_scratch_bytes
                .saturating_add(self.session_fixed_memory_bytes);
            telemetry.memory.tracked_at_create_bytes = self.tracked_memory_at_create_bytes;
            telemetry.memory.tracked_current_bytes = telemetry
                .memory
                .tracked_current_bytes
                .saturating_add(self.session_fixed_memory_bytes);
            telemetry.memory.tracked_peak_bytes = self
                .tracked_memory_peak_bytes
                .max(telemetry.memory.tracked_current_bytes);
            telemetry
        })
    }

    pub(crate) fn update_inputs(&mut self, update: &SimulationUpdate) {
        self.collect_retired();
        self.active.update_inputs(update);
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        self.active.run_direct()
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        self.active.run_pathing()
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        self.active.run_reflections()
    }

    pub(crate) fn prepare_world(
        &mut self,
        mesh: &SceneMesh,
        baked: Option<&BakedProbeBatch>,
        config: S3SimulationConfig,
        descriptors: &[MultiSourceDescriptor],
    ) -> Result<PreparedMultiSourceWorld, BackendError> {
        self.collect_retired();
        if descriptors.len() != self.active.source_count() {
            return Err(BackendError::InvalidInput(
                "prepared world source count must match the active render graph",
            ));
        }
        let generation = next_world_generation();
        let (simulation, render) = build_multi_source_generation(
            mesh,
            baked,
            self.active.audio_config(),
            config,
            descriptors,
            generation,
            self.active.quality_governor_telemetry().quality_tier,
        )?;
        let active_bytes = self
            .active
            .quality_governor_telemetry()
            .memory
            .tracked_current_bytes;
        let prepared_bytes = simulation
            .quality_governor_telemetry()
            .memory
            .tracked_current_bytes;
        self.tracked_memory_peak_bytes = self.tracked_memory_peak_bytes.max(
            active_bytes
                .saturating_add(prepared_bytes)
                .saturating_add(self.session_fixed_memory_bytes),
        );
        Ok(PreparedMultiSourceWorld { simulation, render })
    }

    pub(crate) fn swap_prepared_world(
        &mut self,
        prepared: PreparedMultiSourceWorld,
    ) -> Result<(), PreparedWorldSwapError> {
        self.collect_retired();
        let PreparedMultiSourceWorld { simulation, render } = prepared;
        self.prepared
            .try_push(render)
            .map_err(|_| PreparedWorldSwapError::AdoptionPending)?;
        self.active = simulation;
        Ok(())
    }

    pub(crate) fn delivered_world_state(&self) -> DeliveredWorldState {
        decode_delivery(self.delivered.load(Ordering::Acquire))
    }

    pub(crate) fn diagnostics(&self) -> crate::WorldGenerationDiagnostics {
        self.active.diagnostics()
    }

    pub(crate) fn source_diagnostics(
        &self,
        source_index: usize,
    ) -> Option<crate::SourceAcousticDiagnostics> {
        self.active.source_diagnostics(source_index)
    }
}

struct RetiringGeneration {
    graph: GenerationRenderGraph,
    completed_blocks: u8,
}

pub(crate) struct MultiSourceRenderGraph {
    active: GenerationRenderGraph,
    prepared: world_swap::Consumer<GenerationRenderGraph>,
    retired: world_swap::Producer<GenerationRenderGraph>,
    retiring: Option<RetiringGeneration>,
    retirement_backlog: Option<GenerationRenderGraph>,
    old_left: Vec<f32>,
    old_right: Vec<f32>,
    new_left: Vec<f32>,
    new_right: Vec<f32>,
    delivered: Arc<AtomicU64>,
}

impl MultiSourceRenderGraph {
    pub(crate) fn take_stage_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<StageOutputGains>> {
        self.active.take_stage_output_gain_writer()
    }

    pub(crate) fn take_echo_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<f32>> {
        self.active.take_echo_output_gain_writer()
    }

    pub(crate) fn take_live_stage_energy_reader(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotReader<crate::LiveStageEnergySnapshot>> {
        self.active.take_live_stage_energy_reader()
    }

    fn flush_retirement(&mut self) {
        let Some(retired) = self.retirement_backlog.take() else {
            return;
        };
        if let Err(retired) = self.retired.try_push(retired) {
            self.retirement_backlog = Some(retired);
        }
    }

    fn adopt_at_block_boundary(&mut self) {
        self.flush_retirement();
        if self.retiring.is_some() || self.retirement_backlog.is_some() {
            return;
        }
        let Some(prepared) = self.prepared.try_pop() else {
            return;
        };
        let old = std::mem::replace(&mut self.active, prepared);
        let capabilities = self.active.capabilities();
        self.retiring = Some(RetiringGeneration {
            graph: old,
            completed_blocks: 0,
        });
        self.delivered.store(
            encode_delivery(capabilities, WORLD_SWAP_FADE_BLOCKS),
            Ordering::Release,
        );
    }

    pub(crate) fn render_block(
        &mut self,
        block: PropagationRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        self.adopt_at_block_boundary();
        let Some(retiring) = self.retiring.as_mut() else {
            return self.active.render_block(block);
        };

        self.old_left.fill(0.0);
        self.old_right.fill(0.0);
        self.new_left.fill(0.0);
        self.new_right.fill(0.0);
        retiring.graph.render_block(PropagationRenderBlock {
            listener_orientation: block.listener_orientation,
            sources: block.sources,
            output_left: &mut self.old_left,
            output_right: &mut self.old_right,
        })?;
        self.active.render_block(PropagationRenderBlock {
            listener_orientation: block.listener_orientation,
            sources: block.sources,
            output_left: &mut self.new_left,
            output_right: &mut self.new_right,
        })?;

        let frames = self.old_left.len();
        let fade_frames = frames * usize::from(WORLD_SWAP_FADE_BLOCKS);
        let start_frame = frames * usize::from(retiring.completed_blocks);
        for frame in 0..frames {
            let new_gain = (start_frame + frame) as f32 / (fade_frames - 1) as f32;
            let old_gain = 1.0 - new_gain;
            block.output_left[frame] +=
                self.old_left[frame] * old_gain + self.new_left[frame] * new_gain;
            block.output_right[frame] +=
                self.old_right[frame] * old_gain + self.new_right[frame] * new_gain;
        }

        retiring.completed_blocks += 1;
        let remaining = WORLD_SWAP_FADE_BLOCKS - retiring.completed_blocks;
        self.delivered.store(
            encode_delivery(self.active.capabilities(), remaining),
            Ordering::Release,
        );
        if remaining == 0 {
            let retired = self.retiring.take().expect("transition exists").graph;
            if let Err(retired) = self.retired.try_push(retired) {
                self.retirement_backlog = Some(retired);
            }
        }
        Ok(())
    }
}

pub(crate) fn build_multi_source_session(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[MultiSourceDescriptor],
    quality_tier: QualityTier,
) -> Result<(MultiSourceSimulation, MultiSourceRenderGraph), BackendError> {
    let generation = next_world_generation();
    let (simulation, render) = build_multi_source_generation(
        mesh,
        Some(baked),
        audio,
        config,
        descriptors,
        generation,
        quality_tier,
    )?;
    let capabilities = simulation.capabilities();
    let generation_memory_bytes = simulation
        .quality_governor_telemetry()
        .memory
        .tracked_current_bytes;
    let delivered = Arc::new(AtomicU64::new(encode_delivery(capabilities, 0)));
    let (prepared_tx, prepared_rx) = world_swap::channel();
    let (retired_tx, retired_rx) = world_swap::channel();
    let frames = audio.frame_size as usize;
    // The swap wrapper retains old/new stereo render targets so adopting a
    // prepared generation never allocates on the callback.
    let session_fixed_memory_bytes = (frames as u64)
        .saturating_mul(4)
        .saturating_mul(size_of::<f32>() as u64);
    let tracked_memory_at_create_bytes =
        generation_memory_bytes.saturating_add(session_fixed_memory_bytes);
    Ok((
        MultiSourceSimulation {
            active: simulation,
            prepared: prepared_tx,
            retired: retired_rx,
            delivered: Arc::clone(&delivered),
            session_fixed_memory_bytes,
            tracked_memory_at_create_bytes,
            tracked_memory_peak_bytes: tracked_memory_at_create_bytes,
        },
        MultiSourceRenderGraph {
            active: render,
            prepared: prepared_rx,
            retired: retired_tx,
            retiring: None,
            retirement_backlog: None,
            old_left: vec![0.0; frames],
            old_right: vec![0.0; frames],
            new_left: vec![0.0; frames],
            new_right: vec![0.0; frames],
            delivered,
        },
    ))
}

pub struct Context {
    raw: NonNull<ffi::IPLContextOpaque>,
}

impl Context {
    pub fn create() -> Result<Self, i32> {
        let mut raw = core::ptr::null_mut();
        let mut settings = ffi::IPLContextSettings::pinned_defaults();
        let status = ffi::context_create(&mut settings, &mut raw);
        if status != ffi::IPL_STATUS_SUCCESS {
            return Err(status);
        }
        let raw = NonNull::new(raw).ok_or(status)?;
        Ok(Self { raw })
    }

    pub fn is_valid(&self) -> bool {
        true
    }

    fn raw(&self) -> ffi::IPLContext {
        self.raw.as_ptr()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::context_release(&mut raw);
    }
}

struct AudioBuffer<'context> {
    raw: ffi::IPLAudioBuffer,
    context: &'context Context,
}

impl<'context> AudioBuffer<'context> {
    fn allocate(
        context: &'context Context,
        channels: i32,
        samples: i32,
    ) -> Result<Self, BackendError> {
        let mut raw = ffi::IPLAudioBuffer {
            numChannels: 0,
            numSamples: 0,
            data: core::ptr::null_mut(),
        };
        sdk_status(
            "iplAudioBufferAllocate",
            ffi::audio_buffer_allocate(context.raw(), channels, samples, &mut raw),
        )?;
        Ok(Self { raw, context })
    }

    fn write_interleaved(&mut self, samples: &mut [f32]) {
        ffi::audio_buffer_deinterleave(self.context.raw(), samples, &mut self.raw);
    }

    fn read_interleaved(&mut self, samples: &mut [f32]) {
        ffi::audio_buffer_interleave(self.context.raw(), &mut self.raw, samples);
    }

    fn raw_mut(&mut self) -> &mut ffi::IPLAudioBuffer {
        &mut self.raw
    }
}

impl Drop for AudioBuffer<'_> {
    fn drop(&mut self) {
        ffi::audio_buffer_free(self.context.raw(), &mut self.raw);
    }
}

struct Hrtf<'context> {
    raw: NonNull<ffi::IPLHRTFOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> Hrtf<'context> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLHRTFSettings {
            type_: ffi::IPL_HRTFTYPE_DEFAULT,
            sofaFileName: core::ptr::null(),
            sofaData: core::ptr::null(),
            sofaDataSize: 0,
            volume: 1.0,
            normType: ffi::IPL_HRTFNORMTYPE_NONE,
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::hrtf_create(context.raw(), audio_settings, &mut settings, &mut raw);
        sdk_status("iplHRTFCreate", status)?;
        Ok(Self {
            raw: non_null("iplHRTFCreate", status, raw)?,
            _context: PhantomData,
        })
    }

    fn raw(&self) -> ffi::IPLHRTF {
        self.raw.as_ptr()
    }
}

impl Drop for Hrtf<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::hrtf_release(&mut raw);
    }
}

struct DirectEffect<'context> {
    raw: NonNull<ffi::IPLDirectEffectOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> DirectEffect<'context> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLDirectEffectSettings { numChannels: 1 };
        let mut raw = core::ptr::null_mut();
        let status =
            ffi::direct_effect_create(context.raw(), audio_settings, &mut settings, &mut raw);
        sdk_status("iplDirectEffectCreate", status)?;
        Ok(Self {
            raw: non_null("iplDirectEffectCreate", status, raw)?,
            _context: PhantomData,
        })
    }

    fn apply(
        &mut self,
        params: &mut ffi::IPLDirectEffectParams,
        input: &mut AudioBuffer<'_>,
        output: &mut AudioBuffer<'_>,
    ) {
        ffi::direct_effect_apply(self.raw.as_ptr(), params, input.raw_mut(), output.raw_mut());
    }
}

impl Drop for DirectEffect<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::direct_effect_release(&mut raw);
    }
}

struct BinauralEffect<'context, 'hrtf> {
    raw: NonNull<ffi::IPLBinauralEffectOpaque>,
    _context: PhantomData<&'context Context>,
    _hrtf: PhantomData<&'hrtf Hrtf<'context>>,
}

impl<'context, 'hrtf> BinauralEffect<'context, 'hrtf> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
        hrtf: &'hrtf Hrtf<'context>,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLBinauralEffectSettings { hrtf: hrtf.raw() };
        let mut raw = core::ptr::null_mut();
        let status =
            ffi::binaural_effect_create(context.raw(), audio_settings, &mut settings, &mut raw);
        sdk_status("iplBinauralEffectCreate", status)?;
        Ok(Self {
            raw: non_null("iplBinauralEffectCreate", status, raw)?,
            _context: PhantomData,
            _hrtf: PhantomData,
        })
    }

    fn apply(
        &mut self,
        params: &mut ffi::IPLBinauralEffectParams,
        input: &mut AudioBuffer<'_>,
        output: &mut AudioBuffer<'_>,
    ) {
        ffi::binaural_effect_apply(self.raw.as_ptr(), params, input.raw_mut(), output.raw_mut());
    }
}

impl Drop for BinauralEffect<'_, '_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::binaural_effect_release(&mut raw);
    }
}

struct PathEffect<'context, 'hrtf> {
    raw: NonNull<ffi::IPLPathEffectOpaque>,
    _context: PhantomData<&'context Context>,
    _hrtf: PhantomData<&'hrtf Hrtf<'context>>,
}

impl<'context, 'hrtf> PathEffect<'context, 'hrtf> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
        hrtf: &'hrtf Hrtf<'context>,
        max_order: i32,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLPathEffectSettings {
            maxOrder: max_order,
            spatialize: ffi::IPL_TRUE,
            speakerLayout: ffi::IPLSpeakerLayout {
                type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
                numSpeakers: 0,
                speakers: core::ptr::null_mut(),
            },
            hrtf: hrtf.raw(),
        };
        let mut raw = core::ptr::null_mut();
        let status =
            ffi::path_effect_create(context.raw(), audio_settings, &mut settings, &mut raw);
        sdk_status("iplPathEffectCreate", status)?;
        Ok(Self {
            raw: non_null("iplPathEffectCreate", status, raw)?,
            _context: PhantomData,
            _hrtf: PhantomData,
        })
    }

    fn apply(
        &mut self,
        params: &mut ffi::IPLPathEffectParams,
        input: &mut AudioBuffer<'_>,
        output: &mut AudioBuffer<'_>,
    ) {
        ffi::path_effect_apply(self.raw.as_ptr(), params, input.raw_mut(), output.raw_mut());
    }
}

impl Drop for PathEffect<'_, '_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::path_effect_release(&mut raw);
    }
}

struct ReflectionEffect<'context> {
    raw: NonNull<ffi::IPLReflectionEffectOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> ReflectionEffect<'context> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
        effect_type: ReflectionEffectType,
        ir_size: i32,
        num_channels: i32,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLReflectionEffectSettings {
            type_: reflection_effect_ffi_type(effect_type)?,
            irSize: ir_size,
            numChannels: num_channels,
        };
        let mut raw = core::ptr::null_mut();
        let status =
            ffi::reflection_effect_create(context.raw(), audio_settings, &mut settings, &mut raw);
        sdk_status("iplReflectionEffectCreate", status)?;
        Ok(Self {
            raw: non_null("iplReflectionEffectCreate", status, raw)?,
            _context: PhantomData,
        })
    }

    fn apply(
        &mut self,
        params: &mut ffi::IPLReflectionEffectParams,
        input: &mut AudioBuffer<'_>,
        output: &mut AudioBuffer<'_>,
    ) {
        ffi::reflection_effect_apply(self.raw.as_ptr(), params, input.raw_mut(), output.raw_mut());
    }
}

impl Drop for ReflectionEffect<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::reflection_effect_release(&mut raw);
    }
}

struct AmbisonicsBinauralEffect<'context, 'hrtf> {
    raw: NonNull<ffi::IPLAmbisonicsBinauralEffectOpaque>,
    _context: PhantomData<&'context Context>,
    _hrtf: PhantomData<&'hrtf Hrtf<'context>>,
}

impl<'context, 'hrtf> AmbisonicsBinauralEffect<'context, 'hrtf> {
    fn create(
        context: &'context Context,
        audio_settings: &mut ffi::IPLAudioSettings,
        hrtf: &'hrtf Hrtf<'context>,
        max_order: i32,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLAmbisonicsBinauralEffectSettings {
            hrtf: hrtf.raw(),
            maxOrder: max_order,
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::ambisonics_binaural_effect_create(
            context.raw(),
            audio_settings,
            &mut settings,
            &mut raw,
        );
        sdk_status("iplAmbisonicsBinauralEffectCreate", status)?;
        Ok(Self {
            raw: non_null("iplAmbisonicsBinauralEffectCreate", status, raw)?,
            _context: PhantomData,
            _hrtf: PhantomData,
        })
    }

    fn apply(
        &mut self,
        params: &mut ffi::IPLAmbisonicsBinauralEffectParams,
        input: &mut AudioBuffer<'_>,
        output: &mut AudioBuffer<'_>,
    ) {
        ffi::ambisonics_binaural_effect_apply(
            self.raw.as_ptr(),
            params,
            input.raw_mut(),
            output.raw_mut(),
        );
    }
}

impl Drop for AmbisonicsBinauralEffect<'_, '_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::ambisonics_binaural_effect_release(&mut raw);
    }
}

struct Scene<'context> {
    raw: NonNull<ffi::IPLSceneOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> Scene<'context> {
    fn create_default(context: &'context Context) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLSceneSettings {
            type_: ffi::IPL_SCENETYPE_DEFAULT,
            closestHitCallback: None,
            anyHitCallback: None,
            batchedClosestHitCallback: None,
            batchedAnyHitCallback: None,
            userData: core::ptr::null_mut(),
            embreeDevice: core::ptr::null_mut(),
            radeonRaysDevice: core::ptr::null_mut(),
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::scene_create(context.raw(), &mut settings, &mut raw);
        sdk_status("iplSceneCreate", status)?;
        Ok(Self {
            raw: non_null("iplSceneCreate", status, raw)?,
            _context: PhantomData,
        })
    }

    fn raw(&self) -> ffi::IPLScene {
        self.raw.as_ptr()
    }

    fn commit(&self) {
        ffi::scene_commit(self.raw())
    }
}

impl Drop for Scene<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::scene_release(&mut raw);
    }
}

struct StaticMesh<'scene, 'context> {
    raw: NonNull<ffi::IPLStaticMeshOpaque>,
    scene: &'scene Scene<'context>,
}

impl<'scene, 'context> StaticMesh<'scene, 'context> {
    fn create_and_add(
        scene: &'scene Scene<'context>,
        mesh: &SceneMesh,
    ) -> Result<Self, BackendError> {
        validate_mesh(mesh)?;
        let mut vertices = mesh
            .vertices_enu_m
            .iter()
            .copied()
            .map(raw_vector)
            .collect::<Vec<_>>();
        let mut triangles = mesh
            .triangles
            .iter()
            .copied()
            .map(|indices| ffi::IPLTriangle { indices })
            .collect::<Vec<_>>();
        let mut material_indices = mesh.material_indices.clone();
        let mut materials = mesh
            .materials
            .iter()
            .map(|material| ffi::IPLMaterial {
                absorption: material.absorption,
                scattering: material.scattering,
                transmission: material.transmission,
            })
            .collect::<Vec<_>>();
        let mut settings = ffi::IPLStaticMeshSettings {
            numVertices: checked_i32(vertices.len(), "mesh has too many vertices")?,
            numTriangles: checked_i32(triangles.len(), "mesh has too many triangles")?,
            numMaterials: checked_i32(materials.len(), "mesh has too many materials")?,
            vertices: vertices.as_mut_ptr(),
            triangles: triangles.as_mut_ptr(),
            materialIndices: material_indices.as_mut_ptr(),
            materials: materials.as_mut_ptr(),
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::static_mesh_create(scene.raw(), &mut settings, &mut raw);
        sdk_status("iplStaticMeshCreate", status)?;
        let static_mesh = Self {
            raw: non_null("iplStaticMeshCreate", status, raw)?,
            scene,
        };
        // Creation does not add a mesh to a scene in 4.8.1.
        ffi::static_mesh_add(static_mesh.raw(), scene.raw());
        scene.commit();
        Ok(static_mesh)
    }

    fn raw(&self) -> ffi::IPLStaticMesh {
        self.raw.as_ptr()
    }
}

impl Drop for StaticMesh<'_, '_> {
    fn drop(&mut self) {
        ffi::static_mesh_remove(self.raw(), self.scene.raw());
        self.scene.commit();
        let mut raw = self.raw.as_ptr();
        ffi::static_mesh_release(&mut raw);
    }
}

struct ProbeArray<'context> {
    raw: NonNull<ffi::IPLProbeArrayOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> ProbeArray<'context> {
    fn generate_uniform_floor(
        context: &'context Context,
        scene: &Scene<'context>,
        volume: ProbeVolume,
    ) -> Result<(Self, u32), BackendError> {
        validate_probe_volume(volume)?;
        let mut raw = core::ptr::null_mut();
        let status = ffi::probe_array_create(context.raw(), &mut raw);
        sdk_status("iplProbeArrayCreate", status)?;
        let array = Self {
            raw: non_null("iplProbeArrayCreate", status, raw)?,
            _context: PhantomData,
        };
        let mut params = ffi::IPLProbeGenerationParams {
            type_: ffi::IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR,
            spacing: volume.spacing_m,
            height: volume.height_above_floor_m,
            transform: probe_transform(volume),
        };
        ffi::probe_array_generate_probes(array.raw(), scene.raw(), &mut params);
        let count = ffi::probe_array_get_num_probes(array.raw());
        if count <= 0 {
            return Err(BackendError::ProbeGenerationProducedNoProbes);
        }
        Ok((array, count as u32))
    }

    fn raw(&self) -> ffi::IPLProbeArray {
        self.raw.as_ptr()
    }
}

impl Drop for ProbeArray<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::probe_array_release(&mut raw);
    }
}

struct ProbeBatch<'context> {
    raw: NonNull<ffi::IPLProbeBatchOpaque>,
    _context: PhantomData<&'context Context>,
}

impl<'context> ProbeBatch<'context> {
    fn from_array(
        context: &'context Context,
        array: &ProbeArray<'context>,
    ) -> Result<Self, BackendError> {
        let mut raw = core::ptr::null_mut();
        let status = ffi::probe_batch_create(context.raw(), &mut raw);
        sdk_status("iplProbeBatchCreate", status)?;
        let batch = Self {
            raw: non_null("iplProbeBatchCreate", status, raw)?,
            _context: PhantomData,
        };
        ffi::probe_batch_add_probe_array(batch.raw(), array.raw());
        // The bake silently sees no committed probes if this call is omitted.
        ffi::probe_batch_commit(batch.raw());
        Ok(batch)
    }

    fn load(
        context: &'context Context,
        serialized: &SerializedObject<'context, '_>,
    ) -> Result<Self, BackendError> {
        let mut raw = core::ptr::null_mut();
        let status = ffi::probe_batch_load(context.raw(), serialized.raw(), &mut raw);
        sdk_status("iplProbeBatchLoad", status)?;
        let batch = Self {
            raw: non_null("iplProbeBatchLoad", status, raw)?,
            _context: PhantomData,
        };
        // 4.8.1's deserializing ProbeBatch constructor restores probes and data
        // layers but does not rebuild its ProbeTree. Reflections/pathing query the
        // tree even for a loaded batch, so commit it before adding to a simulator.
        ffi::probe_batch_commit(batch.raw());
        Ok(batch)
    }

    /// Merge manually placed mid-air probes into this batch and return the new
    /// total probe count.
    ///
    /// Steam Audio 4.8.1 bakes pathing against a single batch and the runtime
    /// loads a single batch, so the elevated probes have to live here rather
    /// than in a batch of their own. `iplProbeBatchAddProbe` is the only route
    /// to a probe the uniform-floor generator would never place.
    fn add_elevated_layers(
        &self,
        layers: &[ElevatedProbeLayer],
        volume: ProbeVolume,
        mesh: &SceneMesh,
    ) -> Result<u32, BackendError> {
        for layer in layers {
            for center in elevated_probes::layer_probe_centers(volume, *layer, mesh) {
                let center = enu_to_steam(center);
                ffi::probe_batch_add_probe(
                    self.raw(),
                    ffi::IPLSphere {
                        center: ffi::IPLVector3 {
                            x: center.x,
                            y: center.y,
                            z: center.z,
                        },
                        radius: layer.spacing_m,
                    },
                );
            }
        }
        // Adding probes invalidates the ProbeTree built by the earlier commit.
        ffi::probe_batch_commit(self.raw());
        let count = self.probe_count();
        if count <= 0 {
            return Err(BackendError::ProbeGenerationProducedNoProbes);
        }
        Ok(count as u32)
    }

    fn raw(&self) -> ffi::IPLProbeBatch {
        self.raw.as_ptr()
    }

    fn probe_count(&self) -> i32 {
        ffi::probe_batch_get_num_probes(self.raw())
    }

    fn path_data_size(&self) -> usize {
        let mut identifier = pathing_identifier();
        ffi::probe_batch_get_data_size(self.raw(), &mut identifier)
    }
}

impl Drop for ProbeBatch<'_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::probe_batch_release(&mut raw);
    }
}

struct SerializedObject<'context, 'data> {
    raw: NonNull<ffi::IPLSerializedObjectOpaque>,
    _context: PhantomData<&'context Context>,
    _data: PhantomData<&'data mut [u8]>,
}

impl<'context> SerializedObject<'context, 'static> {
    fn empty(context: &'context Context) -> Result<Self, BackendError> {
        Self::create(context, core::ptr::null_mut(), 0)
    }
}

impl<'context, 'data> SerializedObject<'context, 'data> {
    fn from_bytes(
        context: &'context Context,
        bytes: &'data mut [u8],
    ) -> Result<Self, BackendError> {
        Self::create(context, bytes.as_mut_ptr(), bytes.len())
    }

    fn create(
        context: &'context Context,
        data: *mut u8,
        size: usize,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLSerializedObjectSettings { data, size };
        let mut raw = core::ptr::null_mut();
        let status = ffi::serialized_object_create(context.raw(), &mut settings, &mut raw);
        sdk_status("iplSerializedObjectCreate", status)?;
        Ok(Self {
            raw: non_null("iplSerializedObjectCreate", status, raw)?,
            _context: PhantomData,
            _data: PhantomData,
        })
    }

    fn raw(&self) -> ffi::IPLSerializedObject {
        self.raw.as_ptr()
    }

    fn copy_bytes(&self) -> Vec<u8> {
        ffi::serialized_object_copy_bytes(self.raw())
    }
}

impl Drop for SerializedObject<'_, '_> {
    fn drop(&mut self) {
        let mut raw = self.raw.as_ptr();
        ffi::serialized_object_release(&mut raw);
    }
}

struct BoundSimulator<'context, 'scene, 'probe> {
    raw: NonNull<ffi::IPLSimulatorOpaque>,
    probe_batch: &'probe ProbeBatch<'context>,
    _scene: PhantomData<&'scene Scene<'context>>,
}

impl<'context, 'scene, 'probe> BoundSimulator<'context, 'scene, 'probe> {
    fn create(
        context: &'context Context,
        scene: &'scene Scene<'context>,
        probe_batch: &'probe ProbeBatch<'context>,
        audio: AudioConfig,
        config: S3SimulationConfig,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLSimulationSettings {
            flags: all_simulation_flags(),
            sceneType: ffi::IPL_SCENETYPE_DEFAULT,
            reflectionType: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
            maxNumOcclusionSamples: config.max_occlusion_samples,
            maxNumRays: config.reflection_rays,
            numDiffuseSamples: config.diffuse_samples,
            maxDuration: config.reflection_duration_s,
            // 4.8.1 passes this single capacity into both reflection IR and
            // pathing SH state allocation. Runtime pathingOrder may therefore
            // never exceed maxOrder even when reflections use a lower order.
            maxOrder: config.reflection_order.max(config.pathing_order),
            maxNumSources: 1,
            numThreads: config.simulation_threads,
            rayBatchSize: config.ray_batch_size,
            numVisSamples: config.pathing_visibility_samples,
            samplingRate: audio.sample_rate_hz,
            frameSize: audio.frame_size,
            openCLDevice: core::ptr::null_mut(),
            radeonRaysDevice: core::ptr::null_mut(),
            tanDevice: core::ptr::null_mut(),
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::simulator_create(context.raw(), &mut settings, &mut raw);
        sdk_status("iplSimulatorCreate", status)?;
        let simulator = Self {
            raw: non_null("iplSimulatorCreate", status, raw)?,
            probe_batch,
            _scene: PhantomData,
        };
        ffi::simulator_set_scene(simulator.raw(), scene.raw());
        ffi::simulator_add_probe_batch(simulator.raw(), probe_batch.raw());
        ffi::simulator_commit(simulator.raw());
        Ok(simulator)
    }

    fn raw(&self) -> ffi::IPLSimulator {
        self.raw.as_ptr()
    }

    fn set_shared_inputs(&self, inputs: &mut ffi::IPLSimulationSharedInputs) {
        ffi::simulator_set_shared_inputs(self.raw(), all_simulation_flags(), inputs)
    }

    fn run_direct(&self) {
        ffi::simulator_run_direct(self.raw())
    }

    fn run_reflections(&self) {
        ffi::simulator_run_reflections(self.raw())
    }

    fn run_pathing(&self) {
        ffi::simulator_run_pathing(self.raw())
    }
}

impl Drop for BoundSimulator<'_, '_, '_> {
    fn drop(&mut self) {
        // Remove committed borrowed state before releasing the simulator. Any source
        // borrowing this object must already have been dropped by Rust.
        ffi::simulator_remove_probe_batch(self.raw(), self.probe_batch.raw());
        ffi::simulator_commit(self.raw());
        let mut raw = self.raw.as_ptr();
        ffi::simulator_release(&mut raw);
    }
}

struct SimulationSource<'simulator, 'context, 'scene, 'probe> {
    raw: NonNull<ffi::IPLSourceOpaque>,
    simulator: &'simulator BoundSimulator<'context, 'scene, 'probe>,
}

impl<'simulator, 'context, 'scene, 'probe> SimulationSource<'simulator, 'context, 'scene, 'probe> {
    fn create(
        simulator: &'simulator BoundSimulator<'context, 'scene, 'probe>,
    ) -> Result<Self, BackendError> {
        let mut settings = ffi::IPLSourceSettings {
            flags: all_simulation_flags(),
        };
        let mut raw = core::ptr::null_mut();
        let status = ffi::source_create(simulator.raw(), &mut settings, &mut raw);
        sdk_status("iplSourceCreate", status)?;
        let source = Self {
            raw: non_null("iplSourceCreate", status, raw)?,
            simulator,
        };
        ffi::source_add(source.raw(), simulator.raw());
        ffi::simulator_commit(simulator.raw());
        Ok(source)
    }

    fn raw(&self) -> ffi::IPLSource {
        self.raw.as_ptr()
    }

    fn set_inputs(&self, inputs: &mut ffi::IPLSimulationInputs) {
        ffi::source_set_inputs(self.raw(), all_simulation_flags(), inputs)
    }

    fn get_outputs(&self, flags: i32, outputs: &mut ffi::IPLSimulationOutputs) {
        ffi::source_get_outputs(self.raw(), flags, outputs)
    }
}

impl Drop for SimulationSource<'_, '_, '_, '_> {
    fn drop(&mut self) {
        ffi::source_remove(self.raw(), self.simulator.raw());
        ffi::simulator_commit(self.simulator.raw());
        let mut raw = self.raw.as_ptr();
        ffi::source_release(&mut raw);
    }
}

struct RawReflectionSnapshot {
    owned: ReflectionSnapshot,
    ir: ffi::IPLReflectionEffectIR,
    tan_slot: i32,
}

pub fn render_s0(request: &S0RenderRequest) -> Result<S0RenderOutput, BackendError> {
    validate_audio(request.audio)?;
    validate_listener(request.listener)?;
    validate_position(request.source_position_enu)?;
    validate_signal(&request.input_mono, request.calibration_gain)?;

    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let mut audio_settings = raw_audio_settings(request.audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings)?;
    let mut direct_effect = DirectEffect::create(&context, &mut audio_settings)?;
    let mut binaural_effect = BinauralEffect::create(&context, &mut audio_settings, &hrtf)?;
    let mut input_buffer = AudioBuffer::allocate(&context, 1, request.audio.frame_size)?;
    let mut direct_buffer = AudioBuffer::allocate(&context, 1, request.audio.frame_size)?;
    let mut stereo_buffer = AudioBuffer::allocate(&context, 2, request.audio.frame_size)?;

    let source = raw_vector(request.source_position_enu);
    let listener = raw_vector(request.listener.position_enu);
    let mut distance_model = default_distance_model();
    let distance_attenuation =
        ffi::distance_attenuation_calculate(context.raw(), source, listener, &mut distance_model);
    let air_absorption = if request.apply_air_absorption {
        let mut model = default_air_absorption_model();
        ffi::air_absorption_calculate(context.raw(), source, listener, &mut model)
    } else {
        [1.0; 3]
    };
    let relative_direction_steam =
        relative_direction(request.source_position_enu, request.listener)?;

    let mut direct_params = ffi::IPLDirectEffectParams {
        flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
            | if request.apply_air_absorption {
                ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION
            } else {
                0
            },
        transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
        distanceAttenuation: distance_attenuation,
        airAbsorption: air_absorption,
        directivity: 1.0,
        occlusion: 1.0,
        transmission: [1.0; 3],
    };
    let mut binaural_params = ffi::IPLBinauralEffectParams {
        direction: raw_steam_vector(relative_direction_steam),
        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
        spatialBlend: 1.0,
        hrtf: hrtf.raw(),
        peakDelays: core::ptr::null_mut(),
    };

    let block_size = request.audio.frame_size as usize;
    let mut block = vec![0.0; block_size];
    let mut stereo_block = vec![0.0; block_size * 2];
    let mut interleaved = Vec::with_capacity(request.input_mono.len() * 2);
    for source_block in request.input_mono.chunks(block_size) {
        block.fill(0.0);
        for (output, input) in block.iter_mut().zip(source_block.iter().copied()) {
            *output = input * request.calibration_gain;
        }
        input_buffer.write_interleaved(&mut block);
        direct_effect.apply(&mut direct_params, &mut input_buffer, &mut direct_buffer);
        binaural_effect.apply(&mut binaural_params, &mut direct_buffer, &mut stereo_buffer);
        stereo_buffer.read_interleaved(&mut stereo_block);
        interleaved.extend_from_slice(&stereo_block[..source_block.len() * 2]);
    }

    if !distance_attenuation.is_finite()
        || !air_absorption.into_iter().all(f32::is_finite)
        || !interleaved.iter().all(|sample| sample.is_finite())
    {
        return Err(BackendError::NonFiniteOutput { output: "S0" });
    }

    Ok(S0RenderOutput {
        stereo: OwnedStereoPcm {
            sample_rate_hz: request.audio.sample_rate_hz,
            frame_count: request.input_mono.len(),
            interleaved,
        },
        distance_attenuation,
        air_absorption,
        relative_direction_steam,
    })
}

pub fn bake_s3(request: &S3BakeRequest) -> Result<BakedProbeBatch, BackendError> {
    validate_bake_config(request)?;
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.mesh)?;
    let (probe_array, floor_probe_count) =
        ProbeArray::generate_uniform_floor(&context, &scene, request.probes)?;
    let probe_batch = ProbeBatch::from_array(&context, &probe_array)?;
    // An empty layer list must not touch the batch at all, so a request that
    // predates elevated layers serializes to the same bytes it always did.
    let probe_count = if request.elevated_probe_layers.is_empty() {
        floor_probe_count
    } else {
        probe_batch.add_elevated_layers(
            &request.elevated_probe_layers,
            request.probes,
            &request.mesh,
        )?
    };

    let mut bake_params = ffi::IPLPathBakeParams {
        scene: scene.raw(),
        probeBatch: probe_batch.raw(),
        identifier: pathing_identifier(),
        numSamples: request.pathing.num_visibility_samples,
        radius: request.pathing.probe_visibility_radius_m,
        threshold: request.pathing.visibility_threshold,
        visRange: request.pathing.visibility_range_m,
        pathRange: request.pathing.path_range_m,
        numThreads: request.pathing.num_threads,
    };
    let progress = ffi::path_baker_bake(context.raw(), &mut bake_params);
    let path_data_size = probe_batch.path_data_size();
    if path_data_size == 0 {
        return Err(BackendError::PathBakeProducedNoData);
    }

    let serialized = SerializedObject::empty(&context)?;
    ffi::probe_batch_save(probe_batch.raw(), serialized.raw());
    let bytes = serialized.copy_bytes();
    if bytes.is_empty() {
        return Err(BackendError::EmptySerializedProbeBatch);
    }
    let serialized_size_bytes = bytes.len() as u64;
    let metadata = ProbeBatchMetadata {
        schema_version: PROBE_BATCH_METADATA_SCHEMA,
        steam_audio_version: STEAM_AUDIO_VERSION,
        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
        probe_count,
        path_data_size_bytes: path_data_size as u64,
        serialized_size_bytes,
        content_sha256: sha256_hex(&bytes),
        bake_progress_callback_count: progress.callback_count,
        final_bake_progress_millionths: progress_fraction_millionths(progress.final_fraction),
    };
    Ok(BakedProbeBatch { metadata, bytes })
}

pub fn render_s3(
    request: &S3RenderRequest,
    baked: &BakedProbeBatch,
) -> Result<S3RenderOutput, BackendError> {
    validate_render_config(request)?;
    baked.validate()?;

    // This operation creates every SDK handle afresh. The byte clone remains alive while
    // IPLSerializedObject borrows it; iplProbeBatchLoad copies synchronously.
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.mesh)?;
    let mut serialized_bytes = baked.bytes.clone();
    let serialized = SerializedObject::from_bytes(&context, &mut serialized_bytes)?;
    let probe_batch = ProbeBatch::load(&context, &serialized)?;
    drop(serialized);

    let loaded_probe_count = probe_batch.probe_count();
    if loaded_probe_count <= 0 || loaded_probe_count as u32 != baked.metadata.probe_count {
        return Err(BackendError::InvalidProbeBatch(
            "fresh load probe count does not match bake metadata",
        ));
    }
    let loaded_path_data_size = probe_batch.path_data_size();
    if loaded_path_data_size == 0
        || loaded_path_data_size as u64 != baked.metadata.path_data_size_bytes
    {
        return Err(BackendError::InvalidProbeBatch(
            "fresh load path-data size does not match bake metadata",
        ));
    }

    let simulator = BoundSimulator::create(
        &context,
        &scene,
        &probe_batch,
        request.audio,
        request.simulation,
    )?;
    let source = SimulationSource::create(&simulator)?;
    let mut source_inputs = simulation_inputs(request, &probe_batch)?;
    source.set_inputs(&mut source_inputs);
    let mut path_validation_trace = ffi::PathValidationTrace::default();
    let mut shared_inputs = shared_simulation_inputs(request)?;
    if request.simulation.validate_paths && request.simulation.trace_path_validation {
        shared_inputs.pathingVisCallback = ffi::path_validation_trace_callback();
        shared_inputs.pathingUserData =
            ffi::path_validation_trace_user_data(&mut path_validation_trace);
    }
    simulator.set_shared_inputs(&mut shared_inputs);

    let mut audio_settings = raw_audio_settings(request.audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings)?;

    // Each get/copy/render step completes before another simulator run. Direct and
    // path arrays become Rust-owned immediately. The opaque reflection IR cannot be
    // copied through the public C API, so it is consumed by its effect before pathing
    // can advance any SDK-owned output generation.
    simulator.run_direct();
    let direct_snapshot = copy_direct_snapshot(&source, request.simulation.direct_occlusion)?;

    simulator.run_reflections();
    let raw_reflections = copy_reflection_snapshot(&source, request.simulation, request.audio)?;
    let reflection_capacity = reflection_ir_size(
        request.simulation.reflection_duration_s,
        request.audio.sample_rate_hz,
    )?;
    let reflection_channels = ambisonics_channel_count(request.simulation.reflection_order)?;
    let reflection_render_span =
        if reflection_effect_uses_ir(request.simulation.reflection_effect.effect_type) {
            raw_reflections.owned.ir_size
        } else {
            reflection_capacity
        };
    let render_frames = render_frame_count(
        request.input_mono.len(),
        reflection_render_span,
        request.audio.frame_size,
    )?;
    let mut reflection_effect = ReflectionEffect::create(
        &context,
        &mut audio_settings,
        request.simulation.reflection_effect.effect_type,
        reflection_capacity,
        reflection_channels,
    )?;
    let mut ambisonics_binaural = AmbisonicsBinauralEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.simulation.reflection_order,
    )?;
    let reflections_stem = render_reflections_stem(
        &context,
        request,
        render_frames,
        &raw_reflections,
        &hrtf,
        &mut reflection_effect,
        &mut ambisonics_binaural,
    )?;

    simulator.run_pathing();
    // The simulator retains shared-input callback pointers. Clear the temporary
    // trace before its backing Rust allocation can leave scope.
    shared_inputs.pathingVisCallback = None;
    shared_inputs.pathingUserData = core::ptr::null_mut();
    simulator.set_shared_inputs(&mut shared_inputs);
    let path_snapshot = copy_path_snapshot(
        &source,
        request.simulation.pathing_order,
        path_validation_trace,
    )?;

    let mut direct_effect = DirectEffect::create(&context, &mut audio_settings)?;
    let mut binaural_effect = BinauralEffect::create(&context, &mut audio_settings, &hrtf)?;
    let direct_stem = render_direct_stem(
        &context,
        request,
        render_frames,
        direct_snapshot,
        &hrtf,
        &mut direct_effect,
        &mut binaural_effect,
    )?;
    let mut path_effect = PathEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.simulation.pathing_order,
    )?;
    let path_stem = render_path_stem(
        &context,
        request,
        render_frames,
        &path_snapshot,
        &hrtf,
        &mut path_effect,
    )?;

    let pathing_off_sum = sum_stereo(&direct_stem, &reflections_stem, None)?;
    let pathing_on_sum = sum_stereo(&direct_stem, &reflections_stem, Some(&path_stem))?;
    let snapshot = S3SimulationSnapshot {
        direct: direct_snapshot,
        path: path_snapshot,
        reflections: raw_reflections.owned,
    };
    Ok(S3RenderOutput {
        loaded_probe_count: loaded_probe_count as u32,
        loaded_path_data_size_bytes: loaded_path_data_size as u64,
        snapshot,
        stems: S3Stems {
            direct: direct_stem,
            path: path_stem,
            reflections: reflections_stem,
            pathing_on_sum,
            pathing_off_sum,
        },
    })
}

pub fn render_s3_trajectory(
    request: &S3TrajectoryRenderRequest,
    baked: &BakedProbeBatch,
) -> Result<S3TrajectoryRenderOutput, BackendError> {
    validate_render_config(&request.base)?;
    validate_trajectory_request(request)?;
    baked.validate()?;

    // Every SDK object below is constructed exactly once and remains alive
    // through the ordered block loop. No block delegates to render_s3.
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.base.mesh)?;
    let mut serialized_bytes = baked.bytes.clone();
    let serialized = SerializedObject::from_bytes(&context, &mut serialized_bytes)?;
    let probe_batch = ProbeBatch::load(&context, &serialized)?;
    drop(serialized);

    let loaded_probe_count = probe_batch.probe_count();
    if loaded_probe_count <= 0 || loaded_probe_count as u32 != baked.metadata.probe_count {
        return Err(BackendError::InvalidProbeBatch(
            "trajectory load probe count does not match bake metadata",
        ));
    }
    let loaded_path_data_size = probe_batch.path_data_size();
    if loaded_path_data_size == 0
        || loaded_path_data_size as u64 != baked.metadata.path_data_size_bytes
    {
        return Err(BackendError::InvalidProbeBatch(
            "trajectory load path-data size does not match bake metadata",
        ));
    }

    let simulator = BoundSimulator::create(
        &context,
        &scene,
        &probe_batch,
        request.base.audio,
        request.base.simulation,
    )?;
    let source = SimulationSource::create(&simulator)?;
    let mut source_inputs = simulation_inputs(&request.base, &probe_batch)?;
    source.set_inputs(&mut source_inputs);

    let mut audio_settings = raw_audio_settings(request.base.audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings)?;
    let mut direct_effect = DirectEffect::create(&context, &mut audio_settings)?;
    let mut binaural_effect = BinauralEffect::create(&context, &mut audio_settings, &hrtf)?;
    let mut path_effect = PathEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.base.simulation.pathing_order,
    )?;
    let maximum_ir_size = reflection_ir_size(
        request.base.simulation.reflection_duration_s,
        request.base.audio.sample_rate_hz,
    )?;
    let reflection_channels = ambisonics_channel_count(request.base.simulation.reflection_order)?;
    let mut reflection_effect = ReflectionEffect::create(
        &context,
        &mut audio_settings,
        request.base.simulation.reflection_effect.effect_type,
        maximum_ir_size,
        reflection_channels,
    )?;
    let mut ambisonics_binaural = AmbisonicsBinauralEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.base.simulation.reflection_order,
    )?;

    // Audio buffers are retained too; only Rust-owned block vectors are copied.
    let mut input_buffer = AudioBuffer::allocate(&context, 1, request.base.audio.frame_size)?;
    let mut direct_buffer = AudioBuffer::allocate(&context, 1, request.base.audio.frame_size)?;
    let mut direct_stereo_buffer =
        AudioBuffer::allocate(&context, 2, request.base.audio.frame_size)?;
    let mut path_stereo_buffer = AudioBuffer::allocate(&context, 2, request.base.audio.frame_size)?;
    let mut reflection_ambisonics_buffer =
        AudioBuffer::allocate(&context, reflection_channels, request.base.audio.frame_size)?;
    let mut reflection_stereo_buffer =
        AudioBuffer::allocate(&context, 2, request.base.audio.frame_size)?;

    let block_frames = request.base.audio.frame_size as usize;
    let mut blocks = Vec::with_capacity(request.listener_trajectory.len());
    let mut summed_interleaved = Vec::with_capacity(request.base.input_mono.len() * 2);

    for (block_index, listener) in request.listener_trajectory.iter().copied().enumerate() {
        let input_start = block_index * block_frames;
        let input_end = input_start + block_frames;
        let mut mono = request.base.input_mono[input_start..input_end].to_vec();
        for sample in &mut mono {
            *sample *= request.base.calibration_gain;
        }
        input_buffer.write_interleaved(&mut mono);

        let mut block_request = request.base.clone();
        block_request.listener = listener;
        block_request.input_mono = request.base.input_mono[input_start..input_end].to_vec();
        let mut shared_inputs = shared_simulation_inputs(&block_request)?;
        simulator.set_shared_inputs(&mut shared_inputs);

        simulator.run_direct();
        let direct_snapshot =
            copy_direct_snapshot(&source, request.base.simulation.direct_occlusion)?;
        let mut direct_params = ffi::IPLDirectEffectParams {
            flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYDIRECTIVITY
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYOCCLUSION,
            transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
            distanceAttenuation: direct_snapshot.distance_attenuation,
            airAbsorption: direct_snapshot.air_absorption,
            directivity: direct_snapshot.directivity,
            occlusion: direct_snapshot.occlusion,
            transmission: direct_snapshot.transmission,
        };
        let mut direct_binaural_params = ffi::IPLBinauralEffectParams {
            direction: raw_steam_vector(relative_direction(
                request.base.source_position_enu,
                listener,
            )?),
            interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
            spatialBlend: 1.0,
            hrtf: hrtf.raw(),
            peakDelays: core::ptr::null_mut(),
        };
        direct_effect.apply(&mut direct_params, &mut input_buffer, &mut direct_buffer);
        binaural_effect.apply(
            &mut direct_binaural_params,
            &mut direct_buffer,
            &mut direct_stereo_buffer,
        );
        let direct_stem =
            copy_stereo_block(request.base.audio, &mut direct_stereo_buffer, block_frames)?;

        // Consume the exact reflection IR before any later simulator run can
        // advance the source output generation.
        simulator.run_reflections();
        let raw_reflections =
            copy_reflection_snapshot(&source, request.base.simulation, request.base.audio)?;
        let mut reflection_params = ffi::IPLReflectionEffectParams {
            type_: reflection_effect_ffi_type(
                request.base.simulation.reflection_effect.effect_type,
            )?,
            ir: raw_reflections.ir,
            reverbTimes: raw_reflections.owned.reverb_times,
            eq: raw_reflections.owned.eq,
            delay: raw_reflections.owned.delay_samples,
            numChannels: raw_reflections.owned.num_channels,
            // Exact per-block SDK output, not the creation capacity.
            irSize: raw_reflections.owned.ir_size,
            tanDevice: core::ptr::null_mut(),
            tanSlot: raw_reflections.tan_slot,
        };
        let mut reflection_binaural_params = ffi::IPLAmbisonicsBinauralEffectParams {
            hrtf: hrtf.raw(),
            order: request.base.simulation.reflection_order,
        };
        reflection_effect.apply(
            &mut reflection_params,
            &mut input_buffer,
            &mut reflection_ambisonics_buffer,
        );
        ambisonics_binaural.apply(
            &mut reflection_binaural_params,
            &mut reflection_ambisonics_buffer,
            &mut reflection_stereo_buffer,
        );
        let reflections_stem = copy_stereo_block(
            request.base.audio,
            &mut reflection_stereo_buffer,
            block_frames,
        )?;

        let mut path_validation_trace = ffi::PathValidationTrace::default();
        if request.base.simulation.validate_paths && request.base.simulation.trace_path_validation {
            shared_inputs.pathingVisCallback = ffi::path_validation_trace_callback();
            shared_inputs.pathingUserData =
                ffi::path_validation_trace_user_data(&mut path_validation_trace);
            simulator.set_shared_inputs(&mut shared_inputs);
        }
        simulator.run_pathing();
        shared_inputs.pathingVisCallback = None;
        shared_inputs.pathingUserData = core::ptr::null_mut();
        simulator.set_shared_inputs(&mut shared_inputs);
        let path_snapshot = copy_path_snapshot(
            &source,
            request.base.simulation.pathing_order,
            path_validation_trace,
        )?;
        let mut path_coefficients = path_snapshot.sh_coeffs.clone();
        let mut path_params = ffi::IPLPathEffectParams {
            eqCoeffs: path_snapshot.eq_coeffs,
            shCoeffs: path_coefficients.as_mut_ptr(),
            order: path_snapshot.configured_order,
            binaural: ffi::IPL_TRUE,
            hrtf: hrtf.raw(),
            listener: raw_coordinate_space(listener)?,
            normalizeEQ: ffi::IPL_FALSE,
        };
        path_effect.apply(&mut path_params, &mut input_buffer, &mut path_stereo_buffer);
        let path_stem =
            copy_stereo_block(request.base.audio, &mut path_stereo_buffer, block_frames)?;

        let pathing_off_sum = sum_stereo(&direct_stem, &reflections_stem, None)?;
        let pathing_on_sum = sum_stereo(&direct_stem, &reflections_stem, Some(&path_stem))?;
        let summed = pathing_on_sum.clone();
        summed_interleaved.extend_from_slice(&summed.interleaved);
        let path_strength = path_snapshot
            .sh_coeffs
            .iter()
            .map(|coefficient| coefficient * coefficient)
            .sum::<f32>()
            .sqrt();
        if !path_strength.is_finite() {
            return Err(BackendError::NonFiniteOutput {
                output: "trajectory path strength",
            });
        }
        blocks.push(S3TrajectoryBlock {
            block_index,
            listener,
            direct_occlusion: direct_snapshot.occlusion,
            path_strength,
            snapshot: S3SimulationSnapshot {
                direct: direct_snapshot,
                path: path_snapshot,
                reflections: raw_reflections.owned,
            },
            direct_path_reflection_stems: S3Stems {
                direct: direct_stem,
                path: path_stem,
                reflections: reflections_stem,
                pathing_on_sum,
                pathing_off_sum,
            },
            summed,
        });
    }

    let summed_blocks = blocks
        .iter()
        .map(|block| block.summed.clone())
        .collect::<Vec<_>>();
    let continuity = measure_s3_summed_boundary_continuity(
        &summed_blocks,
        S3_CONTINUITY_WINDOW_FRAMES,
        S3_CONTINUITY_STEP_TO_PEAK_THRESHOLD,
    )?;
    Ok(S3TrajectoryRenderOutput {
        loaded_probe_count: loaded_probe_count as u32,
        loaded_path_data_size_bytes: loaded_path_data_size as u64,
        retained: S3RetainedSessionStats {
            context_generations: 1,
            scene_generations: 1,
            probe_batch_loads: 1,
            simulator_generations: 1,
            source_generations: 1,
            hrtf_generations: 1,
            effect_graph_generations: 1,
            rendered_blocks: blocks.len() as u32,
        },
        summed: OwnedStereoPcm {
            sample_rate_hz: request.base.audio.sample_rate_hz,
            frame_count: request.base.input_mono.len(),
            interleaved: summed_interleaved,
        },
        blocks,
        continuity,
    })
}

pub fn benchmark_s3_stages(
    request: &S3BenchmarkRequest,
    baked: &BakedProbeBatch,
) -> Result<S3BenchmarkOutput, BackendError> {
    validate_render_config(&request.render)?;
    validate_benchmark_request(request)?;
    baked.validate()?;

    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.render.mesh)?;
    let mut serialized_bytes = baked.bytes.clone();
    let serialized = SerializedObject::from_bytes(&context, &mut serialized_bytes)?;
    let probe_batch = ProbeBatch::load(&context, &serialized)?;
    drop(serialized);
    let loaded_probe_count = probe_batch.probe_count();
    let loaded_path_data_size = probe_batch.path_data_size();
    if loaded_probe_count <= 0 || loaded_probe_count as u32 != baked.metadata.probe_count {
        return Err(BackendError::InvalidProbeBatch(
            "benchmark load probe count does not match bake metadata",
        ));
    }
    if loaded_path_data_size == 0
        || loaded_path_data_size as u64 != baked.metadata.path_data_size_bytes
    {
        return Err(BackendError::InvalidProbeBatch(
            "benchmark load path-data size does not match bake metadata",
        ));
    }

    let simulator = BoundSimulator::create(
        &context,
        &scene,
        &probe_batch,
        request.render.audio,
        request.render.simulation,
    )?;
    let source = SimulationSource::create(&simulator)?;
    let mut source_inputs = simulation_inputs(&request.render, &probe_batch)?;
    source.set_inputs(&mut source_inputs);
    let mut shared_inputs = shared_simulation_inputs(&request.render)?;
    // Benchmark timings never enable the diagnostic visualization callback.
    shared_inputs.pathingVisCallback = None;
    shared_inputs.pathingUserData = core::ptr::null_mut();
    simulator.set_shared_inputs(&mut shared_inputs);

    let iterations = request.iterations;
    for _ in 0..iterations.simulation_warmup {
        simulator.run_direct();
    }
    let mut direct_simulation_ns = Vec::with_capacity(iterations.simulation_measured as usize);
    let mut direct_snapshot = None;
    for _ in 0..iterations.simulation_measured {
        direct_simulation_ns.push(elapsed_ns(|| simulator.run_direct()));
        direct_snapshot = Some(copy_direct_snapshot(
            &source,
            request.render.simulation.direct_occlusion,
        )?);
    }
    let direct_snapshot = direct_snapshot.ok_or(BackendError::InvalidInput(
        "benchmark direct simulation measured count must be positive",
    ))?;

    for _ in 0..iterations.simulation_warmup {
        simulator.run_pathing();
    }
    let mut path_simulation_ns = Vec::with_capacity(iterations.simulation_measured as usize);
    let mut path_snapshot = None;
    for _ in 0..iterations.simulation_measured {
        path_simulation_ns.push(elapsed_ns(|| simulator.run_pathing()));
        path_snapshot = Some(copy_path_snapshot(
            &source,
            request.render.simulation.pathing_order,
            ffi::PathValidationTrace::default(),
        )?);
    }
    let path_snapshot = path_snapshot.ok_or(BackendError::InvalidInput(
        "benchmark path simulation measured count must be positive",
    ))?;

    for _ in 0..iterations.reflection_warmup {
        simulator.run_reflections();
    }
    let mut reflection_simulation_ns = Vec::with_capacity(iterations.reflection_measured as usize);
    let mut raw_reflections = None;
    for _ in 0..iterations.reflection_measured {
        reflection_simulation_ns.push(elapsed_ns(|| simulator.run_reflections()));
        raw_reflections = Some(copy_reflection_snapshot(
            &source,
            request.render.simulation,
            request.render.audio,
        )?);
    }
    // No simulator run may occur after this point: the reflection IR is borrowed
    // SDK output and must remain at the generation copied above while effects run.
    let raw_reflections = raw_reflections.ok_or(BackendError::InvalidInput(
        "benchmark reflection simulation measured count must be positive",
    ))?;

    let mut audio_settings = raw_audio_settings(request.render.audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings)?;
    let mut direct_effect = DirectEffect::create(&context, &mut audio_settings)?;
    let mut binaural_effect = BinauralEffect::create(&context, &mut audio_settings, &hrtf)?;
    let mut path_effect = PathEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.render.simulation.pathing_order,
    )?;
    let reflection_capacity = reflection_ir_size(
        request.render.simulation.reflection_duration_s,
        request.render.audio.sample_rate_hz,
    )?;
    let reflection_channels = ambisonics_channel_count(request.render.simulation.reflection_order)?;
    let mut reflection_effect = ReflectionEffect::create(
        &context,
        &mut audio_settings,
        request.render.simulation.reflection_effect.effect_type,
        reflection_capacity,
        reflection_channels,
    )?;
    let mut reflection_decode = AmbisonicsBinauralEffect::create(
        &context,
        &mut audio_settings,
        &hrtf,
        request.render.simulation.reflection_order,
    )?;

    let frame_size = request.render.audio.frame_size as usize;
    let mut mono = request.render.input_mono.clone();
    for sample in &mut mono {
        *sample *= request.render.calibration_gain;
    }
    let mut input_buffer = AudioBuffer::allocate(&context, 1, request.render.audio.frame_size)?;
    input_buffer.write_interleaved(&mut mono);
    let mut direct_buffer = AudioBuffer::allocate(&context, 1, request.render.audio.frame_size)?;
    let mut direct_stereo = AudioBuffer::allocate(&context, 2, request.render.audio.frame_size)?;
    let mut path_stereo = AudioBuffer::allocate(&context, 2, request.render.audio.frame_size)?;
    let mut reflection_ambisonics = AudioBuffer::allocate(
        &context,
        reflection_channels,
        request.render.audio.frame_size,
    )?;
    let mut reflection_stereo =
        AudioBuffer::allocate(&context, 2, request.render.audio.frame_size)?;

    let mut direct_params = ffi::IPLDirectEffectParams {
        flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYDIRECTIVITY
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYOCCLUSION,
        transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
        distanceAttenuation: direct_snapshot.distance_attenuation,
        airAbsorption: direct_snapshot.air_absorption,
        directivity: direct_snapshot.directivity,
        occlusion: direct_snapshot.occlusion,
        transmission: direct_snapshot.transmission,
    };
    let mut direct_binaural_params = ffi::IPLBinauralEffectParams {
        direction: raw_steam_vector(relative_direction(
            request.render.source_position_enu,
            request.render.listener,
        )?),
        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
        spatialBlend: 1.0,
        hrtf: hrtf.raw(),
        peakDelays: core::ptr::null_mut(),
    };
    let mut path_coefficients = path_snapshot.sh_coeffs.clone();
    let mut path_params = ffi::IPLPathEffectParams {
        eqCoeffs: path_snapshot.eq_coeffs,
        shCoeffs: path_coefficients.as_mut_ptr(),
        order: path_snapshot.configured_order,
        binaural: ffi::IPL_TRUE,
        hrtf: hrtf.raw(),
        listener: raw_coordinate_space(request.render.listener)?,
        normalizeEQ: ffi::IPL_FALSE,
    };
    let mut reflection_params = ffi::IPLReflectionEffectParams {
        type_: reflection_effect_ffi_type(request.render.simulation.reflection_effect.effect_type)?,
        ir: raw_reflections.ir,
        reverbTimes: raw_reflections.owned.reverb_times,
        eq: raw_reflections.owned.eq,
        delay: raw_reflections.owned.delay_samples,
        numChannels: raw_reflections.owned.num_channels,
        irSize: raw_reflections.owned.ir_size,
        tanDevice: core::ptr::null_mut(),
        tanSlot: raw_reflections.tan_slot,
    };
    let mut reflection_decode_params = ffi::IPLAmbisonicsBinauralEffectParams {
        hrtf: hrtf.raw(),
        order: request.render.simulation.reflection_order,
    };

    let mut direct_effect_binaural_apply_ns =
        Vec::with_capacity(iterations.effect_measured as usize);
    let mut path_effect_apply_ns = Vec::with_capacity(iterations.effect_measured as usize);
    let mut reflection_effect_decode_apply_ns =
        Vec::with_capacity(iterations.effect_measured as usize);
    let mut direct_readback = vec![0.0; frame_size * 2];
    let mut path_readback = vec![0.0; frame_size * 2];
    let mut reflection_readback = vec![0.0; frame_size * 2];
    let mut direct_effect_samples_checked = 0;
    let mut path_effect_samples_checked = 0;
    let mut reflection_effect_samples_checked = 0;

    let executed_effect_blocks = iterations.effect_warmup + iterations.effect_measured;
    for index in 0..executed_effect_blocks {
        let direct_ns = elapsed_ns(|| {
            direct_effect.apply(&mut direct_params, &mut input_buffer, &mut direct_buffer);
            binaural_effect.apply(
                &mut direct_binaural_params,
                &mut direct_buffer,
                &mut direct_stereo,
            );
        });
        let path_ns =
            elapsed_ns(|| path_effect.apply(&mut path_params, &mut input_buffer, &mut path_stereo));
        let reflection_ns = elapsed_ns(|| {
            reflection_effect.apply(
                &mut reflection_params,
                &mut input_buffer,
                &mut reflection_ambisonics,
            );
            reflection_decode.apply(
                &mut reflection_decode_params,
                &mut reflection_ambisonics,
                &mut reflection_stereo,
            );
        });
        if index >= iterations.effect_warmup {
            direct_effect_binaural_apply_ns.push(direct_ns);
            path_effect_apply_ns.push(path_ns);
            reflection_effect_decode_apply_ns.push(reflection_ns);
            if !read_buffer_is_finite(&mut direct_stereo, &mut direct_readback) {
                return Err(BackendError::NonFiniteOutput {
                    output: "benchmark direct effect sample",
                });
            }
            direct_effect_samples_checked += 1;
            if !read_buffer_is_finite(&mut path_stereo, &mut path_readback) {
                return Err(BackendError::NonFiniteOutput {
                    output: "benchmark path effect sample",
                });
            }
            path_effect_samples_checked += 1;
            if !read_buffer_is_finite(&mut reflection_stereo, &mut reflection_readback) {
                return Err(BackendError::NonFiniteOutput {
                    output: "benchmark reflection effect sample",
                });
            }
            reflection_effect_samples_checked += 1;
        }
    }

    Ok(S3BenchmarkOutput {
        loaded_probe_count: loaded_probe_count as u32,
        loaded_path_data_size_bytes: loaded_path_data_size as u64,
        retained: S3RetainedSessionStats {
            context_generations: 1,
            scene_generations: 1,
            probe_batch_loads: 1,
            simulator_generations: 1,
            source_generations: 1,
            hrtf_generations: 1,
            effect_graph_generations: 1,
            rendered_blocks: executed_effect_blocks,
        },
        iterations,
        requested_simulation: request.render.simulation,
        delivered_simulation: request.render.simulation,
        snapshot: S3SimulationSnapshot {
            direct: direct_snapshot,
            path: path_snapshot,
            reflections: raw_reflections.owned,
        },
        samples: S3StageTimingSamples {
            direct_simulation_ns,
            path_simulation_ns,
            reflection_simulation_ns,
            direct_effect_binaural_apply_ns,
            path_effect_apply_ns,
            reflection_effect_decode_apply_ns,
        },
        finite: S3BenchmarkFiniteChecks {
            direct_simulation: true,
            path_simulation: true,
            reflection_simulation: true,
            direct_effect_binaural_apply: true,
            path_effect_apply: true,
            reflection_effect_decode_apply: true,
            direct_simulation_samples_checked: iterations.simulation_measured,
            path_simulation_samples_checked: iterations.simulation_measured,
            reflection_simulation_samples_checked: iterations.reflection_measured,
            direct_effect_samples_checked,
            path_effect_samples_checked,
            reflection_effect_samples_checked,
        },
    })
}

fn elapsed_ns(operation: impl FnOnce()) -> u64 {
    let started = Instant::now();
    operation();
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn read_buffer_is_finite(buffer: &mut AudioBuffer<'_>, samples: &mut [f32]) -> bool {
    buffer.read_interleaved(samples);
    samples.iter().copied().all(f32::is_finite)
}

fn copy_stereo_block(
    audio: AudioConfig,
    buffer: &mut AudioBuffer<'_>,
    frame_count: usize,
) -> Result<OwnedStereoPcm, BackendError> {
    let mut interleaved = vec![0.0; frame_count * 2];
    buffer.read_interleaved(&mut interleaved);
    if !interleaved.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::NonFiniteOutput {
            output: "trajectory stem",
        });
    }
    Ok(OwnedStereoPcm {
        sample_rate_hz: audio.sample_rate_hz,
        frame_count,
        interleaved,
    })
}

fn copy_direct_snapshot(
    source: &SimulationSource<'_, '_, '_, '_>,
    configured_occlusion_mode: DirectOcclusionMode,
) -> Result<DirectSnapshot, BackendError> {
    let mut outputs = ffi::IPLSimulationOutputs::zeroed();
    source.get_outputs(ffi::IPL_SIMULATIONFLAGS_DIRECT, &mut outputs);
    let direct = DirectSnapshot {
        distance_attenuation: outputs.direct.distanceAttenuation,
        air_absorption: outputs.direct.airAbsorption,
        directivity: outputs.direct.directivity,
        occlusion: outputs.direct.occlusion,
        transmission: outputs.direct.transmission,
        requested_occlusion_mode: configured_occlusion_mode,
        delivered_occlusion_mode: configured_occlusion_mode,
    };
    validate_direct_snapshot(&direct)?;
    Ok(direct)
}

fn copy_reflection_snapshot(
    source: &SimulationSource<'_, '_, '_, '_>,
    config: S3SimulationConfig,
    audio: AudioConfig,
) -> Result<RawReflectionSnapshot, BackendError> {
    let mut outputs = ffi::IPLSimulationOutputs::zeroed();
    source.get_outputs(ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut outputs);
    let effect_type = config.reflection_effect.effect_type;
    let maximum_ir_size = reflection_ir_size(config.reflection_duration_s, audio.sample_rate_hz)?;
    let expected_channels = ambisonics_channel_count(config.reflection_order)?;
    if reflection_effect_uses_ir(effect_type) {
        if outputs.reflections.ir.is_null() {
            return Err(BackendError::InvalidSdkOutput(
                "IR-backed reflection effect returned a null IR",
            ));
        }
        if outputs.reflections.numChannels != expected_channels {
            return Err(BackendError::InvalidSdkOutput(
                "reflection channel count does not match configured order",
            ));
        }
        if outputs.reflections.irSize <= 0 || outputs.reflections.irSize > maximum_ir_size {
            return Err(BackendError::InvalidSdkOutput(
                "reflection irSize is outside configured capacity",
            ));
        }
    } else if outputs.reflections.numChannels != 0 || outputs.reflections.irSize != 0 {
        return Err(BackendError::InvalidSdkOutput(
            "parametric reflection output unexpectedly contains IR data",
        ));
    }
    let uses_reverb = reflection_effect_uses_reverb(effect_type);
    if uses_reverb
        && !outputs
            .reflections
            .reverbTimes
            .into_iter()
            .all(|value| value.is_finite() && value > 0.0)
    {
        return Err(BackendError::InvalidSdkOutput(
            "parametric reflection RT60 values must be finite and positive",
        ));
    }
    if effect_type == ReflectionEffectType::Hybrid
        && (!outputs
            .reflections
            .eq
            .into_iter()
            .all(|value| value.is_finite() && (0.0..=1.0).contains(&value))
            || outputs.reflections.delay <= 0)
    {
        return Err(BackendError::InvalidSdkOutput(
            "hybrid reflection EQ/delay output is invalid",
        ));
    }
    let owned = ReflectionSnapshot {
        requested_effect_type: effect_type,
        delivered_effect_type: effect_type,
        num_channels: expected_channels,
        sdk_num_channels: outputs.reflections.numChannels,
        // Copy and later use this exact SDK output value, never duration * rate.
        ir_size: outputs.reflections.irSize,
        reverb_times: outputs.reflections.reverbTimes,
        eq: outputs.reflections.eq,
        delay_samples: outputs.reflections.delay,
        configured_hybrid_transition_time_s: config.reflection_effect.hybrid_transition_time_s,
        configured_hybrid_overlap_percent: config.reflection_effect.hybrid_overlap_percent,
        applied_reverb_times: uses_reverb.then_some(outputs.reflections.reverbTimes),
        applied_hybrid_eq: (effect_type == ReflectionEffectType::Hybrid)
            .then_some(outputs.reflections.eq),
        applied_hybrid_delay_samples: (effect_type == ReflectionEffectType::Hybrid)
            .then_some(outputs.reflections.delay),
    };
    if !owned.reverb_times.into_iter().all(f32::is_finite)
        || !owned.eq.into_iter().all(f32::is_finite)
    {
        return Err(BackendError::NonFiniteOutput {
            output: "reflection simulation",
        });
    }
    Ok(RawReflectionSnapshot {
        owned,
        ir: outputs.reflections.ir,
        tan_slot: outputs.reflections.tanSlot,
    })
}

fn copy_path_snapshot(
    source: &SimulationSource<'_, '_, '_, '_>,
    configured_order: i32,
    validation_trace: ffi::PathValidationTrace,
) -> Result<PathSnapshot, BackendError> {
    let mut outputs = ffi::IPLSimulationOutputs::zeroed();
    source.get_outputs(ffi::IPL_SIMULATIONFLAGS_PATHING, &mut outputs);
    let coefficient_count = ambisonics_channel_count(configured_order)? as usize;
    let sh_coeffs = ffi::copy_path_coefficients(outputs.pathing.shCoeffs, coefficient_count)
        .ok_or(BackendError::InvalidSdkOutput(
            "path SH coefficient pointer is null",
        ))?;
    let snapshot = PathSnapshot {
        eq_coeffs: outputs.pathing.eqCoeffs,
        direction: decode_path_direction_enu(configured_order, &sh_coeffs)?,
        sh_coeffs,
        // 4.8.1 does not write outputs.pathing.order.
        configured_order,
        validation_segments: validation_trace
            .into_segments()
            .into_iter()
            .map(|segment| PathValidationSegment {
                from_enu_m: steam_to_enu(SteamVector3::new(
                    segment.from.x,
                    segment.from.y,
                    segment.from.z,
                )),
                to_enu_m: steam_to_enu(SteamVector3::new(segment.to.x, segment.to.y, segment.to.z)),
                occluded: segment.occluded,
            })
            .collect(),
    };
    if !snapshot.eq_coeffs.into_iter().all(f32::is_finite)
        || !snapshot.sh_coeffs.iter().copied().all(f32::is_finite)
    {
        return Err(BackendError::NonFiniteOutput {
            output: "path simulation",
        });
    }
    Ok(snapshot)
}

fn render_direct_stem(
    context: &Context,
    request: &S3RenderRequest,
    frame_count: usize,
    snapshot: DirectSnapshot,
    hrtf: &Hrtf<'_>,
    direct_effect: &mut DirectEffect<'_>,
    binaural_effect: &mut BinauralEffect<'_, '_>,
) -> Result<OwnedStereoPcm, BackendError> {
    let mut input_buffer = AudioBuffer::allocate(context, 1, request.audio.frame_size)?;
    let mut direct_buffer = AudioBuffer::allocate(context, 1, request.audio.frame_size)?;
    let mut stereo_buffer = AudioBuffer::allocate(context, 2, request.audio.frame_size)?;
    let relative_direction = relative_direction(request.source_position_enu, request.listener)?;
    let mut direct_params = ffi::IPLDirectEffectParams {
        // SourceGetOutputs leaves flags unset; the owned graph selects them explicitly.
        flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYDIRECTIVITY
            | ffi::IPL_DIRECTEFFECTFLAGS_APPLYOCCLUSION,
        transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
        distanceAttenuation: snapshot.distance_attenuation,
        airAbsorption: snapshot.air_absorption,
        directivity: snapshot.directivity,
        occlusion: snapshot.occlusion,
        transmission: snapshot.transmission,
    };
    let mut binaural_params = ffi::IPLBinauralEffectParams {
        direction: raw_steam_vector(relative_direction),
        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
        spatialBlend: 1.0,
        hrtf: hrtf.raw(),
        peakDelays: core::ptr::null_mut(),
    };
    let mut output = render_stereo_blocks(
        request,
        frame_count,
        &mut input_buffer,
        &mut stereo_buffer,
        |input, stereo| {
            direct_effect.apply(&mut direct_params, input, &mut direct_buffer);
            binaural_effect.apply(&mut binaural_params, &mut direct_buffer, stereo);
        },
    )?;
    output.sample_rate_hz = request.audio.sample_rate_hz;
    Ok(output)
}

fn render_path_stem(
    context: &Context,
    request: &S3RenderRequest,
    frame_count: usize,
    snapshot: &PathSnapshot,
    hrtf: &Hrtf<'_>,
    path_effect: &mut PathEffect<'_, '_>,
) -> Result<OwnedStereoPcm, BackendError> {
    let mut input_buffer = AudioBuffer::allocate(context, 1, request.audio.frame_size)?;
    let mut stereo_buffer = AudioBuffer::allocate(context, 2, request.audio.frame_size)?;
    let mut sh_coeffs = snapshot.sh_coeffs.clone();
    let mut params = ffi::IPLPathEffectParams {
        eqCoeffs: snapshot.eq_coeffs,
        shCoeffs: sh_coeffs.as_mut_ptr(),
        // SourceGetOutputs leaves order/binaural/HRTF/listener unset.
        order: snapshot.configured_order,
        binaural: ffi::IPL_TRUE,
        hrtf: hrtf.raw(),
        listener: raw_coordinate_space(request.listener)?,
        normalizeEQ: ffi::IPL_FALSE,
    };
    render_stereo_blocks(
        request,
        frame_count,
        &mut input_buffer,
        &mut stereo_buffer,
        |input, stereo| path_effect.apply(&mut params, input, stereo),
    )
}

fn render_reflections_stem(
    context: &Context,
    request: &S3RenderRequest,
    frame_count: usize,
    snapshot: &RawReflectionSnapshot,
    hrtf: &Hrtf<'_>,
    reflection_effect: &mut ReflectionEffect<'_>,
    binaural_effect: &mut AmbisonicsBinauralEffect<'_, '_>,
) -> Result<OwnedStereoPcm, BackendError> {
    let mut input_buffer = AudioBuffer::allocate(context, 1, request.audio.frame_size)?;
    let mut ambisonics_buffer = AudioBuffer::allocate(
        context,
        snapshot.owned.num_channels,
        request.audio.frame_size,
    )?;
    let mut stereo_buffer = AudioBuffer::allocate(context, 2, request.audio.frame_size)?;
    let mut reflection_params = ffi::IPLReflectionEffectParams {
        // SourceGetOutputs leaves the reflection type unset.
        type_: reflection_effect_ffi_type(request.simulation.reflection_effect.effect_type)?,
        ir: snapshot.ir,
        reverbTimes: snapshot.owned.reverb_times,
        eq: snapshot.owned.eq,
        delay: snapshot.owned.delay_samples,
        numChannels: snapshot.owned.num_channels,
        // This is intentionally the exact outputs.reflections.irSize value.
        irSize: snapshot.owned.ir_size,
        tanDevice: core::ptr::null_mut(),
        tanSlot: snapshot.tan_slot,
    };
    let mut binaural_params = ffi::IPLAmbisonicsBinauralEffectParams {
        hrtf: hrtf.raw(),
        order: request.simulation.reflection_order,
    };
    render_stereo_blocks(
        request,
        frame_count,
        &mut input_buffer,
        &mut stereo_buffer,
        |input, stereo| {
            reflection_effect.apply(&mut reflection_params, input, &mut ambisonics_buffer);
            binaural_effect.apply(&mut binaural_params, &mut ambisonics_buffer, stereo);
        },
    )
}

fn render_stereo_blocks(
    request: &S3RenderRequest,
    frame_count: usize,
    input_buffer: &mut AudioBuffer<'_>,
    stereo_buffer: &mut AudioBuffer<'_>,
    mut process: impl FnMut(&mut AudioBuffer<'_>, &mut AudioBuffer<'_>),
) -> Result<OwnedStereoPcm, BackendError> {
    let block_size = request.audio.frame_size as usize;
    let mut mono_block = vec![0.0; block_size];
    let mut stereo_block = vec![0.0; block_size * 2];
    let mut interleaved = Vec::with_capacity(frame_count * 2);
    for block_start in (0..frame_count).step_by(block_size) {
        mono_block.fill(0.0);
        for (offset, output) in mono_block.iter_mut().enumerate() {
            if let Some(input) = request.input_mono.get(block_start + offset) {
                *output = *input * request.calibration_gain;
            }
        }
        input_buffer.write_interleaved(&mut mono_block);
        process(input_buffer, stereo_buffer);
        stereo_buffer.read_interleaved(&mut stereo_block);
        interleaved.extend_from_slice(&stereo_block);
    }
    if !interleaved.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::NonFiniteOutput {
            output: "rendered stem",
        });
    }
    Ok(OwnedStereoPcm {
        sample_rate_hz: request.audio.sample_rate_hz,
        frame_count,
        interleaved,
    })
}

fn sum_stereo(
    first: &OwnedStereoPcm,
    second: &OwnedStereoPcm,
    third: Option<&OwnedStereoPcm>,
) -> Result<OwnedStereoPcm, BackendError> {
    if first.sample_rate_hz != second.sample_rate_hz
        || first.frame_count != second.frame_count
        || first.interleaved.len() != second.interleaved.len()
        || third.is_some_and(|third| {
            third.sample_rate_hz != first.sample_rate_hz
                || third.frame_count != first.frame_count
                || third.interleaved.len() != first.interleaved.len()
        })
    {
        return Err(BackendError::InvalidSdkOutput("stem lengths do not match"));
    }
    let mut interleaved = first.interleaved.clone();
    for (output, value) in interleaved.iter_mut().zip(&second.interleaved) {
        *output += value;
    }
    if let Some(third) = third {
        for (output, value) in interleaved.iter_mut().zip(&third.interleaved) {
            *output += value;
        }
    }
    if !interleaved.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::NonFiniteOutput {
            output: "pathing toggle sum",
        });
    }
    Ok(OwnedStereoPcm {
        sample_rate_hz: first.sample_rate_hz,
        frame_count: first.frame_count,
        interleaved,
    })
}

fn raw_audio_settings(config: AudioConfig) -> ffi::IPLAudioSettings {
    ffi::IPLAudioSettings {
        samplingRate: config.sample_rate_hz,
        frameSize: config.frame_size,
    }
}

fn raw_vector(vector: EnuVector3) -> ffi::IPLVector3 {
    raw_steam_vector(enu_to_steam(vector))
}

fn raw_steam_vector(vector: SteamVector3) -> ffi::IPLVector3 {
    ffi::IPLVector3 {
        x: vector.x,
        y: vector.y,
        z: vector.z,
    }
}

fn default_distance_model() -> ffi::IPLDistanceAttenuationModel {
    ffi::IPLDistanceAttenuationModel {
        type_: ffi::IPL_DISTANCEATTENUATIONTYPE_DEFAULT,
        minDistance: 1.0,
        callback: None,
        userData: core::ptr::null_mut(),
        dirty: ffi::IPL_FALSE,
    }
}

fn default_air_absorption_model() -> ffi::IPLAirAbsorptionModel {
    ffi::IPLAirAbsorptionModel {
        type_: ffi::IPL_AIRABSORPTIONTYPE_DEFAULT,
        coefficients: [0.0; 3],
        callback: None,
        userData: core::ptr::null_mut(),
        dirty: ffi::IPL_FALSE,
    }
}

fn all_simulation_flags() -> i32 {
    ffi::IPL_SIMULATIONFLAGS_DIRECT
        | ffi::IPL_SIMULATIONFLAGS_REFLECTIONS
        | ffi::IPL_SIMULATIONFLAGS_PATHING
}

fn simulation_inputs(
    request: &S3RenderRequest,
    probe_batch: &ProbeBatch<'_>,
) -> Result<ffi::IPLSimulationInputs, BackendError> {
    Ok(ffi::IPLSimulationInputs {
        flags: all_simulation_flags(),
        directFlags: ffi::IPL_DIRECTSIMULATIONFLAGS_DISTANCEATTENUATION
            | ffi::IPL_DIRECTSIMULATIONFLAGS_AIRABSORPTION
            | ffi::IPL_DIRECTSIMULATIONFLAGS_DIRECTIVITY
            | ffi::IPL_DIRECTSIMULATIONFLAGS_OCCLUSION,
        source: raw_coordinate_space(ListenerPose::at(request.source_position_enu))?,
        distanceAttenuationModel: default_distance_model(),
        airAbsorptionModel: default_air_absorption_model(),
        directivity: ffi::IPLDirectivity {
            dipoleWeight: 0.0,
            dipolePower: 1.0,
            callback: None,
            userData: core::ptr::null_mut(),
        },
        occlusionType: direct_occlusion_ffi_type(request.simulation.direct_occlusion),
        occlusionRadius: match request.simulation.direct_occlusion {
            DirectOcclusionMode::Raycast => 0.0,
            DirectOcclusionMode::Volumetric { radius_m, .. } => radius_m,
        },
        numOcclusionSamples: match request.simulation.direct_occlusion {
            DirectOcclusionMode::Raycast => 0,
            DirectOcclusionMode::Volumetric { sample_count, .. } => sample_count,
        },
        reverbScale: [1.0; 3],
        hybridReverbTransitionTime: request
            .simulation
            .reflection_effect
            .hybrid_transition_time_s
            .unwrap_or(0.0),
        hybridReverbOverlapPercent: request
            .simulation
            .reflection_effect
            .hybrid_overlap_percent
            .unwrap_or(0.0),
        baked: ffi::IPL_FALSE,
        bakedDataIdentifier: ffi::IPLBakedDataIdentifier::default(),
        pathingProbes: probe_batch.raw(),
        visRadius: request.simulation.pathing_visibility_radius_m,
        visThreshold: request.simulation.pathing_visibility_threshold,
        visRange: request.simulation.pathing_visibility_range_m,
        pathingOrder: request.simulation.pathing_order,
        enableValidation: bool_to_ipl(request.simulation.validate_paths),
        findAlternatePaths: bool_to_ipl(request.simulation.find_alternate_paths),
        numTransmissionRays: 1,
        deviationModel: core::ptr::null_mut(),
    })
}

fn shared_simulation_inputs(
    request: &S3RenderRequest,
) -> Result<ffi::IPLSimulationSharedInputs, BackendError> {
    Ok(ffi::IPLSimulationSharedInputs {
        listener: raw_coordinate_space(request.listener)?,
        numRays: request.simulation.reflection_rays,
        numBounces: request.simulation.reflection_bounces,
        duration: request.simulation.reflection_duration_s,
        order: request.simulation.reflection_order,
        irradianceMinDistance: 1.0,
        pathingVisCallback: None,
        pathingUserData: core::ptr::null_mut(),
    })
}

fn raw_coordinate_space(pose: ListenerPose) -> Result<ffi::IPLCoordinateSpace3, BackendError> {
    let ahead = normalized(pose.ahead_enu)?;
    let up = normalized(pose.up_enu)?;
    let right = normalized(cross(ahead, up))?;
    Ok(ffi::IPLCoordinateSpace3 {
        right: raw_vector(right),
        up: raw_vector(up),
        ahead: raw_vector(ahead),
        origin: raw_vector(pose.position_enu),
    })
}

fn bool_to_ipl(value: bool) -> i32 {
    if value { ffi::IPL_TRUE } else { ffi::IPL_FALSE }
}

fn direct_occlusion_ffi_type(mode: DirectOcclusionMode) -> i32 {
    let discriminant = match mode {
        DirectOcclusionMode::Raycast => ffi::IPL_OCCLUSIONTYPE_RAYCAST,
        DirectOcclusionMode::Volumetric { .. } => ffi::IPL_OCCLUSIONTYPE_VOLUMETRIC,
    };
    debug_assert_eq!(discriminant, mode.steam_audio_discriminant());
    discriminant
}

fn reflection_effect_ffi_type(effect_type: ReflectionEffectType) -> Result<i32, BackendError> {
    let discriminant = effect_type.steam_audio_cpu_discriminant()?;
    debug_assert!(matches!(
        discriminant,
        ffi::IPL_REFLECTIONEFFECTTYPE_CONVOLUTION
            | ffi::IPL_REFLECTIONEFFECTTYPE_PARAMETRIC
            | ffi::IPL_REFLECTIONEFFECTTYPE_HYBRID
    ));
    Ok(discriminant)
}

fn reflection_effect_uses_ir(effect_type: ReflectionEffectType) -> bool {
    matches!(
        effect_type,
        ReflectionEffectType::Convolution | ReflectionEffectType::Hybrid
    )
}

fn reflection_effect_uses_reverb(effect_type: ReflectionEffectType) -> bool {
    matches!(
        effect_type,
        ReflectionEffectType::Parametric | ReflectionEffectType::Hybrid
    )
}

fn ambisonics_channel_count(order: i32) -> Result<i32, BackendError> {
    let side = order
        .checked_add(1)
        .ok_or(BackendError::InvalidInput("Ambisonic order is too large"))?;
    side.checked_mul(side)
        .ok_or(BackendError::InvalidInput("Ambisonic order is too large"))
}

fn reflection_ir_size(duration_s: f32, sample_rate_hz: i32) -> Result<i32, BackendError> {
    let samples = (duration_s * sample_rate_hz as f32).ceil();
    if !samples.is_finite() || samples < 1.0 || samples > i32::MAX as f32 {
        return Err(BackendError::InvalidInput(
            "reflection duration produces an invalid IR size",
        ));
    }
    Ok(samples as i32)
}

fn render_frame_count(
    input_frames: usize,
    reflection_ir_size: i32,
    block_size: i32,
) -> Result<usize, BackendError> {
    let ir_size = usize::try_from(reflection_ir_size)
        .map_err(|_| BackendError::InvalidSdkOutput("reflection irSize cannot be represented"))?;
    let block_size = usize::try_from(block_size)
        .map_err(|_| BackendError::InvalidInput("frame size must be positive"))?;
    let unpadded = input_frames
        .checked_add(ir_size)
        .ok_or(BackendError::InvalidSdkOutput(
            "reflection render length overflows",
        ))?;
    unpadded
        .checked_add(block_size - 1)
        .map(|value| value / block_size * block_size)
        .ok_or(BackendError::InvalidSdkOutput(
            "reflection render length overflows",
        ))
}

fn pathing_identifier() -> ffi::IPLBakedDataIdentifier {
    ffi::IPLBakedDataIdentifier {
        type_: ffi::IPL_BAKEDDATATYPE_PATHING,
        variation: ffi::IPL_BAKEDDATAVARIATION_DYNAMIC,
        endpointInfluence: ffi::IPLSphere::default(),
    }
}

fn probe_transform(volume: ProbeVolume) -> ffi::IPLMatrix4x4 {
    let steam_min = SteamVector3::new(volume.min_enu_m.x, volume.min_enu_m.z, -volume.max_enu_m.y);
    let steam_max = SteamVector3::new(volume.max_enu_m.x, volume.max_enu_m.z, -volume.min_enu_m.y);
    let size = SteamVector3::new(
        steam_max.x - steam_min.x,
        steam_max.y - steam_min.y,
        steam_max.z - steam_min.z,
    );
    let center = SteamVector3::new(
        (steam_min.x + steam_max.x) * 0.5,
        (steam_min.y + steam_max.y) * 0.5,
        (steam_min.z + steam_max.z) * 0.5,
    );
    // Despite the public header describing a [0, 1] unit cube, the exact
    // 4.8.1 ProbeGenerator implementation samples [-0.5, 0.5]. This
    // scale-and-center matrix follows the implementation and official itest.
    ffi::IPLMatrix4x4 {
        elements: [
            [size.x, 0.0, 0.0, center.x],
            [0.0, size.y, 0.0, center.y],
            [0.0, 0.0, size.z, center.z],
            [0.0, 0.0, 0.0, 1.0],
        ],
    }
}

fn progress_fraction_millionths(progress: f32) -> u32 {
    if !progress.is_finite() {
        return 0;
    }
    (progress.clamp(0.0, 1.0) * 1_000_000.0).round() as u32
}

fn validate_bake_config(request: &S3BakeRequest) -> Result<(), BackendError> {
    validate_mesh(&request.mesh)?;
    validate_probe_volume(request.probes)?;
    for layer in &request.elevated_probe_layers {
        validate_elevated_probe_layer(*layer)?;
    }
    let config = request.pathing;
    if config.num_visibility_samples <= 0 {
        return Err(BackendError::InvalidInput(
            "path bake visibility sample count must be positive",
        ));
    }
    if !config.probe_visibility_radius_m.is_finite() || config.probe_visibility_radius_m < 0.0 {
        return Err(BackendError::InvalidInput(
            "path bake visibility radius must be finite and non-negative",
        ));
    }
    if !config.visibility_threshold.is_finite()
        || !(0.0..=1.0).contains(&config.visibility_threshold)
    {
        return Err(BackendError::InvalidInput(
            "path bake visibility threshold must be between zero and one",
        ));
    }
    if !config.visibility_range_m.is_finite()
        || config.visibility_range_m <= 0.0
        || !config.path_range_m.is_finite()
        || config.path_range_m <= 0.0
    {
        return Err(BackendError::InvalidInput(
            "path bake visibility and path ranges must be finite and positive",
        ));
    }
    if config.num_threads <= 0 {
        return Err(BackendError::InvalidInput(
            "path bake thread count must be positive",
        ));
    }
    Ok(())
}

fn validate_render_config(request: &S3RenderRequest) -> Result<(), BackendError> {
    validate_audio(request.audio)?;
    validate_mesh(&request.mesh)?;
    validate_listener(request.listener)?;
    validate_position(request.source_position_enu)?;
    validate_signal(&request.input_mono, request.calibration_gain)?;
    if request.source_position_enu == request.listener.position_enu {
        return Err(BackendError::InvalidInput(
            "S3 source and listener positions must differ",
        ));
    }
    let config = request.simulation;
    if config.max_occlusion_samples <= 0
        || config.reflection_rays <= 0
        || config.diffuse_samples <= 0
        || config.reflection_bounces < 0
        || config.simulation_threads <= 0
        || config.ray_batch_size <= 0
        || config.pathing_visibility_samples <= 0
    {
        return Err(BackendError::InvalidInput(
            "simulation counts must be positive (reflection bounces may be zero)",
        ));
    }
    validate_direct_occlusion(config)?;
    if !(0..=3).contains(&config.reflection_order) || !(0..=3).contains(&config.pathing_order) {
        return Err(BackendError::InvalidInput(
            "Phase A Ambisonic orders must be between zero and three",
        ));
    }
    reflection_ir_size(config.reflection_duration_s, request.audio.sample_rate_hz)?;
    validate_reflection_effect_config(config)?;
    ambisonics_channel_count(config.reflection_order)?;
    ambisonics_channel_count(config.pathing_order)?;
    if !config.pathing_visibility_radius_m.is_finite() || config.pathing_visibility_radius_m < 0.0 {
        return Err(BackendError::InvalidInput(
            "pathing visibility radius must be finite and non-negative",
        ));
    }
    if !config.pathing_visibility_threshold.is_finite()
        || !(0.0..=1.0).contains(&config.pathing_visibility_threshold)
    {
        return Err(BackendError::InvalidInput(
            "pathing visibility threshold must be between zero and one",
        ));
    }
    if !config.pathing_visibility_range_m.is_finite() || config.pathing_visibility_range_m <= 0.0 {
        return Err(BackendError::InvalidInput(
            "pathing visibility range must be finite and positive",
        ));
    }
    Ok(())
}

fn validate_benchmark_request(request: &S3BenchmarkRequest) -> Result<(), BackendError> {
    let iterations = request.iterations;
    if iterations.simulation_measured == 0
        || iterations.reflection_measured == 0
        || iterations.effect_measured == 0
    {
        return Err(BackendError::InvalidInput(
            "benchmark measured iteration counts must be positive",
        ));
    }
    if iterations
        .simulation_warmup
        .checked_add(iterations.simulation_measured)
        .is_none_or(|total| total > S3_BENCHMARK_MAX_STANDARD_ITERATIONS)
        || iterations
            .effect_warmup
            .checked_add(iterations.effect_measured)
            .is_none_or(|total| total > S3_BENCHMARK_MAX_STANDARD_ITERATIONS)
        || iterations
            .reflection_warmup
            .checked_add(iterations.reflection_measured)
            .is_none_or(|total| total > S3_BENCHMARK_MAX_REFLECTION_ITERATIONS)
    {
        return Err(BackendError::InvalidInput(
            "benchmark iteration count exceeds the offline safety bound",
        ));
    }
    if request.render.input_mono.len() != request.render.audio.frame_size as usize {
        return Err(BackendError::InvalidInput(
            "benchmark input must contain exactly one audio frame",
        ));
    }
    if request.render.simulation.trace_path_validation {
        return Err(BackendError::InvalidInput(
            "benchmark path timing cannot enable validation tracing",
        ));
    }
    let simulation = request.render.simulation;
    if simulation.max_occlusion_samples > S3_BENCHMARK_MAX_OCCLUSION_SAMPLES
        || simulation.reflection_rays > S3_BENCHMARK_MAX_REFLECTION_RAYS
        || simulation.diffuse_samples > S3_BENCHMARK_MAX_DIFFUSE_SAMPLES
        || simulation.reflection_bounces > S3_BENCHMARK_MAX_REFLECTION_BOUNCES
        || simulation.simulation_threads > S3_BENCHMARK_MAX_SIMULATION_THREADS
        || simulation.ray_batch_size > S3_BENCHMARK_MAX_RAY_BATCH_SIZE
    {
        return Err(BackendError::InvalidInput(
            "benchmark simulation resource setting exceeds the offline safety bound",
        ));
    }
    if reflection_ir_size(
        simulation.reflection_duration_s,
        request.render.audio.sample_rate_hz,
    )? > S3_BENCHMARK_MAX_REFLECTION_IR_SAMPLES
    {
        return Err(BackendError::InvalidInput(
            "benchmark reflection IR capacity exceeds the offline safety bound",
        ));
    }
    Ok(())
}

fn validate_direct_occlusion(config: S3SimulationConfig) -> Result<(), BackendError> {
    match config.direct_occlusion {
        DirectOcclusionMode::Raycast => Ok(()),
        DirectOcclusionMode::Volumetric {
            radius_m,
            sample_count,
        } => {
            if !radius_m.is_finite() || radius_m <= 0.0 {
                return Err(BackendError::InvalidInput(
                    "volumetric direct occlusion radius must be finite and positive",
                ));
            }
            if sample_count <= 0 {
                return Err(BackendError::InvalidInput(
                    "volumetric direct occlusion sample count must be positive",
                ));
            }
            if sample_count > config.max_occlusion_samples {
                return Err(BackendError::InvalidInput(
                    "volumetric direct occlusion samples must not exceed simulator capacity",
                ));
            }
            Ok(())
        }
    }
}

fn validate_reflection_effect_config(config: S3SimulationConfig) -> Result<(), BackendError> {
    let effect = config.reflection_effect;
    match effect.effect_type {
        ReflectionEffectType::Convolution | ReflectionEffectType::Parametric => {
            if effect.hybrid_transition_time_s.is_some() || effect.hybrid_overlap_percent.is_some()
            {
                return Err(BackendError::InvalidInput(
                    "hybrid transition settings are inapplicable to convolution or parametric reflections",
                ));
            }
        }
        ReflectionEffectType::Hybrid => {
            let transition = effect
                .hybrid_transition_time_s
                .ok_or(BackendError::InvalidInput(
                    "hybrid reflections require a transition time",
                ))?;
            let overlap = effect
                .hybrid_overlap_percent
                .ok_or(BackendError::InvalidInput(
                    "hybrid reflections require an overlap percent",
                ))?;
            if !transition.is_finite()
                || transition <= 0.0
                || transition > config.reflection_duration_s
            {
                return Err(BackendError::InvalidInput(
                    "hybrid transition time must be finite, positive, and no greater than reflection duration",
                ));
            }
            if !overlap.is_finite() || !(0.0..1.0).contains(&overlap) {
                return Err(BackendError::InvalidInput(
                    "hybrid overlap percent must be finite, non-negative, and less than one",
                ));
            }
        }
        ReflectionEffectType::TrueAudioNext => {
            effect.effect_type.steam_audio_cpu_discriminant()?;
        }
    }
    Ok(())
}

fn validate_mesh(mesh: &SceneMesh) -> Result<(), BackendError> {
    if mesh.vertices_enu_m.is_empty() || mesh.triangles.is_empty() || mesh.materials.is_empty() {
        return Err(BackendError::InvalidInput(
            "scene mesh must contain vertices, triangles, and materials",
        ));
    }
    if mesh.material_indices.len() != mesh.triangles.len() {
        return Err(BackendError::InvalidInput(
            "one material index is required per triangle",
        ));
    }
    if !mesh
        .vertices_enu_m
        .iter()
        .copied()
        .all(EnuVector3::is_finite)
    {
        return Err(BackendError::InvalidInput("scene vertices must be finite"));
    }
    let vertex_count = mesh.vertices_enu_m.len();
    if mesh.triangles.iter().flatten().any(|&index| {
        index < 0 || usize::try_from(index).map_or(true, |index| index >= vertex_count)
    }) {
        return Err(BackendError::InvalidInput(
            "triangle vertex index is out of range",
        ));
    }
    let material_count = mesh.materials.len();
    if mesh.material_indices.iter().any(|&index| {
        index < 0 || usize::try_from(index).map_or(true, |index| index >= material_count)
    }) {
        return Err(BackendError::InvalidInput(
            "triangle material index is out of range",
        ));
    }
    if mesh.materials.iter().any(|material| {
        !material.scattering.is_finite()
            || !(0.0..=1.0).contains(&material.scattering)
            || material
                .absorption
                .into_iter()
                .chain(material.transmission)
                .any(|value| !value.is_finite() || !(0.0..=1.0).contains(&value))
    }) {
        return Err(BackendError::InvalidInput(
            "material coefficients must be finite values between zero and one",
        ));
    }
    checked_i32(mesh.vertices_enu_m.len(), "mesh has too many vertices")?;
    checked_i32(mesh.triangles.len(), "mesh has too many triangles")?;
    checked_i32(mesh.materials.len(), "mesh has too many materials")?;
    Ok(())
}

fn validate_probe_volume(volume: ProbeVolume) -> Result<(), BackendError> {
    if !volume.min_enu_m.is_finite() || !volume.max_enu_m.is_finite() {
        return Err(BackendError::InvalidInput(
            "probe-volume bounds must be finite",
        ));
    }
    if volume.min_enu_m.x >= volume.max_enu_m.x
        || volume.min_enu_m.y >= volume.max_enu_m.y
        || volume.min_enu_m.z >= volume.max_enu_m.z
    {
        return Err(BackendError::InvalidInput(
            "probe-volume minimum must be below maximum on every axis",
        ));
    }
    if !volume.spacing_m.is_finite() || volume.spacing_m <= 0.0 {
        return Err(BackendError::InvalidInput(
            "probe spacing must be finite and positive",
        ));
    }
    if !volume.height_above_floor_m.is_finite() || volume.height_above_floor_m <= 0.0 {
        return Err(BackendError::InvalidInput(
            "probe height must be finite and positive",
        ));
    }
    Ok(())
}

fn validate_elevated_probe_layer(layer: ElevatedProbeLayer) -> Result<(), BackendError> {
    if !layer.height_enu_m.is_finite() {
        return Err(BackendError::InvalidInput(
            "elevated probe layer height must be finite",
        ));
    }
    if !layer.spacing_m.is_finite() || layer.spacing_m <= 0.0 {
        return Err(BackendError::InvalidInput(
            "elevated probe layer spacing must be finite and positive",
        ));
    }
    Ok(())
}

fn checked_i32(value: usize, message: &'static str) -> Result<i32, BackendError> {
    i32::try_from(value).map_err(|_| BackendError::InvalidInput(message))
}

fn validate_audio(config: AudioConfig) -> Result<(), BackendError> {
    if !(8_000..=384_000).contains(&config.sample_rate_hz) {
        return Err(BackendError::InvalidInput(
            "sample rate must be between 8 kHz and 384 kHz",
        ));
    }
    if !(1..=16_384).contains(&config.frame_size) {
        return Err(BackendError::InvalidInput(
            "frame size must be between 1 and 16384 samples",
        ));
    }
    Ok(())
}

fn validate_trajectory_request(request: &S3TrajectoryRenderRequest) -> Result<(), BackendError> {
    if request.listener_trajectory.len() < 2 {
        return Err(BackendError::InvalidInput(
            "S3 trajectory must contain at least two listener poses",
        ));
    }
    let block_frames = usize::try_from(request.base.audio.frame_size)
        .map_err(|_| BackendError::InvalidInput("frame size must be positive"))?;
    let expected_frames = request
        .listener_trajectory
        .len()
        .checked_mul(block_frames)
        .ok_or(BackendError::InvalidInput(
            "S3 trajectory frame count overflows",
        ))?;
    if request.base.input_mono.len() != expected_frames {
        return Err(BackendError::InvalidInput(
            "S3 trajectory input must contain exactly one audio block per listener pose",
        ));
    }
    if request.base.listener != request.listener_trajectory[0] {
        return Err(BackendError::InvalidInput(
            "S3 trajectory base listener must equal the first trajectory pose",
        ));
    }
    for listener in &request.listener_trajectory {
        validate_listener(*listener)?;
        if request.base.source_position_enu == listener.position_enu {
            return Err(BackendError::InvalidInput(
                "S3 source and trajectory listener positions must differ",
            ));
        }
    }
    Ok(())
}

fn validate_position(position: EnuVector3) -> Result<(), BackendError> {
    if !position.is_finite() {
        return Err(BackendError::InvalidInput(
            "positions and directions must be finite",
        ));
    }
    Ok(())
}

fn validate_listener(listener: ListenerPose) -> Result<(), BackendError> {
    validate_position(listener.position_enu)?;
    validate_position(listener.ahead_enu)?;
    validate_position(listener.up_enu)?;
    let ahead = normalized(listener.ahead_enu)?;
    let up = normalized(listener.up_enu)?;
    if dot(ahead, up).abs() > 1.0e-3 {
        return Err(BackendError::InvalidInput(
            "listener ahead and up vectors must be orthogonal",
        ));
    }
    Ok(())
}

fn validate_signal(signal: &[f32], gain: f32) -> Result<(), BackendError> {
    if signal.is_empty() {
        return Err(BackendError::InvalidInput(
            "input PCM must contain at least one sample",
        ));
    }
    if !gain.is_finite() || gain < 0.0 {
        return Err(BackendError::InvalidInput(
            "calibration gain must be finite and non-negative",
        ));
    }
    if !signal.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::InvalidInput(
            "input PCM must contain only finite samples",
        ));
    }
    Ok(())
}

fn relative_direction(
    source: EnuVector3,
    listener: ListenerPose,
) -> Result<SteamVector3, BackendError> {
    let ahead = normalized(listener.ahead_enu)?;
    let up = normalized(listener.up_enu)?;
    let right = normalized(cross(ahead, up))?;
    let difference = normalized(subtract(source, listener.position_enu))?;
    Ok(SteamVector3::new(
        dot(difference, right),
        dot(difference, up),
        -dot(difference, ahead),
    ))
}

fn normalized(vector: EnuVector3) -> Result<EnuVector3, BackendError> {
    let length_squared = dot(vector, vector);
    if !length_squared.is_finite() || length_squared <= 1.0e-12 {
        return Err(BackendError::InvalidInput(
            "orientation and relative direction vectors must be nonzero",
        ));
    }
    let inverse_length = length_squared.sqrt().recip();
    Ok(EnuVector3::new(
        vector.x * inverse_length,
        vector.y * inverse_length,
        vector.z * inverse_length,
    ))
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(left.x - right.x, left.y - right.y, left.z - right.z)
}

fn dot(left: EnuVector3, right: EnuVector3) -> f32 {
    left.x * right.x + left.y * right.y + left.z * right.z
}

fn cross(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.y * right.z - left.z * right.y,
        left.z * right.x - left.x * right.z,
        left.x * right.y - left.y * right.x,
    )
}

fn sdk_status(function: &'static str, status: i32) -> Result<(), BackendError> {
    if status == ffi::IPL_STATUS_SUCCESS {
        Ok(())
    } else {
        Err(BackendError::SdkCall { function, status })
    }
}

fn non_null<T>(
    function: &'static str,
    status: i32,
    pointer: *mut T,
) -> Result<NonNull<T>, BackendError> {
    NonNull::new(pointer).ok_or(BackendError::SdkCall { function, status })
}
