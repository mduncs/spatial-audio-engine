//! Retained multi-source implementation of the frozen runtime backend seam.
//!
//! `WorldGeneration` is shared by the bound pair. In this B2 single-generation
//! session that lifetime bound is the retirement mechanism required by
//! invariant 5: the simulator, every owning `IPLSource`, and their reflection
//! IR storage remain alive until both halves (and therefore every possible
//! callback using this generation) have dropped.

use super::*;
use crate::StageOutputGains;
use crate::backend_snapshot::{
    SteamDirectParams, SteamPropagationSnapshot, SteamReflectionParams, SteamSourcePropagation,
    WORLD_GENERATION, api_enu_to_steam, fixed_path_sh, path_coefficient_count,
};
use crate::motion_smoothing::{
    PROPAGATION_SLEW_TIME_SECONDS, SourcePropagationSmoother, maximum_propagation_delay_samples,
    propagation_delay_samples,
};
use fightbox_api::{EnuVector3 as ApiEnuVector3, Pose};
use fightbox_runtime::backend::{
    BackendRenderError, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationError, SimulationUpdate,
};
use fightbox_runtime::{FractionalDelayLine, SnapshotPublication};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy)]
struct SteamPose {
    position: SteamVector3,
    forward: SteamVector3,
    up: SteamVector3,
}

impl SteamPose {
    fn from_api(pose: Pose) -> Option<Self> {
        if !pose.position.is_finite() || !pose.forward.is_finite() || !pose.up.is_finite() {
            return None;
        }
        let forward = normalized_api(pose.forward)?;
        let up = normalized_api(pose.up)?;
        normalized_api(cross_api(forward, up))?;
        Some(Self {
            position: api_enu_to_steam(pose.position),
            forward: api_enu_to_steam(forward),
            up: api_enu_to_steam(up),
        })
    }
}

#[derive(Clone, Copy)]
struct SimulationFrame {
    listener: SteamPose,
    sources: [SteamPose; MAX_ACTIVE_SOURCES],
    active: [bool; MAX_ACTIVE_SOURCES],
}

fn default_api_pose(position: ApiEnuVector3) -> Pose {
    Pose {
        position,
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn normalized_api(vector: ApiEnuVector3) -> Option<ApiEnuVector3> {
    let length_squared = dot_api(vector, vector);
    if !length_squared.is_finite() || length_squared <= 1.0e-12 {
        return None;
    }
    let scale = length_squared.sqrt().recip();
    Some(ApiEnuVector3::new(
        vector.east_m * scale,
        vector.north_m * scale,
        vector.up_m * scale,
    ))
}

fn dot_api(left: ApiEnuVector3, right: ApiEnuVector3) -> f32 {
    left.east_m * right.east_m + left.north_m * right.north_m + left.up_m * right.up_m
}

fn cross_api(left: ApiEnuVector3, right: ApiEnuVector3) -> ApiEnuVector3 {
    ApiEnuVector3::new(
        left.north_m * right.up_m - left.up_m * right.north_m,
        left.up_m * right.east_m - left.east_m * right.up_m,
        left.east_m * right.north_m - left.north_m * right.east_m,
    )
}

fn handle<T>(value: usize) -> *mut T {
    value as *mut T
}

struct WorldGeneration {
    context: usize,
    scene: usize,
    static_mesh: usize,
    probe_batch: usize,
    simulator: usize,
    sources: [usize; MAX_ACTIVE_SOURCES],
    source_count: usize,
    // Steam Audio's serialized-object API accepts caller-owned bytes. Retain
    // them with the loaded generation so pathing never observes reclaimed
    // backing storage.
    _serialized_bytes: Vec<u8>,
}

impl WorldGeneration {
    fn context(&self) -> ffi::IPLContext {
        handle(self.context)
    }

    fn simulator(&self) -> ffi::IPLSimulator {
        handle(self.simulator)
    }

    fn probe_batch(&self) -> ffi::IPLProbeBatch {
        handle(self.probe_batch)
    }

    fn source(&self, index: usize) -> ffi::IPLSource {
        handle(self.sources[index])
    }
}

impl Drop for WorldGeneration {
    fn drop(&mut self) {
        if self.simulator != 0 {
            let simulator = self.simulator();
            for index in 0..self.source_count {
                if self.sources[index] != 0 {
                    let mut source = self.source(index);
                    ffi::source_remove(source, simulator);
                    ffi::source_release(&mut source);
                }
            }
            ffi::simulator_commit(simulator);
            if self.probe_batch != 0 {
                ffi::simulator_remove_probe_batch(simulator, self.probe_batch());
                ffi::simulator_commit(simulator);
            }
            let mut simulator = simulator;
            ffi::simulator_release(&mut simulator);
        }

        if self.static_mesh != 0 {
            let scene = handle(self.scene);
            let mut static_mesh = handle(self.static_mesh);
            ffi::static_mesh_remove(static_mesh, scene);
            ffi::static_mesh_release(&mut static_mesh);
        }
        if self.probe_batch != 0 {
            let mut probe_batch = self.probe_batch();
            ffi::probe_batch_release(&mut probe_batch);
        }
        if self.scene != 0 {
            let mut scene = handle(self.scene);
            ffi::scene_release(&mut scene);
        }
        if self.context != 0 {
            let mut context = self.context();
            ffi::context_release(&mut context);
        }
    }
}

pub(crate) struct MultiSourceSimulation {
    world: Arc<WorldGeneration>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    frame: SimulationFrame,
    valid_update: bool,
    snapshot: SteamPropagationSnapshot,
    publication: fightbox_runtime::SnapshotWriter<SteamPropagationSnapshot>,
    started: Instant,
}

impl MultiSourceSimulation {
    pub(crate) fn update_inputs(&mut self, update: &SimulationUpdate) {
        let Some(listener) = SteamPose::from_api(update.listener.pose) else {
            self.valid_update = false;
            return;
        };
        if !update.listener.linear_velocity_mps.is_finite() {
            self.valid_update = false;
            return;
        }
        let mut sources = self.frame.sources;
        let mut active = [false; MAX_ACTIVE_SOURCES];
        for index in 0..self.world.source_count {
            let motion = update.sources[index];
            if !motion.linear_velocity_mps.is_finite() {
                self.valid_update = false;
                return;
            }
            let Some(pose) = SteamPose::from_api(motion.pose) else {
                self.valid_update = false;
                return;
            };
            sources[index] = pose;
            active[index] = motion.active;
        }
        self.frame = SimulationFrame {
            listener,
            sources,
            active,
        };
        self.valid_update = true;
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.run_pass(ffi::IPL_SIMULATIONFLAGS_DIRECT)
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.run_pass(ffi::IPL_SIMULATIONFLAGS_PATHING)
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        self.run_pass(ffi::IPL_SIMULATIONFLAGS_REFLECTIONS)
    }

    fn run_pass(&mut self, flag: i32) -> Result<(), SimulationError> {
        if !self.valid_update {
            return Err(SimulationError::InvalidUpdate);
        }
        // Steam Audio documents direct inputs as independently writable from
        // the indirect worker. Pathing and reflections share that indirect
        // input lane, even though their blocking run calls remain separate.
        let input_flags = if flag == ffi::IPL_SIMULATIONFLAGS_DIRECT {
            ffi::IPL_SIMULATIONFLAGS_DIRECT
        } else {
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS | ffi::IPL_SIMULATIONFLAGS_PATHING
        };
        let mut shared = shared_inputs(self.frame.listener, self.config)
            .ok_or(SimulationError::InvalidUpdate)?;
        ffi::simulator_set_shared_inputs(self.world.simulator(), input_flags, &mut shared);
        for index in 0..self.world.source_count {
            let mut inputs = source_inputs(
                self.frame.sources[index],
                self.world.probe_batch(),
                self.config,
                input_flags,
            )
            .ok_or(SimulationError::InvalidUpdate)?;
            ffi::source_set_inputs(self.world.source(index), input_flags, &mut inputs);
        }

        match flag {
            ffi::IPL_SIMULATIONFLAGS_DIRECT => {
                ffi::simulator_run_direct(self.world.simulator());
            }
            ffi::IPL_SIMULATIONFLAGS_PATHING => {
                ffi::simulator_run_pathing(self.world.simulator());
            }
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => {
                ffi::simulator_run_reflections(self.world.simulator());
            }
            _ => return Err(SimulationError::KernelFailure),
        }
        self.copy_and_publish(flag)
    }

    fn copy_and_publish(&mut self, flag: i32) -> Result<(), SimulationError> {
        self.snapshot.sequence = self.snapshot.sequence.wrapping_add(1);
        self.snapshot.simulated_at_ns =
            self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.snapshot.listener_position = self.frame.listener.position;

        for index in 0..self.world.source_count {
            let source_snapshot = &mut self.snapshot.sources[index];
            source_snapshot.active = self.frame.active[index];
            source_snapshot.source_position = self.frame.sources[index].position;
            let mut outputs = ffi::IPLSimulationOutputs::zeroed();
            ffi::source_get_outputs(self.world.source(index), flag, &mut outputs);
            match flag {
                ffi::IPL_SIMULATIONFLAGS_DIRECT => {
                    let direct = SteamDirectParams {
                        distance_attenuation: outputs.direct.distanceAttenuation,
                        air_absorption: outputs.direct.airAbsorption,
                        directivity: outputs.direct.directivity,
                        occlusion: outputs.direct.occlusion,
                        transmission: outputs.direct.transmission,
                    };
                    if !direct_is_finite(direct) {
                        return Err(SimulationError::KernelFailure);
                    }
                    source_snapshot.direct = direct;
                }
                ffi::IPL_SIMULATIONFLAGS_PATHING => {
                    let coefficient_count = path_coefficient_count(self.config.pathing_order)
                        .ok_or(SimulationError::KernelFailure)?;
                    let copied =
                        ffi::copy_path_coefficients(outputs.pathing.shCoeffs, coefficient_count)
                            .ok_or(SimulationError::KernelFailure)?;
                    if !outputs.pathing.eqCoeffs.into_iter().all(f32::is_finite)
                        || !copied.iter().copied().all(f32::is_finite)
                    {
                        return Err(SimulationError::KernelFailure);
                    }
                    source_snapshot.path_eq = outputs.pathing.eqCoeffs;
                    source_snapshot.path_sh = fixed_path_sh(self.config.pathing_order, &copied)
                        .map_err(|_| SimulationError::KernelFailure)?;
                    source_snapshot.configured_pathing_order = self.config.pathing_order as u8;
                }
                ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => {
                    let uses_ir =
                        reflection_effect_uses_ir(self.config.reflection_effect.effect_type);
                    let expected_channels = ambisonics_channel_count(self.config.reflection_order)
                        .map_err(|_| SimulationError::KernelFailure)?;
                    let maximum_ir_size = reflection_ir_size(
                        self.config.reflection_duration_s,
                        self.audio.sample_rate_hz,
                    )
                    .map_err(|_| SimulationError::KernelFailure)?;
                    if uses_ir
                        && (outputs.reflections.ir.is_null()
                            || outputs.reflections.numChannels != expected_channels
                            || outputs.reflections.irSize <= 0
                            || outputs.reflections.irSize > maximum_ir_size)
                    {
                        return Err(SimulationError::KernelFailure);
                    }
                    if !outputs
                        .reflections
                        .reverbTimes
                        .into_iter()
                        .chain(outputs.reflections.eq)
                        .all(f32::is_finite)
                    {
                        return Err(SimulationError::KernelFailure);
                    }
                    source_snapshot.reflections = SteamReflectionParams {
                        ir: outputs.reflections.ir as usize,
                        reverb_times: outputs.reflections.reverbTimes,
                        eq: outputs.reflections.eq,
                        delay: outputs.reflections.delay,
                        num_channels: outputs.reflections.numChannels,
                        ir_size: outputs.reflections.irSize,
                        tan_slot: outputs.reflections.tanSlot,
                    };
                }
                _ => return Err(SimulationError::KernelFailure),
            }
        }
        self.publication.publish(self.snapshot);
        Ok(())
    }
}

fn direct_is_finite(direct: SteamDirectParams) -> bool {
    direct.distance_attenuation.is_finite()
        && direct.air_absorption.into_iter().all(f32::is_finite)
        && direct.directivity.is_finite()
        && direct.occlusion.is_finite()
        && direct.transmission.into_iter().all(f32::is_finite)
}

fn coordinate_space(pose: SteamPose) -> Option<ffi::IPLCoordinateSpace3> {
    let forward = steam_vector_to_api(pose.forward);
    let up = steam_vector_to_api(pose.up);
    let right = normalized_api(cross_api(forward, up))?;
    Some(ffi::IPLCoordinateSpace3 {
        right: raw_steam_vector(api_enu_to_steam(right)),
        up: raw_steam_vector(pose.up),
        ahead: raw_steam_vector(pose.forward),
        origin: raw_steam_vector(pose.position),
    })
}

fn steam_vector_to_api(vector: SteamVector3) -> ApiEnuVector3 {
    let enu = steam_to_enu(vector);
    ApiEnuVector3::new(enu.x, enu.y, enu.z)
}

fn shared_inputs(
    listener: SteamPose,
    config: S3SimulationConfig,
) -> Option<ffi::IPLSimulationSharedInputs> {
    Some(ffi::IPLSimulationSharedInputs {
        listener: coordinate_space(listener)?,
        numRays: config.reflection_rays,
        numBounces: config.reflection_bounces,
        duration: config.reflection_duration_s,
        order: config.reflection_order,
        irradianceMinDistance: 1.0,
        pathingVisCallback: None,
        pathingUserData: core::ptr::null_mut(),
    })
}

fn source_inputs(
    source: SteamPose,
    probe_batch: ffi::IPLProbeBatch,
    config: S3SimulationConfig,
    flag: i32,
) -> Option<ffi::IPLSimulationInputs> {
    Some(ffi::IPLSimulationInputs {
        flags: flag,
        directFlags: ffi::IPL_DIRECTSIMULATIONFLAGS_DISTANCEATTENUATION
            | ffi::IPL_DIRECTSIMULATIONFLAGS_AIRABSORPTION
            | ffi::IPL_DIRECTSIMULATIONFLAGS_DIRECTIVITY
            | ffi::IPL_DIRECTSIMULATIONFLAGS_OCCLUSION
            | ffi::IPL_DIRECTSIMULATIONFLAGS_TRANSMISSION,
        source: coordinate_space(source)?,
        distanceAttenuationModel: default_distance_model(),
        airAbsorptionModel: default_air_absorption_model(),
        directivity: ffi::IPLDirectivity {
            dipoleWeight: 0.0,
            dipolePower: 1.0,
            callback: None,
            userData: core::ptr::null_mut(),
        },
        occlusionType: direct_occlusion_ffi_type(config.direct_occlusion),
        occlusionRadius: match config.direct_occlusion {
            DirectOcclusionMode::Raycast => 0.0,
            DirectOcclusionMode::Volumetric { radius_m, .. } => radius_m,
        },
        numOcclusionSamples: match config.direct_occlusion {
            DirectOcclusionMode::Raycast => 0,
            DirectOcclusionMode::Volumetric { sample_count, .. } => sample_count,
        },
        reverbScale: [1.0; 3],
        hybridReverbTransitionTime: config
            .reflection_effect
            .hybrid_transition_time_s
            .unwrap_or(0.0),
        hybridReverbOverlapPercent: config
            .reflection_effect
            .hybrid_overlap_percent
            .unwrap_or(0.0),
        baked: ffi::IPL_FALSE,
        bakedDataIdentifier: ffi::IPLBakedDataIdentifier::default(),
        pathingProbes: probe_batch,
        visRadius: config.pathing_visibility_radius_m,
        visThreshold: config.pathing_visibility_threshold,
        visRange: config.pathing_visibility_range_m,
        pathingOrder: config.pathing_order,
        enableValidation: bool_to_ipl(config.validate_paths),
        findAlternatePaths: bool_to_ipl(config.find_alternate_paths),
        numTransmissionRays: 1,
        deviationModel: core::ptr::null_mut(),
    })
}

struct OwnedAudioBuffer {
    context: usize,
    channels: i32,
    samples: i32,
    data: usize,
}

impl OwnedAudioBuffer {
    fn allocate(
        context: ffi::IPLContext,
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
            ffi::audio_buffer_allocate(context, channels, samples, &mut raw),
        )?;
        Ok(Self {
            context: context as usize,
            channels: raw.numChannels,
            samples: raw.numSamples,
            data: raw.data as usize,
        })
    }

    fn raw(&self) -> ffi::IPLAudioBuffer {
        ffi::IPLAudioBuffer {
            numChannels: self.channels,
            numSamples: self.samples,
            data: handle(self.data),
        }
    }

    fn write_mono(&mut self, samples: &mut [f32]) {
        let mut raw = self.raw();
        ffi::audio_buffer_deinterleave(handle(self.context), samples, &mut raw);
    }

    fn read_interleaved(&mut self, output: &mut [f32]) {
        let mut raw = self.raw();
        ffi::audio_buffer_interleave(handle(self.context), &mut raw, output);
    }
}

impl Drop for OwnedAudioBuffer {
    fn drop(&mut self) {
        let mut raw = self.raw();
        ffi::audio_buffer_free(handle(self.context), &mut raw);
    }
}

struct SourceRenderState {
    direct_effect: usize,
    binaural_effect: usize,
    path_effect: usize,
    reflection_effect: usize,
    input: OwnedAudioBuffer,
    direct_mono: OwnedAudioBuffer,
    direct_stereo: OwnedAudioBuffer,
    path_stereo: OwnedAudioBuffer,
    reflection_scratch: OwnedAudioBuffer,
    propagation_smoother: SourcePropagationSmoother,
    propagation_delay: FractionalDelayLine,
    applied_delay_samples: f32,
    delay_initialized: bool,
    rendered_since_reset: bool,
    guard_reactivation_history: bool,
    reactivation_epoch_samples: usize,
}

pub(crate) struct MultiSourceRenderGraph {
    config: S3SimulationConfig,
    audio: AudioConfig,
    hrtf: usize,
    sources: Vec<SourceRenderState>,
    reflection_mixer: usize,
    reflection_mix: OwnedAudioBuffer,
    reflection_stereo: OwnedAudioBuffer,
    ambisonics_decode: usize,
    mono_work: Vec<f32>,
    stereo_work: Vec<f32>,
    publication: fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
    stage_output_gain_writer: Option<fightbox_runtime::SnapshotWriter<StageOutputGains>>,
    stage_output_gains: fightbox_runtime::SnapshotReader<StageOutputGains>,
    propagation_block_retention: f32,
    // Must drop after every SDK effect and audio buffer. Keeping the world
    // last also keeps its context alive when the simulation half dropped first.
    world: Arc<WorldGeneration>,
}

impl MultiSourceRenderGraph {
    pub(crate) fn take_stage_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<StageOutputGains>> {
        self.stage_output_gain_writer.take()
    }

    pub(crate) fn render_block(
        &mut self,
        block: PropagationRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        let frames = self.audio.frame_size as usize;
        if block.output_left.len() != frames || block.output_right.len() != frames {
            return Err(BackendRenderError::InvalidBlockLength);
        }
        for source in block.sources {
            if source.source_index >= self.world.source_count {
                return Err(BackendRenderError::InvalidSourceIndex);
            }
            if source.input_mono.len() != frames {
                return Err(BackendRenderError::InvalidBlockLength);
            }
            if !source.input_mono.iter().copied().all(f32::is_finite) {
                return Err(BackendRenderError::InactiveGraph);
            }
        }
        let listener =
            listener_pose(block.listener_orientation).ok_or(BackendRenderError::InactiveGraph)?;
        let snapshot = self.publication.read();
        if snapshot.world_generation != WORLD_GENERATION {
            return Err(BackendRenderError::InactiveGraph);
        }
        let stage_output_gains = self.stage_output_gains.read();
        for (index, state) in self.sources.iter_mut().enumerate() {
            if !snapshot.sources[index].active {
                state.propagation_smoother.reset();
                if state.rendered_since_reset {
                    state.guard_reactivation_history = true;
                }
                state.delay_initialized = false;
                state.reactivation_epoch_samples = 0;
            }
        }

        for source_block in block.sources {
            let propagation = snapshot.sources[source_block.source_index];
            if !propagation.active {
                continue;
            }
            self.render_source(
                source_block,
                propagation,
                listener,
                snapshot.listener_position,
                block.output_left,
                block.output_right,
                stage_output_gains,
            );
        }
        self.render_reflection_mix(
            listener,
            block.output_left,
            block.output_right,
            stage_output_gains.reflections,
        );
        Ok(())
    }

    fn render_source(
        &mut self,
        source_block: &BackendSourceBlock<'_>,
        propagation: SteamSourcePropagation,
        listener: SteamPose,
        listener_position: SteamVector3,
        output_left: &mut [f32],
        output_right: &mut [f32],
        stage_output_gains: StageOutputGains,
    ) {
        let state = &mut self.sources[source_block.source_index];
        let smoothed = state
            .propagation_smoother
            .advance(
                propagation,
                listener_position,
                self.propagation_block_retention,
            )
            .endpoint();
        let delay_target_samples = propagation_delay_samples(
            smoothed.source_position,
            smoothed.listener_position,
            self.audio.sample_rate_hz,
        );
        if !state.delay_initialized {
            // First observation and reactivation adopt the complete target
            // immediately. This avoids an artificial zero-delay attack ramp;
            // normal motion is linearly traversed over each subsequent block.
            state.applied_delay_samples = delay_target_samples;
            state.delay_initialized = true;
        }
        let delay_step =
            (delay_target_samples - state.applied_delay_samples) / self.audio.frame_size as f32;
        let mut delay_samples = state.applied_delay_samples;
        for (frame, input) in source_block.input_mono.iter().copied().enumerate() {
            if frame + 1 == source_block.input_mono.len() {
                delay_samples = delay_target_samples;
            } else {
                delay_samples += delay_step;
            }
            state
                .propagation_delay
                .set_target_delay_samples(delay_samples);
            let delayed = state.propagation_delay.process_sample(input);
            if state.guard_reactivation_history {
                state.reactivation_epoch_samples =
                    state.reactivation_epoch_samples.saturating_add(1);
                if state.reactivation_epoch_samples as f32
                    <= state.propagation_delay.current_delay_samples().ceil() + 2.0
                {
                    self.mono_work[frame] = 0.0;
                } else {
                    state.guard_reactivation_history = false;
                    self.mono_work[frame] = delayed;
                }
            } else {
                self.mono_work[frame] = delayed;
            }
        }
        state.applied_delay_samples = delay_target_samples;
        state.rendered_since_reset = true;
        state.input.write_mono(&mut self.mono_work);

        let mut input = state.input.raw();
        let mut direct_mono = state.direct_mono.raw();
        let mut direct_stereo = state.direct_stereo.raw();
        // DirectEffect retains the preceding parameter frame and interpolates
        // through this block toward the exact backend endpoint supplied here.
        let mut direct_params = ffi::IPLDirectEffectParams {
            flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYDIRECTIVITY
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYOCCLUSION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYTRANSMISSION,
            transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
            distanceAttenuation: smoothed.direct.distance_attenuation,
            airAbsorption: smoothed.direct.air_absorption,
            directivity: smoothed.direct.directivity,
            occlusion: smoothed.direct.occlusion,
            transmission: smoothed.direct.transmission,
        };
        ffi::direct_effect_apply(
            handle(state.direct_effect),
            &mut direct_params,
            &mut input,
            &mut direct_mono,
        );
        let mut binaural_params = ffi::IPLBinauralEffectParams {
            direction: relative_direction_steam(
                smoothed.source_position,
                smoothed.listener_position,
                listener,
            ),
            interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
            spatialBlend: 1.0,
            hrtf: handle(self.hrtf),
            peakDelays: core::ptr::null_mut(),
        };
        ffi::binaural_effect_apply(
            handle(state.binaural_effect),
            &mut binaural_params,
            &mut direct_mono,
            &mut direct_stereo,
        );
        state.direct_stereo.read_interleaved(&mut self.stereo_work);
        accumulate_stereo(
            &self.stereo_work,
            output_left,
            output_right,
            stage_output_gains.direct,
        );

        // PathEffect likewise retains its EQ/SH parameter frame and
        // interpolates toward these one-pole endpoints within the block.
        let mut path_coefficients = smoothed.path_sh;
        let mut path_params = ffi::IPLPathEffectParams {
            eqCoeffs: smoothed.path_eq,
            shCoeffs: path_coefficients.as_mut_ptr(),
            order: i32::from(propagation.configured_pathing_order),
            binaural: ffi::IPL_TRUE,
            hrtf: handle(self.hrtf),
            listener: coordinate_space(SteamPose {
                position: smoothed.listener_position,
                ..listener
            })
            .expect("validated listener orientation"),
            normalizeEQ: ffi::IPL_FALSE,
        };
        let mut path_stereo = state.path_stereo.raw();
        ffi::path_effect_apply(
            handle(state.path_effect),
            &mut path_params,
            &mut input,
            &mut path_stereo,
        );
        state.path_stereo.read_interleaved(&mut self.stereo_work);
        accumulate_stereo(
            &self.stereo_work,
            output_left,
            output_right,
            stage_output_gains.pathing,
        );

        let reflection = propagation.reflections;
        if reflection.ir != 0
            || reflection_effect_uses_reverb(self.config.reflection_effect.effect_type)
        {
            let mut reflection_params = reflection_effect_params(reflection, self.config);
            let mut scratch = state.reflection_scratch.raw();
            ffi::reflection_effect_apply_to_mixer(
                handle(state.reflection_effect),
                &mut reflection_params,
                &mut input,
                &mut scratch,
                handle(self.reflection_mixer),
            );
        }
    }

    fn render_reflection_mix(
        &mut self,
        listener: SteamPose,
        output_left: &mut [f32],
        output_right: &mut [f32],
        gain: f32,
    ) {
        let mut mixer_params = ffi::IPLReflectionEffectParams {
            type_: reflection_effect_ffi_type(self.config.reflection_effect.effect_type)
                .expect("validated reflection effect"),
            ir: core::ptr::null_mut(),
            reverbTimes: [0.0; 3],
            eq: [1.0; 3],
            delay: 0,
            numChannels: ambisonics_channel_count(self.config.reflection_order)
                .expect("validated reflection order"),
            irSize: reflection_ir_size(
                self.config.reflection_duration_s,
                self.audio.sample_rate_hz,
            )
            .expect("validated reflection duration"),
            tanDevice: core::ptr::null_mut(),
            tanSlot: 0,
        };
        let mut reflection_mix = self.reflection_mix.raw();
        ffi::reflection_mixer_apply(
            handle(self.reflection_mixer),
            &mut mixer_params,
            &mut reflection_mix,
        );
        let mut decode_params = ffi::IPLAmbisonicsDecodeEffectParams {
            order: self.config.reflection_order,
            hrtf: handle(self.hrtf),
            orientation: coordinate_space(listener).expect("validated listener orientation"),
            binaural: ffi::IPL_TRUE,
        };
        let mut reflection_stereo = self.reflection_stereo.raw();
        ffi::ambisonics_decode_effect_apply(
            handle(self.ambisonics_decode),
            &mut decode_params,
            &mut reflection_mix,
            &mut reflection_stereo,
        );
        self.reflection_stereo
            .read_interleaved(&mut self.stereo_work);
        accumulate_stereo(&self.stereo_work, output_left, output_right, gain);
    }
}

fn accumulate_stereo(interleaved: &[f32], left: &mut [f32], right: &mut [f32], gain: f32) {
    if gain == 0.0 {
        return;
    }
    if gain == 1.0 {
        for ((frame, left), right) in interleaved
            .chunks_exact(2)
            .zip(left.iter_mut())
            .zip(right.iter_mut())
        {
            *left += frame[0];
            *right += frame[1];
        }
        return;
    }
    for ((frame, left), right) in interleaved
        .chunks_exact(2)
        .zip(left.iter_mut())
        .zip(right.iter_mut())
    {
        *left += frame[0] * gain;
        *right += frame[1] * gain;
    }
}

fn listener_pose(orientation: ListenerOrientation) -> Option<SteamPose> {
    SteamPose::from_api(Pose {
        position: ApiEnuVector3::default(),
        forward: orientation.forward,
        up: orientation.up,
    })
}

fn relative_direction_steam(
    source: SteamVector3,
    listener_position: SteamVector3,
    listener: SteamPose,
) -> ffi::IPLVector3 {
    let source = steam_vector_to_api(source);
    let origin = steam_vector_to_api(listener_position);
    let difference = normalized_api(ApiEnuVector3::new(
        source.east_m - origin.east_m,
        source.north_m - origin.north_m,
        source.up_m - origin.up_m,
    ))
    .unwrap_or(ApiEnuVector3::new(0.0, 1.0, 0.0));
    let forward = steam_vector_to_api(listener.forward);
    let up = steam_vector_to_api(listener.up);
    let right = normalized_api(cross_api(forward, up)).unwrap_or(ApiEnuVector3::new(1.0, 0.0, 0.0));
    raw_steam_vector(SteamVector3::new(
        dot_api(difference, right),
        dot_api(difference, up),
        -dot_api(difference, forward),
    ))
}

fn reflection_effect_params(
    reflection: SteamReflectionParams,
    config: S3SimulationConfig,
) -> ffi::IPLReflectionEffectParams {
    ffi::IPLReflectionEffectParams {
        type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)
            .expect("validated reflection type"),
        ir: handle(reflection.ir),
        reverbTimes: reflection.reverb_times,
        eq: reflection.eq,
        delay: reflection.delay,
        numChannels: reflection.num_channels,
        irSize: reflection.ir_size,
        tanDevice: core::ptr::null_mut(),
        tanSlot: reflection.tan_slot,
    }
}

impl Drop for MultiSourceRenderGraph {
    fn drop(&mut self) {
        for source in &mut self.sources {
            let mut direct = handle(source.direct_effect);
            ffi::direct_effect_release(&mut direct);
            let mut binaural = handle(source.binaural_effect);
            ffi::binaural_effect_release(&mut binaural);
            let mut path = handle(source.path_effect);
            ffi::path_effect_release(&mut path);
            let mut reflections = handle(source.reflection_effect);
            ffi::reflection_effect_release(&mut reflections);
        }
        let mut decode = handle(self.ambisonics_decode);
        ffi::ambisonics_decode_effect_release(&mut decode);
        let mut mixer = handle(self.reflection_mixer);
        ffi::reflection_mixer_release(&mut mixer);
        let mut hrtf = handle(self.hrtf);
        ffi::hrtf_release(&mut hrtf);
    }
}

pub(crate) fn build_multi_source_session(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
) -> Result<(MultiSourceSimulation, MultiSourceRenderGraph), BackendError> {
    validate_multi_source_config(mesh, baked, audio, config, descriptors)?;
    let world = Arc::new(create_world(mesh, baked, audio, config, descriptors.len())?);
    let mut initial = SteamPropagationSnapshot::default();
    let mut source_poses = [SteamPose {
        position: SteamVector3::default(),
        forward: SteamVector3::new(0.0, 0.0, -1.0),
        up: SteamVector3::new(0.0, 1.0, 0.0),
    }; MAX_ACTIVE_SOURCES];
    let mut active = [false; MAX_ACTIVE_SOURCES];
    for (index, descriptor) in descriptors.iter().enumerate() {
        source_poses[index] =
            SteamPose::from_api(default_api_pose(descriptor.initial_position_enu)).ok_or(
                BackendError::InvalidInput("multi-source descriptor position must be finite"),
            )?;
        active[index] = true;
        initial.sources[index].active = true;
        initial.sources[index].source_position = source_poses[index].position;
        initial.sources[index].configured_pathing_order = config.pathing_order as u8;
    }
    let listener = SteamPose::from_api(default_api_pose(ApiEnuVector3::default()))
        .expect("canonical listener pose is valid");
    let (writer, reader) = SnapshotPublication::new(initial);
    let (stage_output_gain_writer, stage_output_gains) =
        SnapshotPublication::new(StageOutputGains::UNITY);
    let render = create_render_graph(
        Arc::clone(&world),
        audio,
        config,
        reader,
        stage_output_gain_writer,
        stage_output_gains,
    )?;
    let simulation = MultiSourceSimulation {
        world,
        audio,
        config,
        frame: SimulationFrame {
            listener,
            sources: source_poses,
            active,
        },
        valid_update: true,
        snapshot: initial,
        publication: writer,
        started: Instant::now(),
    };
    Ok((simulation, render))
}

fn validate_multi_source_config(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
) -> Result<(), BackendError> {
    validate_audio(audio)?;
    validate_mesh(mesh)?;
    baked.validate()?;
    if descriptors.is_empty() || descriptors.len() > MAX_ACTIVE_SOURCES {
        return Err(BackendError::InvalidInput(
            "multi-source session requires between one and MAX_ACTIVE_SOURCES descriptors",
        ));
    }
    if descriptors
        .iter()
        .any(|descriptor| !descriptor.initial_position_enu.is_finite())
    {
        return Err(BackendError::InvalidInput(
            "multi-source descriptor positions must be finite",
        ));
    }
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
    validate_reflection_effect_config(config)?;
    if path_coefficient_count(config.pathing_order).is_none()
        || !(0..=3).contains(&config.reflection_order)
    {
        return Err(BackendError::InvalidInput(
            "Ambisonic orders must be between zero and three",
        ));
    }
    reflection_ir_size(config.reflection_duration_s, audio.sample_rate_hz)?;
    if !config.pathing_visibility_radius_m.is_finite()
        || config.pathing_visibility_radius_m < 0.0
        || !config.pathing_visibility_threshold.is_finite()
        || !(0.0..=1.0).contains(&config.pathing_visibility_threshold)
        || !config.pathing_visibility_range_m.is_finite()
        || config.pathing_visibility_range_m <= 0.0
    {
        return Err(BackendError::InvalidInput(
            "pathing visibility settings are invalid",
        ));
    }
    Ok(())
}

fn create_world(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    source_count: usize,
) -> Result<WorldGeneration, BackendError> {
    let mut context = core::ptr::null_mut();
    let mut context_settings = ffi::IPLContextSettings::pinned_defaults();
    sdk_status(
        "iplContextCreate",
        ffi::context_create(&mut context_settings, &mut context),
    )?;
    let mut world = WorldGeneration {
        context: context as usize,
        scene: 0,
        static_mesh: 0,
        probe_batch: 0,
        simulator: 0,
        sources: [0; MAX_ACTIVE_SOURCES],
        source_count: 0,
        _serialized_bytes: baked.bytes.clone(),
    };

    let result = (|| {
        let mut scene_settings = ffi::IPLSceneSettings {
            type_: ffi::IPL_SCENETYPE_DEFAULT,
            closestHitCallback: None,
            anyHitCallback: None,
            batchedClosestHitCallback: None,
            batchedAnyHitCallback: None,
            userData: core::ptr::null_mut(),
            embreeDevice: core::ptr::null_mut(),
            radeonRaysDevice: core::ptr::null_mut(),
        };
        let mut scene = core::ptr::null_mut();
        sdk_status(
            "iplSceneCreate",
            ffi::scene_create(context, &mut scene_settings, &mut scene),
        )?;
        world.scene = scene as usize;
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
        let mut mesh_settings = ffi::IPLStaticMeshSettings {
            numVertices: checked_i32(vertices.len(), "mesh has too many vertices")?,
            numTriangles: checked_i32(triangles.len(), "mesh has too many triangles")?,
            numMaterials: checked_i32(materials.len(), "mesh has too many materials")?,
            vertices: vertices.as_mut_ptr(),
            triangles: triangles.as_mut_ptr(),
            materialIndices: material_indices.as_mut_ptr(),
            materials: materials.as_mut_ptr(),
        };
        let mut static_mesh = core::ptr::null_mut();
        sdk_status(
            "iplStaticMeshCreate",
            ffi::static_mesh_create(scene, &mut mesh_settings, &mut static_mesh),
        )?;
        world.static_mesh = static_mesh as usize;
        ffi::static_mesh_add(static_mesh, scene);
        ffi::scene_commit(scene);

        let mut serialized_settings = ffi::IPLSerializedObjectSettings {
            data: world._serialized_bytes.as_mut_ptr(),
            size: world._serialized_bytes.len(),
        };
        let mut serialized = core::ptr::null_mut();
        sdk_status(
            "iplSerializedObjectCreate",
            ffi::serialized_object_create(context, &mut serialized_settings, &mut serialized),
        )?;
        let mut probe_batch = core::ptr::null_mut();
        let load_status = ffi::probe_batch_load(context, serialized, &mut probe_batch);
        ffi::serialized_object_release(&mut serialized);
        sdk_status("iplProbeBatchLoad", load_status)?;
        world.probe_batch = probe_batch as usize;
        // Deserialization restores probes and data layers, but 4.8.1 does not
        // rebuild the query tree until this explicit commit.
        ffi::probe_batch_commit(probe_batch);

        let mut simulator_settings = ffi::IPLSimulationSettings {
            flags: all_simulation_flags(),
            sceneType: ffi::IPL_SCENETYPE_DEFAULT,
            reflectionType: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
            maxNumOcclusionSamples: config.max_occlusion_samples,
            maxNumRays: config.reflection_rays,
            numDiffuseSamples: config.diffuse_samples,
            maxDuration: config.reflection_duration_s,
            maxOrder: config.reflection_order.max(config.pathing_order),
            maxNumSources: MAX_ACTIVE_SOURCES as i32,
            numThreads: config.simulation_threads,
            rayBatchSize: config.ray_batch_size,
            numVisSamples: config.pathing_visibility_samples,
            samplingRate: audio.sample_rate_hz,
            frameSize: audio.frame_size,
            openCLDevice: core::ptr::null_mut(),
            radeonRaysDevice: core::ptr::null_mut(),
            tanDevice: core::ptr::null_mut(),
        };
        let mut simulator = core::ptr::null_mut();
        sdk_status(
            "iplSimulatorCreate",
            ffi::simulator_create(context, &mut simulator_settings, &mut simulator),
        )?;
        world.simulator = simulator as usize;
        ffi::simulator_set_scene(simulator, scene);
        ffi::simulator_add_probe_batch(simulator, probe_batch);
        for index in 0..source_count {
            let mut settings = ffi::IPLSourceSettings {
                flags: all_simulation_flags(),
            };
            let mut source = core::ptr::null_mut();
            sdk_status(
                "iplSourceCreate",
                ffi::source_create(simulator, &mut settings, &mut source),
            )?;
            ffi::source_add(source, simulator);
            world.sources[index] = source as usize;
            world.source_count = index + 1;
        }
        ffi::simulator_commit(simulator);
        Ok(())
    })();
    result?;
    Ok(world)
}

fn create_render_graph(
    world: Arc<WorldGeneration>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    mut publication: fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
    stage_output_gain_writer: fightbox_runtime::SnapshotWriter<StageOutputGains>,
    stage_output_gains: fightbox_runtime::SnapshotReader<StageOutputGains>,
) -> Result<MultiSourceRenderGraph, BackendError> {
    let context = world.context();
    let mut audio_settings = raw_audio_settings(audio);
    let mut hrtf_settings = ffi::IPLHRTFSettings {
        type_: ffi::IPL_HRTFTYPE_DEFAULT,
        sofaFileName: core::ptr::null(),
        sofaData: core::ptr::null(),
        sofaDataSize: 0,
        volume: 1.0,
        normType: ffi::IPL_HRTFNORMTYPE_NONE,
    };
    let mut hrtf = core::ptr::null_mut();
    sdk_status(
        "iplHRTFCreate",
        ffi::hrtf_create(context, &mut audio_settings, &mut hrtf_settings, &mut hrtf),
    )?;
    let channels = ambisonics_channel_count(config.reflection_order)?;
    let ir_size = reflection_ir_size(config.reflection_duration_s, audio.sample_rate_hz)?;
    let mut reflection_settings = ffi::IPLReflectionEffectSettings {
        type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
        irSize: ir_size,
        numChannels: channels,
    };
    let mut mixer = core::ptr::null_mut();
    sdk_status(
        "iplReflectionMixerCreate",
        ffi::reflection_mixer_create(
            context,
            &mut audio_settings,
            &mut reflection_settings,
            &mut mixer,
        ),
    )?;
    let mut decode_settings = ffi::IPLAmbisonicsDecodeEffectSettings {
        speakerLayout: ffi::IPLSpeakerLayout {
            type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
            numSpeakers: 0,
            speakers: core::ptr::null_mut(),
        },
        hrtf,
        maxOrder: config.reflection_order,
    };
    let mut decode = core::ptr::null_mut();
    sdk_status(
        "iplAmbisonicsDecodeEffectCreate",
        ffi::ambisonics_decode_effect_create(
            context,
            &mut audio_settings,
            &mut decode_settings,
            &mut decode,
        ),
    )?;

    let initial_snapshot = publication.read();
    let maximum_delay_samples = maximum_propagation_delay_samples(audio.sample_rate_hz);
    let mut source_states = Vec::with_capacity(world.source_count);
    for index in 0..world.source_count {
        let initial_delay_samples = propagation_delay_samples(
            initial_snapshot.sources[index].source_position,
            initial_snapshot.listener_position,
            audio.sample_rate_hz,
        );
        source_states.push(create_source_render_state(
            context,
            &mut audio_settings,
            hrtf,
            config,
            ir_size,
            channels,
            maximum_delay_samples,
            initial_delay_samples,
        )?);
    }
    Ok(MultiSourceRenderGraph {
        world,
        config,
        audio,
        hrtf: hrtf as usize,
        sources: source_states,
        reflection_mixer: mixer as usize,
        reflection_mix: OwnedAudioBuffer::allocate(context, channels, audio.frame_size)?,
        reflection_stereo: OwnedAudioBuffer::allocate(context, 2, audio.frame_size)?,
        ambisonics_decode: decode as usize,
        mono_work: vec![0.0; audio.frame_size as usize],
        stereo_work: vec![0.0; audio.frame_size as usize * 2],
        publication,
        stage_output_gain_writer: Some(stage_output_gain_writer),
        stage_output_gains,
        propagation_block_retention: (-(audio.frame_size as f32 / audio.sample_rate_hz as f32)
            / PROPAGATION_SLEW_TIME_SECONDS)
            .exp(),
    })
}

fn create_source_render_state(
    context: ffi::IPLContext,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: ffi::IPLHRTF,
    config: S3SimulationConfig,
    ir_size: i32,
    channels: i32,
    maximum_delay_samples: usize,
    initial_delay_samples: f32,
) -> Result<SourceRenderState, BackendError> {
    let mut direct_settings = ffi::IPLDirectEffectSettings { numChannels: 1 };
    let mut direct = core::ptr::null_mut();
    sdk_status(
        "iplDirectEffectCreate",
        ffi::direct_effect_create(context, audio_settings, &mut direct_settings, &mut direct),
    )?;
    let mut binaural_settings = ffi::IPLBinauralEffectSettings { hrtf };
    let mut binaural = core::ptr::null_mut();
    sdk_status(
        "iplBinauralEffectCreate",
        ffi::binaural_effect_create(
            context,
            audio_settings,
            &mut binaural_settings,
            &mut binaural,
        ),
    )?;
    let mut path_settings = ffi::IPLPathEffectSettings {
        maxOrder: config.pathing_order,
        spatialize: ffi::IPL_TRUE,
        speakerLayout: ffi::IPLSpeakerLayout {
            type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
            numSpeakers: 0,
            speakers: core::ptr::null_mut(),
        },
        hrtf,
    };
    let mut path = core::ptr::null_mut();
    sdk_status(
        "iplPathEffectCreate",
        ffi::path_effect_create(context, audio_settings, &mut path_settings, &mut path),
    )?;
    let mut reflection_settings = ffi::IPLReflectionEffectSettings {
        type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
        irSize: ir_size,
        numChannels: channels,
    };
    let mut reflection = core::ptr::null_mut();
    sdk_status(
        "iplReflectionEffectCreate",
        ffi::reflection_effect_create(
            context,
            audio_settings,
            &mut reflection_settings,
            &mut reflection,
        ),
    )?;
    Ok(SourceRenderState {
        direct_effect: direct as usize,
        binaural_effect: binaural as usize,
        path_effect: path as usize,
        reflection_effect: reflection as usize,
        input: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        direct_mono: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        direct_stereo: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
        path_stereo: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
        reflection_scratch: OwnedAudioBuffer::allocate(
            context,
            channels,
            audio_settings.frameSize,
        )?,
        propagation_smoother: SourcePropagationSmoother::default(),
        // The 2,048 m physical cap is converted at the graph's sample rate.
        // Setting the per-sample bound to the full capacity lets the backend
        // prescribe an exact intra-block ramp without reconstructing the line.
        propagation_delay: FractionalDelayLine::new(
            maximum_delay_samples,
            initial_delay_samples,
            maximum_delay_samples as f32,
        ),
        applied_delay_samples: initial_delay_samples,
        delay_initialized: false,
        rendered_since_reset: false,
        guard_reactivation_history: false,
        reactivation_epoch_samples: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AcousticMaterial;
    use fightbox_runtime::backend::{BackendSourceBlock, SourceMotion};
    use std::f32::consts::TAU;

    fn test_config() -> S3SimulationConfig {
        S3SimulationConfig {
            reflection_rays: 64,
            diffuse_samples: 8,
            reflection_bounces: 1,
            reflection_duration_s: 0.05,
            reflection_order: 1,
            pathing_order: 1,
            ..S3SimulationConfig::default()
        }
    }

    fn update(first_active: bool, second_active: bool) -> SimulationUpdate {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        sources[0] = SourceMotion {
            active: first_active,
            pose: default_api_pose(ApiEnuVector3::new(2.0, 3.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        sources[1] = SourceMotion {
            active: second_active,
            pose: default_api_pose(ApiEnuVector3::new(5.0, 2.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(ApiEnuVector3::new(4.0, 6.0, 1.5)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        }
    }

    fn one_source_update(
        active: bool,
        source_position: ApiEnuVector3,
        listener_position: ApiEnuVector3,
    ) -> SimulationUpdate {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        sources[0] = SourceMotion {
            active,
            pose: default_api_pose(source_position),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(listener_position),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        }
    }

    fn render_one_source_block(
        render: &mut MultiSourceRenderGraph,
        input: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let source = [BackendSourceBlock {
            source_index: 0,
            input_mono: input,
        }];
        let mut left = vec![0.0; input.len()];
        let mut right = vec![0.0; input.len()];
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &source,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        (left, right)
    }

    fn wall_mesh(wall: AcousticMaterial) -> SceneMesh {
        SceneMesh {
            vertices_enu_m: vec![
                EnuVector3::new(-5.0, 0.0, 0.0),
                EnuVector3::new(5.0, 0.0, 0.0),
                EnuVector3::new(5.0, 0.0, 5.0),
                EnuVector3::new(-5.0, 0.0, 5.0),
                EnuVector3::new(-10.0, -10.0, 0.0),
                EnuVector3::new(10.0, -10.0, 0.0),
                EnuVector3::new(10.0, 10.0, 0.0),
                EnuVector3::new(-10.0, 10.0, 0.0),
            ],
            triangles: vec![
                [0, 1, 2],
                [0, 2, 3],
                [2, 1, 0],
                [3, 2, 0],
                [4, 5, 6],
                [4, 6, 7],
                [6, 5, 4],
                [7, 6, 4],
            ],
            material_indices: vec![0, 0, 0, 0, 1, 1, 1, 1],
            materials: vec![wall, AcousticMaterial::GROUND],
        }
    }

    fn impulse_onset_at_distance(
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
        distance_meters: f32,
    ) -> usize {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptor = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            distance_meters,
            0.0,
            0.0,
        ))];
        let (_simulation, mut render) =
            build_multi_source_session(mesh, baked, audio, test_config(), &descriptor).unwrap();
        let mut dry = Vec::with_capacity(6_144);
        for block in 0..48 {
            let mut input = vec![0.0; audio.frame_size as usize];
            if block == 0 {
                input[0] = 1.0;
            }
            render_one_source_block(&mut render, &input);
            dry.extend_from_slice(&render.mono_work);
        }
        dry.iter()
            .position(|sample| sample.abs() > 1.0e-7)
            .expect("delayed impulse should emerge within the captured window")
    }

    fn doppler_capture(
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
        initial_distance_meters: f32,
        radial_speed_mps: f32,
    ) -> Vec<f32> {
        const WARMUP_BLOCKS: usize = 80;
        const MOTION_LEAD_BLOCKS: usize = 48;
        const CAPTURE_BLOCKS: usize = 96;
        const TONE_HZ: f32 = 1_000.0;
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptor = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            initial_distance_meters,
            0.0,
            0.0,
        ))];
        let (mut simulation, mut render) =
            build_multi_source_session(mesh, baked, audio, test_config(), &descriptor).unwrap();
        let mut captured = Vec::with_capacity(CAPTURE_BLOCKS * audio.frame_size as usize);
        let mut global_frame = 0_usize;

        for block in 0..(WARMUP_BLOCKS + MOTION_LEAD_BLOCKS + CAPTURE_BLOCKS) {
            let motion_block = block.saturating_sub(WARMUP_BLOCKS);
            let elapsed =
                motion_block as f32 * audio.frame_size as f32 / audio.sample_rate_hz as f32;
            let distance = initial_distance_meters + radial_speed_mps * elapsed;
            let mut snapshot = simulation.snapshot;
            snapshot.sequence = snapshot.sequence.wrapping_add(1);
            snapshot.sources[0].source_position = SteamVector3::new(distance, 0.0, 0.0);
            simulation.publication.publish(snapshot);

            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample =
                        (TAU * TONE_HZ * global_frame as f32 / audio.sample_rate_hz as f32).sin();
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            render_one_source_block(&mut render, &input);
            if block >= WARMUP_BLOCKS + MOTION_LEAD_BLOCKS {
                captured.extend_from_slice(&render.mono_work);
            }
        }
        captured
    }

    fn dominant_bin(samples: &[f32], sample_rate_hz: f32, low_hz: f32, high_hz: f32) -> usize {
        let first = (low_hz * samples.len() as f32 / sample_rate_hz).floor() as usize;
        let last = (high_hz * samples.len() as f32 / sample_rate_hz).ceil() as usize;
        (first..=last)
            .max_by(|left, right| {
                let power = |bin: usize| {
                    let radians_per_sample = TAU * bin as f32 / samples.len() as f32;
                    let (real, imaginary) = samples.iter().copied().enumerate().fold(
                        (0.0_f64, 0.0_f64),
                        |(real, imaginary), (frame, sample)| {
                            let phase = radians_per_sample * frame as f32;
                            (
                                real + f64::from(sample * phase.cos()),
                                imaginary - f64::from(sample * phase.sin()),
                            )
                        },
                    );
                    real * real + imaginary * imaginary
                };
                power(*left).total_cmp(&power(*right))
            })
            .unwrap()
    }

    fn transmission_wall_rms(
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
    ) -> (f32, SteamDirectParams) {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_position = ApiEnuVector3::new(0.0, 2.0, 1.5);
        let listener_position = ApiEnuVector3::new(0.0, -2.0, 1.5);
        let descriptor = [crate::MultiSourceDescriptor::at(source_position)];
        let (mut simulation, mut render) =
            build_multi_source_session(mesh, baked, audio, test_config(), &descriptor).unwrap();
        simulation.update_inputs(&one_source_update(true, source_position, listener_position));
        simulation.run_direct().unwrap();
        let direct = simulation.snapshot.sources[0].direct;
        let mut energy = 0.0_f64;
        let mut measured = 0_usize;
        let mut global_frame = 0_usize;
        let mut direct_interleaved = vec![0.0; audio.frame_size as usize * 2];
        for block in 0..20 {
            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = (TAU * 800.0 * global_frame as f32 / audio.sample_rate_hz as f32)
                        .sin()
                        * 0.25;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            render_one_source_block(&mut render, &input);
            if block >= 10 {
                render.sources[0]
                    .direct_stereo
                    .read_interleaved(&mut direct_interleaved);
                for sample in direct_interleaved.iter().copied() {
                    assert!(
                        sample.is_finite(),
                        "non-finite transmission output: {direct:?}"
                    );
                    energy += f64::from(sample * sample);
                    measured += 1;
                }
            }
        }
        ((energy / measured as f64).sqrt() as f32, direct)
    }

    #[test]
    fn linked_distance_delay_places_far_impulse_at_its_physical_onset() {
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();

        let near_onset = impulse_onset_at_distance(&mesh, &baked, 1.0);
        let far_onset = impulse_onset_at_distance(&mesh, &baked, 34.3);

        assert!(
            (4_798..=4_802).contains(&far_onset),
            "far onset was {far_onset}"
        );
        let relative_latency = far_onset - near_onset;
        assert!(
            (4_657..=4_663).contains(&relative_latency),
            "far-vs-near latency was {relative_latency} samples"
        );
    }

    #[test]
    fn linked_delay_slope_shifts_away_tone_below_approaching_tone() {
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let away = doppler_capture(&mesh, &baked, 20.0, 30.0);
        let away_repeated = doppler_capture(&mesh, &baked, 20.0, 30.0);
        let approaching = doppler_capture(&mesh, &baked, 50.0, -30.0);
        assert_eq!(
            away.iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>(),
            away_repeated
                .iter()
                .map(|sample| sample.to_bits())
                .collect::<Vec<_>>(),
            "identical delay trajectories must be byte-identical"
        );
        let away_bin = dominant_bin(&away, 48_000.0, 700.0, 1_300.0);
        let approaching_bin = dominant_bin(&approaching, 48_000.0, 700.0, 1_300.0);

        assert!(
            away_bin + 8 < approaching_bin,
            "away bin {away_bin} was not measurably below approaching bin {approaching_bin}"
        );
    }

    #[test]
    fn linked_reactivation_adopts_new_delay_without_leaking_old_history() {
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptor = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            1.0, 0.0, 0.0,
        ))];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptor).unwrap();
        let ones = vec![1.0; audio.frame_size as usize];
        let zeros = vec![0.0; audio.frame_size as usize];
        for _ in 0..4 {
            render_one_source_block(&mut render, &ones);
        }

        let mut snapshot = simulation.snapshot;
        snapshot.sequence = snapshot.sequence.wrapping_add(1);
        snapshot.sources[0].active = false;
        simulation.publication.publish(snapshot);
        render_one_source_block(&mut render, &zeros);

        snapshot.sequence = snapshot.sequence.wrapping_add(1);
        snapshot.sources[0].active = true;
        snapshot.sources[0].source_position = SteamVector3::new(2.0, 0.0, 0.0);
        simulation.publication.publish(snapshot);
        render_one_source_block(&mut render, &zeros);

        let expected_delay = 2.0 * 48_000.0 / 343.0;
        assert!(
            (render.sources[0].propagation_delay.current_delay_samples() - expected_delay).abs()
                < 0.001
        );
        assert!(
            render.mono_work.iter().all(|sample| sample.to_bits() == 0),
            "reactivation leaked pre-deactivation delay history"
        );
    }

    #[test]
    fn linked_glass_wall_transmits_where_concrete_wall_is_near_silent() {
        let concrete_mesh = wall_mesh(AcousticMaterial::MASONRY);
        let baked = bake_s3(&S3BakeRequest {
            mesh: concrete_mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let glass_mesh = wall_mesh(AcousticMaterial {
            absorption: [0.06, 0.03, 0.02],
            scattering: 0.05,
            transmission: [0.8, 0.7, 0.6],
        });

        let (concrete_rms, concrete_direct) = transmission_wall_rms(&concrete_mesh, &baked);
        let (glass_rms, glass_direct) = transmission_wall_rms(&glass_mesh, &baked);

        assert!(concrete_direct.occlusion < 0.01, "{concrete_direct:?}");
        assert!(glass_direct.occlusion < 0.01, "{glass_direct:?}");
        assert!(
            glass_direct.transmission.iter().any(|band| *band > 0.1),
            "{glass_direct:?}"
        );
        assert!(glass_rms > 1.0e-6, "glass RMS was {glass_rms}");
        assert!(
            concrete_rms < glass_rms * 0.01,
            "concrete RMS {concrete_rms} was not near-silent beside glass RMS {glass_rms}"
        );
    }

    #[test]
    fn linked_two_source_session_renders_isolates_and_drops_in_either_order() {
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let config = test_config();
        let descriptors = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5)),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5)),
        ];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, config, &descriptors).unwrap();
        simulation.update_inputs(&update(true, true));
        simulation.run_direct().unwrap();
        simulation.run_pathing().unwrap();
        simulation.run_reflections().unwrap();
        let source_zero_before = simulation.snapshot.sources[0].direct;

        let input_a = (0..audio.frame_size)
            .map(|sample| (sample as f32 * 0.071).sin() * 0.1)
            .collect::<Vec<_>>();
        let input_b = (0..audio.frame_size)
            .map(|sample| (sample as f32 * 0.113).sin() * 0.08)
            .collect::<Vec<_>>();
        let blocks = [
            BackendSourceBlock {
                source_index: 0,
                input_mono: &input_a,
            },
            BackendSourceBlock {
                source_index: 1,
                input_mono: &input_b,
            },
        ];
        let mut left = vec![0.0; audio.frame_size as usize];
        let mut right = vec![0.0; audio.frame_size as usize];
        for _ in 0..8 {
            left.fill(0.0);
            right.fill(0.0);
            render
                .render_block(PropagationRenderBlock {
                    listener_orientation: ListenerOrientation {
                        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                    },
                    sources: &blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        }
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
        assert!(
            left.iter()
                .chain(&right)
                .any(|sample| sample.abs() > 1.0e-8)
        );
        let first_applied = render.sources[0].propagation_smoother.applied();
        assert_eq!(
            first_applied.direct.distance_attenuation.to_bits(),
            simulation.snapshot.sources[0]
                .direct
                .distance_attenuation
                .to_bits()
        );
        assert_eq!(
            first_applied.direct.occlusion.to_bits(),
            simulation.snapshot.sources[0].direct.occlusion.to_bits()
        );
        assert_eq!(
            first_applied.path_eq.map(f32::to_bits),
            simulation.snapshot.sources[0].path_eq.map(f32::to_bits)
        );
        assert_eq!(
            first_applied.path_sh.map(f32::to_bits),
            simulation.snapshot.sources[0].path_sh.map(f32::to_bits)
        );

        simulation.update_inputs(&update(true, false));
        simulation.run_direct().unwrap();
        assert_eq!(simulation.snapshot.sources[0].direct, source_zero_before);
        assert!(!simulation.snapshot.sources[1].active);
        assert!(simulation.snapshot.sources[0].active);

        let render_isolated = |first_active: bool, second_active: bool| -> (Vec<f32>, Vec<f32>) {
            let (mut simulation, mut render) =
                build_multi_source_session(&mesh, &baked, audio, config, &descriptors).unwrap();
            simulation.update_inputs(&update(first_active, second_active));
            simulation.run_direct().unwrap();
            simulation.run_pathing().unwrap();
            simulation.run_reflections().unwrap();
            let mut isolated_left = vec![0.0; audio.frame_size as usize];
            let mut isolated_right = vec![0.0; audio.frame_size as usize];
            for _ in 0..8 {
                isolated_left.fill(0.0);
                isolated_right.fill(0.0);
                render
                    .render_block(PropagationRenderBlock {
                        listener_orientation: ListenerOrientation {
                            forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                        },
                        sources: &blocks,
                        output_left: &mut isolated_left,
                        output_right: &mut isolated_right,
                    })
                    .unwrap();
            }
            (isolated_left, isolated_right)
        };
        let (only_zero_left, only_zero_right) = render_isolated(true, false);
        let (only_one_left, only_one_right) = render_isolated(false, true);
        for ((both, zero), one) in left
            .iter()
            .zip(&only_zero_left)
            .zip(&only_one_left)
            .chain(right.iter().zip(&only_zero_right).zip(&only_one_right))
        {
            assert!((*both - (*zero + *one)).abs() <= 1.0e-5);
        }

        let previous = render.sources[0].propagation_smoother.applied();
        let mut stepped = simulation.snapshot;
        stepped.sequence = stepped.sequence.wrapping_add(1);
        stepped.sources[0].direct.distance_attenuation = 0.25;
        stepped.sources[0].direct.air_absorption = [0.3, 0.4, 0.5];
        stepped.sources[0].direct.directivity = 0.6;
        stepped.sources[0].direct.occlusion = 0.1;
        stepped.sources[0].direct.transmission = [0.2, 0.3, 0.4];
        stepped.sources[0].path_eq = [0.15, 0.25, 0.35];
        stepped.sources[0].path_sh = std::array::from_fn(|index| 0.01 * (index + 1) as f32);
        simulation.publication.publish(stepped);
        left.fill(0.0);
        right.fill(0.0);
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        let applied = render.sources[0].propagation_smoother.applied();
        let expected =
            |old: f32, target: f32| target + (old - target) * render.propagation_block_retention;
        assert_eq!(
            applied.direct.distance_attenuation.to_bits(),
            expected(
                previous.direct.distance_attenuation,
                stepped.sources[0].direct.distance_attenuation
            )
            .to_bits()
        );
        assert_eq!(
            applied.direct.occlusion.to_bits(),
            expected(
                previous.direct.occlusion,
                stepped.sources[0].direct.occlusion
            )
            .to_bits()
        );
        assert_eq!(
            applied.path_eq[1].to_bits(),
            expected(previous.path_eq[1], stepped.sources[0].path_eq[1]).to_bits()
        );
        assert_eq!(
            applied.path_sh[3].to_bits(),
            expected(previous.path_sh[3], stepped.sources[0].path_sh[3]).to_bits()
        );
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));

        drop(simulation);
        drop(render);

        let (simulation_first, render_second) =
            build_multi_source_session(&mesh, &baked, audio, config, &descriptors).unwrap();
        drop(render_second);
        drop(simulation_first);
    }
}
