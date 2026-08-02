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
    api_enu_to_steam, fixed_path_sh, path_coefficient_count,
};
use crate::governor::{
    GovernorRenderSnapshot, GovernorSimulationPass, QualityGovernor, QualityGovernorTelemetry,
    ReverbStrategy, SourceQualityLevel,
};
use crate::motion_smoothing::{
    PROPAGATION_SLEW_TIME_SECONDS, SourcePropagationSmoother, maximum_propagation_delay_samples,
    propagation_delay_samples,
};
use crate::probe_influence::SerializedProbeInfluences;
use crate::propagation_delay::PropagationDelayLine;
use crate::{MemoryTrackingStatus, QualityTier, SessionMemoryTelemetry};
use fightbox_api::{Directivity, EnuVector3 as ApiEnuVector3, Pose};
use fightbox_runtime::SnapshotPublication;
use fightbox_runtime::backend::{
    BackendRenderError, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationError, SimulationUpdate,
};
use std::mem::size_of;
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
    listener_linear_velocity_mps: SteamVector3,
    sources: [SteamPose; MAX_ACTIVE_SOURCES],
    source_linear_velocities_mps: [SteamVector3; MAX_ACTIVE_SOURCES],
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
    generation: u64,
    has_baked_pathing: bool,
    baked_data_fingerprint: u64,
    context: usize,
    scene: usize,
    static_mesh: usize,
    probe_batch: usize,
    simulator: usize,
    sources: [usize; MAX_ACTIVE_SOURCES],
    source_count: usize,
    probe_influences: Option<SerializedProbeInfluences>,
    // Steam Audio's serialized-object API accepts caller-owned bytes. Retain
    // them with the loaded generation so pathing never observes reclaimed
    // backing storage.
    serialized_bytes: Vec<u8>,
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

    fn has_influencing_probe(&self, position: SteamVector3) -> bool {
        self.probe_influences
            .is_some_and(|probes| probes.contains(&self.serialized_bytes, position))
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
            if self.probe_batch != 0 && self.has_baked_pathing {
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
    source_directivities: [Directivity; MAX_ACTIVE_SOURCES],
    source_occlusion_modes: [DirectOcclusionMode; MAX_ACTIVE_SOURCES],
    frame: SimulationFrame,
    valid_update: bool,
    snapshot: SteamPropagationSnapshot,
    publication: fightbox_runtime::SnapshotWriter<SteamPropagationSnapshot>,
    governor: QualityGovernor,
    reflection_cadence_tick: u64,
    last_pass_started_ns: [Option<u64>; 3],
    last_direct_frame: Option<SimulationFrame>,
    started: Instant,
}

impl MultiSourceSimulation {
    pub(crate) fn audio_config(&self) -> AudioConfig {
        self.audio
    }

    pub(crate) fn source_count(&self) -> usize {
        self.world.source_count
    }

    pub(crate) fn capabilities(&self) -> crate::PreparedWorldCapabilities {
        crate::PreparedWorldCapabilities {
            generation: self.world.generation,
            baked_pathing: self.world.has_baked_pathing,
            reflections: crate::WorldReflectionState::from_effect(
                self.config.reflection_effect.effect_type,
            ),
        }
    }

    pub(crate) fn diagnostics(&self) -> crate::WorldGenerationDiagnostics {
        let source = self.snapshot.sources[0];
        crate::WorldGenerationDiagnostics {
            generation: self.world.generation,
            baked_data_fingerprint: self.world.baked_data_fingerprint,
            path_eq: source.path_eq,
            path_sh_energy: source
                .path_sh
                .into_iter()
                .map(|coefficient| coefficient * coefficient)
                .sum(),
            reflection_reverb_times: source.reflections.reverb_times,
            reflection_ir_size: source.reflections.ir_size,
        }
    }

    pub(crate) fn source_diagnostics(
        &self,
        source_index: usize,
    ) -> Option<crate::SourceAcousticDiagnostics> {
        // The snapshot array is fixed at MAX_ACTIVE_SOURCES, so bound the index
        // by the generation's configured source count. Reading past it would
        // report a default-constructed slot as if it were a real source.
        if source_index >= self.world.source_count {
            return None;
        }
        let source = self.snapshot.sources[source_index];
        Some(crate::SourceAcousticDiagnostics {
            source_index,
            active: source.active,
            distance_attenuation: source.direct.distance_attenuation,
            air_absorption: source.direct.air_absorption,
            directivity: source.direct.directivity,
            occlusion: source.direct.occlusion,
            transmission: source.direct.transmission,
            path_eq: source.path_eq,
            path_sh_energy: source
                .path_sh
                .into_iter()
                .map(|coefficient| coefficient * coefficient)
                .sum(),
            reflection_ir_size: source.reflections.ir_size,
        })
    }

    pub(crate) fn observe_render_timing(&mut self, elapsed_ns: u64) {
        self.governor.observe_block_timing(elapsed_ns);
    }

    pub(crate) fn observe_simulation_lateness(
        &mut self,
        pass: GovernorSimulationPass,
        lateness_ns: u64,
    ) {
        self.governor.observe_simulation_lateness(pass, lateness_ns);
    }

    pub(crate) fn quality_governor_telemetry(&self) -> QualityGovernorTelemetry {
        self.governor.telemetry()
    }

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
        let mut source_linear_velocities_mps = self.frame.source_linear_velocities_mps;
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
            source_linear_velocities_mps[index] = api_enu_to_steam(motion.linear_velocity_mps);
            active[index] = motion.active;
        }
        self.frame = SimulationFrame {
            listener,
            listener_linear_velocity_mps: api_enu_to_steam(update.listener.linear_velocity_mps),
            sources,
            source_linear_velocities_mps,
            active,
        };
        self.valid_update = true;
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.run_pass(
            ffi::IPL_SIMULATIONFLAGS_DIRECT,
            GovernorSimulationPass::Direct,
        )
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        if !self.world.has_baked_pathing {
            return Err(SimulationError::KernelFailure);
        }
        self.run_pass(
            ffi::IPL_SIMULATIONFLAGS_PATHING,
            GovernorSimulationPass::Pathing,
        )
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        let cadence_divisor = u64::from(self.governor.render_quality().reflections.cadence_divisor);
        let tick = self.reflection_cadence_tick;
        self.reflection_cadence_tick = self.reflection_cadence_tick.wrapping_add(1);
        if !tick.is_multiple_of(cadence_divisor) {
            return Ok(());
        }
        self.run_pass(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
        )
    }

    fn run_pass(&mut self, flag: i32, pass: GovernorSimulationPass) -> Result<(), SimulationError> {
        if !self.valid_update {
            return Err(SimulationError::InvalidUpdate);
        }
        let pass_started = Instant::now();
        let pass_started_ns = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let quality = self.governor.render_quality();
        let target_interval_ns = match pass {
            GovernorSimulationPass::Direct => 1_000_000_000 / 60,
            GovernorSimulationPass::Pathing => 1_000_000_000 / 15,
            GovernorSimulationPass::Reflections => {
                (1_000_000_000 / 5) * u64::from(quality.reflections.cadence_divisor)
            }
        };
        let pass_index = pass.index();
        let previous_started_ns = self.last_pass_started_ns[pass_index].replace(pass_started_ns);
        let lateness_ns = previous_started_ns.map_or(0, |previous| {
            pass_started_ns
                .saturating_sub(previous)
                .saturating_sub(target_interval_ns)
        });
        self.governor.observe_simulation_lateness(pass, lateness_ns);
        // Steam Audio documents direct inputs as independently writable from
        // the indirect worker. Pathing and reflections share that indirect
        // input lane, even though their blocking run calls remain separate.
        let input_flags = if flag == ffi::IPL_SIMULATIONFLAGS_DIRECT {
            ffi::IPL_SIMULATIONFLAGS_DIRECT
        } else {
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS | ffi::IPL_SIMULATIONFLAGS_PATHING
        };
        let mut shared =
            shared_inputs(self.frame.listener, quality).ok_or(SimulationError::InvalidUpdate)?;
        ffi::simulator_set_shared_inputs(self.world.simulator(), input_flags, &mut shared);
        for index in 0..self.world.source_count {
            let mut inputs = source_inputs(
                self.frame.sources[index],
                self.source_directivities[index],
                self.source_occlusion_modes[index],
                self.world.probe_batch(),
                self.config,
                quality,
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
        let result = self.copy_and_publish(flag, quality);
        let elapsed_ns = pass_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.governor
            .observe_simulation_lateness(pass, elapsed_ns.saturating_sub(target_interval_ns));
        result
    }

    fn copy_and_publish(
        &mut self,
        flag: i32,
        quality: GovernorRenderSnapshot,
    ) -> Result<(), SimulationError> {
        self.snapshot.sequence = self.snapshot.sequence.wrapping_add(1);
        self.snapshot.simulated_at_ns =
            self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        self.snapshot.listener_position = self.frame.listener.position;
        self.snapshot.listener_linear_velocity_mps = self.frame.listener_linear_velocity_mps;
        let listener_has_probe = flag == ffi::IPL_SIMULATIONFLAGS_PATHING
            && self
                .world
                .has_influencing_probe(self.frame.listener.position);

        for index in 0..self.world.source_count {
            let source_snapshot = &mut self.snapshot.sources[index];
            source_snapshot.active = self.frame.active[index];
            source_snapshot.source_position = self.frame.sources[index].position;
            source_snapshot.linear_velocity_mps = self.frame.source_linear_velocities_mps[index];
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
                    self.governor
                        .observe_source_gain(index, predicted_direct_gain(direct));
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
                    // Steam Audio's path simulator returns without writing its
                    // retained output when an occluded endpoint has no
                    // influencing probe. The public result has no success bit,
                    // so mirror that exact precondition from the serialized
                    // probe spheres and replace the stale target with silence.
                    //
                    // A line-of-sight path is valid without probes. Only trust
                    // raycast direct occlusion for this purpose when it was
                    // simulated at the exact current endpoints. Volumetric
                    // visibility does not expose the center ray used by
                    // pathing, so it cannot prove this bypass.
                    let direct_line_of_sight =
                        matches!(
                            self.source_occlusion_modes[index],
                            DirectOcclusionMode::Raycast
                        ) && self.last_direct_frame.is_some_and(|direct_frame| {
                            same_position(
                                direct_frame.listener.position,
                                self.frame.listener.position,
                            ) && same_position(
                                direct_frame.sources[index].position,
                                self.frame.sources[index].position,
                            ) && source_snapshot.direct.occlusion >= 1.0 - 1.0e-6
                        });
                    let endpoints_have_probes = listener_has_probe
                        && self
                            .world
                            .has_influencing_probe(self.frame.sources[index].position);
                    if direct_line_of_sight || endpoints_have_probes {
                        source_snapshot.path_eq = outputs.pathing.eqCoeffs;
                        source_snapshot.path_sh = fixed_path_sh(self.config.pathing_order, &copied)
                            .map_err(|_| SimulationError::KernelFailure)?;
                    } else {
                        source_snapshot.path_eq = [1.0; 3];
                        source_snapshot.path_sh =
                            [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS];
                    }
                    source_snapshot.configured_pathing_order = self.config.pathing_order as u8;
                }
                ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => {
                    let uses_ir =
                        reflection_effect_uses_ir(self.config.reflection_effect.effect_type);
                    let expected_channels = ambisonics_channel_count(quality.ambisonic_order)
                        .map_err(|_| SimulationError::KernelFailure)?;
                    let maximum_ir_size = reflection_ir_size(
                        quality.reflections.ir_duration_s,
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
        if flag == ffi::IPL_SIMULATIONFLAGS_DIRECT {
            self.last_direct_frame = Some(self.frame);
        }
        self.publication.publish(self.snapshot);
        Ok(())
    }
}

fn same_position(left: SteamVector3, right: SteamVector3) -> bool {
    left.x.to_bits() == right.x.to_bits()
        && left.y.to_bits() == right.y.to_bits()
        && left.z.to_bits() == right.z.to_bits()
}

fn radial_velocity_mps(
    source_position: SteamVector3,
    source_velocity_mps: SteamVector3,
    listener_position: SteamVector3,
    listener_velocity_mps: SteamVector3,
) -> f32 {
    let offset = SteamVector3::new(
        source_position.x - listener_position.x,
        source_position.y - listener_position.y,
        source_position.z - listener_position.z,
    );
    let distance_squared = offset.x * offset.x + offset.y * offset.y + offset.z * offset.z;
    if !distance_squared.is_finite() || distance_squared <= 1.0e-12 {
        return 0.0;
    }
    let inverse_distance = distance_squared.sqrt().recip();
    let relative_velocity = SteamVector3::new(
        source_velocity_mps.x - listener_velocity_mps.x,
        source_velocity_mps.y - listener_velocity_mps.y,
        source_velocity_mps.z - listener_velocity_mps.z,
    );
    let radial = (relative_velocity.x * offset.x
        + relative_velocity.y * offset.y
        + relative_velocity.z * offset.z)
        * inverse_distance;
    if radial.is_finite() { radial } else { 0.0 }
}

fn direct_is_finite(direct: SteamDirectParams) -> bool {
    direct.distance_attenuation.is_finite()
        && direct.air_absorption.into_iter().all(f32::is_finite)
        && direct.directivity.is_finite()
        && direct.occlusion.is_finite()
        && direct.transmission.into_iter().all(f32::is_finite)
}

fn predicted_direct_gain(direct: SteamDirectParams) -> f32 {
    let air = direct.air_absorption.into_iter().sum::<f32>() / 3.0;
    let transmission = direct.transmission.into_iter().sum::<f32>() / 3.0;
    let visibility = direct.occlusion.max(transmission);
    (direct.distance_attenuation * air * direct.directivity * visibility)
        .abs()
        .max(0.0)
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
    quality: GovernorRenderSnapshot,
) -> Option<ffi::IPLSimulationSharedInputs> {
    Some(ffi::IPLSimulationSharedInputs {
        listener: coordinate_space(listener)?,
        numRays: quality.reflections.rays,
        numBounces: quality.reflections.bounces,
        duration: quality.reflections.ir_duration_s,
        order: quality.ambisonic_order,
        irradianceMinDistance: 1.0,
        pathingVisCallback: None,
        pathingUserData: core::ptr::null_mut(),
    })
}

fn source_inputs(
    source: SteamPose,
    directivity: Directivity,
    direct_occlusion: DirectOcclusionMode,
    probe_batch: ffi::IPLProbeBatch,
    config: S3SimulationConfig,
    quality: GovernorRenderSnapshot,
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
            dipoleWeight: directivity.dipole_weight,
            dipolePower: directivity.dipole_power,
            callback: None,
            userData: core::ptr::null_mut(),
        },
        occlusionType: direct_occlusion_ffi_type(direct_occlusion),
        occlusionRadius: match direct_occlusion {
            DirectOcclusionMode::Raycast => 0.0,
            DirectOcclusionMode::Volumetric { radius_m, .. } => radius_m,
        },
        numOcclusionSamples: match direct_occlusion {
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
        enableValidation: bool_to_ipl(quality.validate_paths),
        findAlternatePaths: bool_to_ipl(quality.find_alternate_paths),
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
    propagation_delay: PropagationDelayLine,
    last_propagation_observation: Option<(u64, u32, u32)>,
    rendered_since_reset: bool,
    guard_reactivation_history: bool,
    reactivation_epoch_samples: usize,
    quality_gains: [f32; 3],
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
    governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    applied_governor_quality: GovernorRenderSnapshot,
    reflection_output_gain: f32,
    propagation_block_retention: f32,
    #[cfg(test)]
    governor_snapshot_reads: u64,
    // Must drop after every SDK effect and audio buffer. Keeping the world
    // last also keeps its context alive when the simulation half dropped first.
    world: Arc<WorldGeneration>,
}

impl MultiSourceRenderGraph {
    pub(crate) fn capabilities(&self) -> crate::PreparedWorldCapabilities {
        crate::PreparedWorldCapabilities {
            generation: self.world.generation,
            baked_pathing: self.world.has_baked_pathing,
            reflections: crate::WorldReflectionState::from_effect(
                self.config.reflection_effect.effect_type,
            ),
        }
    }

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
        if snapshot.world_generation != self.world.generation {
            return Err(BackendRenderError::InactiveGraph);
        }
        let stage_output_gains = self.stage_output_gains.read();
        let governor_quality = self.governor_quality.read();
        #[cfg(test)]
        {
            self.governor_snapshot_reads = self.governor_snapshot_reads.saturating_add(1);
        }
        self.applied_governor_quality = governor_quality;
        for (index, state) in self.sources.iter_mut().enumerate() {
            if !snapshot.sources[index].active {
                state.propagation_smoother.reset();
                if state.rendered_since_reset {
                    state.guard_reactivation_history = true;
                }
                state.propagation_delay.invalidate();
                state.last_propagation_observation = None;
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
                snapshot.listener_linear_velocity_mps,
                snapshot.sequence,
                block.output_left,
                block.output_right,
                stage_output_gains,
                governor_quality,
            );
        }
        self.render_reflection_mix(
            listener,
            block.output_left,
            block.output_right,
            stage_output_gains.reflections,
            governor_quality,
        );
        Ok(())
    }

    /// Renders one source's direct, baked-path, and reflection sends.
    ///
    /// # Stage alignment under propagation delay
    ///
    /// The dry stem is delayed once, before Steam Audio sees it, so all three
    /// stages share the same source-distance time of flight. That is exactly
    /// right for the direct stage and an accepted approximation for the other
    /// two, on the following basis.
    ///
    /// Published source and listener velocities refine the delay-head rate
    /// between position snapshots. Their relative velocity is projected onto
    /// the raw source-listener axis, producing the exact reception-time pitch
    /// ratio `1 / (1 + v_radial / 343)`. Raw positions remain the absolute
    /// anchor and the sole teleport signal; any velocity-integrated drift back
    /// to that anchor is corrected through the delay line's one-pole rather
    /// than by stepping a read head. A default zero velocity deliberately
    /// retains the wave-6 position-only behavior and its conditional
    /// `1 - v_radial / 343` approximation.
    ///
    /// *Reflections.* Measured against a standalone `IPLReflectionEffect` fed
    /// an impulse (see `linked_reflection_ir_does_not_encode_source_distance`),
    /// a simulated reflection IR responds within the first block regardless of
    /// how far the source is from the listener: Steam Audio's IR is referenced
    /// to the listener, with the source-to-listener flight time already
    /// removed. Reflections therefore need this delay added, and adding it
    /// keeps them behind the direct arrival rather than ahead of it, which is
    /// the audible ordering that matters. What the shared delay does *not*
    /// model is that each reflected path is longer than the direct path by its
    /// own amount; those differences live inside the IR's own envelope, so the
    /// error is a constant offset of the whole reflected field rather than a
    /// reordering within it.
    ///
    /// *Baked pathing.* A path around a corner is longer than the straight
    /// line, so the true delay exceeds the direct one. Baked paths carry no
    /// per-path length, so the source-distance delay is used as a lower bound.
    /// The consequence is that around-corner energy arrives slightly early;
    /// the alternative, leaving it undelayed, would make it arrive before the
    /// source was audible at all.
    fn render_source(
        &mut self,
        source_block: &BackendSourceBlock<'_>,
        propagation: SteamSourcePropagation,
        listener: SteamPose,
        listener_position: SteamVector3,
        listener_linear_velocity_mps: SteamVector3,
        propagation_sequence: u64,
        output_left: &mut [f32],
        output_right: &mut [f32],
        stage_output_gains: StageOutputGains,
        governor_quality: GovernorRenderSnapshot,
    ) {
        let state = &mut self.sources[source_block.source_index];
        let source_quality = governor_quality.sources[source_block.source_index];
        let listener_centric_reflection = governor_quality.reverb
            != ReverbStrategy::ListenerCentric
            || usize::from(governor_quality.listener_centric_source) == source_block.source_index;
        let mut targets = source_quality_targets(source_quality, listener_centric_reflection);
        if !self.world.has_baked_pathing {
            targets[1] = 0.0;
        }
        let quality_ramps: [GainRamp; 3] = std::array::from_fn(|index| {
            GainRamp::new(
                state.quality_gains[index],
                targets[index],
                self.audio.frame_size as usize,
            )
        });
        state.quality_gains = targets;
        let smoothed = state
            .propagation_smoother
            .advance(
                propagation,
                listener_position,
                self.propagation_block_retention,
            )
            .endpoint();
        // Time of flight is anchored by the simulated endpoints rather than by
        // the 80 ms acoustic smoother above. The smoother exists to keep gain
        // and occlusion from zippering; running the raw delay through it too
        // would erase the distinction between motion and a teleport, which is
        // exactly what the delay line must be able to tell apart. Published
        // velocity supplies the between-snapshot rate, while the delay line
        // corrects integrated drift and ordinary positional motion smoothly.
        let delay_target_samples = propagation_delay_samples(
            propagation.source_position,
            listener_position,
            self.audio.sample_rate_hz,
        );
        let radial_velocity_mps = radial_velocity_mps(
            propagation.source_position,
            propagation.linear_velocity_mps,
            listener_position,
            listener_linear_velocity_mps,
        );
        let observation = (
            propagation_sequence,
            delay_target_samples.to_bits(),
            radial_velocity_mps.to_bits(),
        );
        if state.last_propagation_observation != Some(observation) {
            if radial_velocity_mps == 0.0 {
                state
                    .propagation_delay
                    .observe_block_target(delay_target_samples);
            } else {
                state
                    .propagation_delay
                    .observe_block_target_with_velocity(delay_target_samples, radial_velocity_mps);
            }
            state.last_propagation_observation = Some(observation);
        }
        for (frame, input) in source_block.input_mono.iter().copied().enumerate() {
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
        state.rendered_since_reset = true;
        // Every downstream stage reads `mono_work`, so the direct, baked-path,
        // and reflection sends all inherit this one source-distance delay.
        // See the stage-alignment note on `render_source`.
        state.input.write_mono(&mut self.mono_work);

        let mut input = state.input.raw();
        if quality_ramps[0].is_audible() {
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
            accumulate_stereo_ramped(
                &self.stereo_work,
                output_left,
                output_right,
                stage_output_gains.direct,
                quality_ramps[0],
            );
        }

        // PathEffect likewise retains its EQ/SH parameter frame and
        // interpolates toward these one-pole endpoints within the block.
        if quality_ramps[1].is_audible() {
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
            accumulate_stereo_ramped(
                &self.stereo_work,
                output_left,
                output_right,
                stage_output_gains.pathing,
                quality_ramps[1],
            );
        }

        let reflection = propagation.reflections;
        if quality_ramps[2].is_audible()
            && (reflection.ir != 0
                || reflection_effect_uses_reverb(self.config.reflection_effect.effect_type))
        {
            for (frame, sample) in self.mono_work.iter_mut().enumerate() {
                *sample *= quality_ramps[2].at(frame);
            }
            state.input.write_mono(&mut self.mono_work);
            input = state.input.raw();
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
        governor_quality: GovernorRenderSnapshot,
    ) {
        let mut mixer_params = ffi::IPLReflectionEffectParams {
            type_: reflection_effect_ffi_type(self.config.reflection_effect.effect_type)
                .expect("validated reflection effect"),
            ir: core::ptr::null_mut(),
            reverbTimes: [0.0; 3],
            eq: [1.0; 3],
            delay: 0,
            numChannels: ambisonics_channel_count(governor_quality.ambisonic_order)
                .expect("validated reflection order"),
            irSize: reflection_ir_size(
                governor_quality.reflections.ir_duration_s,
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
            order: governor_quality.ambisonic_order,
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
        let reflection_gain_ramp = GainRamp::new(
            self.reflection_output_gain,
            governor_quality.reflection_output_gain,
            self.audio.frame_size as usize,
        );
        self.reflection_output_gain = governor_quality.reflection_output_gain;
        accumulate_stereo_ramped(
            &self.stereo_work,
            output_left,
            output_right,
            gain,
            reflection_gain_ramp,
        );
    }
}

#[derive(Clone, Copy)]
struct GainRamp {
    start: f32,
    step: f32,
    end: f32,
}

impl GainRamp {
    fn new(start: f32, end: f32, frames: usize) -> Self {
        Self {
            start,
            step: if frames > 1 {
                (end - start) / (frames - 1) as f32
            } else {
                0.0
            },
            end,
        }
    }

    fn at(self, frame: usize) -> f32 {
        if frame == 0 && self.step != 0.0 {
            self.start
        } else if self.step == 0.0 {
            self.end
        } else {
            (self.start + self.step * frame as f32)
                .clamp(self.start.min(self.end), self.start.max(self.end))
        }
    }

    fn is_audible(self) -> bool {
        self.start != 0.0 || self.end != 0.0
    }
}

fn accumulate_stereo_ramped(
    interleaved: &[f32],
    left: &mut [f32],
    right: &mut [f32],
    stage_gain: f32,
    ramp: GainRamp,
) {
    if stage_gain == 0.0 || !ramp.is_audible() {
        return;
    }
    for (frame_index, ((frame, left), right)) in interleaved
        .chunks_exact(2)
        .zip(left.iter_mut())
        .zip(right.iter_mut())
        .enumerate()
    {
        let gain = stage_gain * ramp.at(frame_index);
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

#[cfg(test)]
pub(crate) fn build_multi_source_session(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
) -> Result<(MultiSourceSimulation, MultiSourceRenderGraph), BackendError> {
    build_multi_source_generation(
        mesh,
        Some(baked),
        audio,
        config,
        descriptors,
        1,
        QualityTier::Desktop,
    )
}

pub(crate) fn build_multi_source_generation(
    mesh: &SceneMesh,
    baked: Option<&BakedProbeBatch>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    generation: u64,
    quality_tier: QualityTier,
) -> Result<(MultiSourceSimulation, MultiSourceRenderGraph), BackendError> {
    validate_multi_source_config(mesh, baked, audio, config, descriptors, quality_tier)?;
    let memory = session_memory_telemetry(audio, config, descriptors.len(), baked)?;
    let world = Arc::new(create_world(
        mesh,
        baked,
        audio,
        config,
        descriptors.len(),
        generation,
        quality_tier,
    )?);
    let mut initial = SteamPropagationSnapshot::default();
    initial.world_generation = generation;
    let mut source_poses = [SteamPose {
        position: SteamVector3::default(),
        forward: SteamVector3::new(0.0, 0.0, -1.0),
        up: SteamVector3::new(0.0, 1.0, 0.0),
    }; MAX_ACTIVE_SOURCES];
    let mut source_directivities = [Directivity::OMNIDIRECTIONAL; MAX_ACTIVE_SOURCES];
    let mut source_occlusion_modes = [config.direct_occlusion; MAX_ACTIVE_SOURCES];
    let mut active = [false; MAX_ACTIVE_SOURCES];
    for (index, descriptor) in descriptors.iter().enumerate() {
        source_poses[index] =
            SteamPose::from_api(default_api_pose(descriptor.initial_position_enu)).ok_or(
                BackendError::InvalidInput("multi-source descriptor position must be finite"),
            )?;
        source_directivities[index] = descriptor.directivity;
        source_occlusion_modes[index] =
            crate::direct_occlusion_for_extent(config, descriptor.extent);
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
    let (governor, governor_quality) =
        QualityGovernor::new(audio, config, descriptors, quality_tier, memory);
    let render = create_render_graph(
        Arc::clone(&world),
        audio,
        config,
        reader,
        stage_output_gain_writer,
        stage_output_gains,
        governor_quality,
    )?;
    let simulation = MultiSourceSimulation {
        world,
        audio,
        config,
        source_directivities,
        source_occlusion_modes,
        frame: SimulationFrame {
            listener,
            listener_linear_velocity_mps: SteamVector3::default(),
            sources: source_poses,
            source_linear_velocities_mps: [SteamVector3::default(); MAX_ACTIVE_SOURCES],
            active,
        },
        valid_update: true,
        snapshot: initial,
        publication: writer,
        governor,
        reflection_cadence_tick: 0,
        last_pass_started_ns: [None; 3],
        last_direct_frame: None,
        started: Instant::now(),
    };
    Ok((simulation, render))
}

fn validate_multi_source_config(
    mesh: &SceneMesh,
    baked: Option<&BakedProbeBatch>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    quality_tier: QualityTier,
) -> Result<(), BackendError> {
    validate_audio(audio)?;
    validate_mesh(mesh)?;
    if let Some(baked) = baked {
        baked.validate()?;
    }
    if descriptors.is_empty() || descriptors.len() > quality_tier.active_source_cap() {
        return Err(BackendError::InvalidInput(
            "multi-source session source count exceeds the selected quality tier cap",
        ));
    }
    if descriptors.iter().any(|descriptor| {
        !descriptor.initial_position_enu.is_finite() || !descriptor.declared_level_db().is_finite()
    }) {
        return Err(BackendError::InvalidInput(
            "multi-source descriptor positions and reference levels must be finite",
        ));
    }
    if descriptors
        .iter()
        .any(|descriptor| descriptor.directivity.validate().is_err())
    {
        return Err(BackendError::InvalidInput(
            "multi-source descriptor directivity is outside the validated ranges",
        ));
    }
    if descriptors
        .iter()
        .any(|descriptor| descriptor.extent.validate().is_err())
    {
        return Err(BackendError::InvalidInput(
            "multi-source descriptor extent is invalid",
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

fn session_memory_telemetry(
    audio: AudioConfig,
    config: S3SimulationConfig,
    source_count: usize,
    baked: Option<&BakedProbeBatch>,
) -> Result<SessionMemoryTelemetry, BackendError> {
    let frames = u64::try_from(audio.frame_size)
        .map_err(|_| BackendError::InvalidInput("audio frame size must be positive"))?;
    let sources = source_count as u64;
    let channels = u64::try_from(ambisonics_channel_count(config.reflection_order)?)
        .map_err(|_| BackendError::InvalidInput("Ambisonic channel count must be positive"))?;
    let ir_samples = u64::try_from(reflection_ir_size(
        config.reflection_duration_s,
        audio.sample_rate_hz,
    )?)
    .map_err(|_| BackendError::InvalidInput("reflection IR size must be positive"))?;
    let float_bytes = size_of::<f32>() as u64;

    // SnapshotPublication owns three shared payload slots; each reader retains
    // one last-complete payload. This reports payload bytes, not Arc/allocator
    // bookkeeping.
    let snapshot_ring_payload_bytes = 4_u64.saturating_mul(
        size_of::<SteamPropagationSnapshot>()
            .saturating_add(size_of::<GovernorRenderSnapshot>())
            .saturating_add(size_of::<StageOutputGains>()) as u64,
    );
    let reflection_ir_payload_capacity_bytes =
        if reflection_effect_uses_ir(config.reflection_effect.effect_type) {
            sources
                .saturating_mul(channels)
                .saturating_mul(ir_samples)
                .saturating_mul(float_bytes)
        } else {
            0
        };
    // Per source: mono input + mono direct + stereo direct + stereo path +
    // Ambisonic reflection scratch. Shared: Ambisonic reflection mix + stereo
    // decode target.
    let audio_buffer_payload_bytes = sources
        .saturating_mul(6_u64.saturating_add(channels))
        .saturating_add(channels.saturating_add(2))
        .saturating_mul(frames)
        .saturating_mul(float_bytes);
    let render_scratch_bytes = 3_u64.saturating_mul(frames).saturating_mul(float_bytes);
    let propagation_delay_line_bytes = sources
        .saturating_mul(
            maximum_propagation_delay_samples(audio.sample_rate_hz).saturating_add(4) as u64,
        )
        .saturating_mul(float_bytes);
    let retained_bake_bytes = baked.map_or(0, |value| value.bytes.len() as u64);
    let tracked = snapshot_ring_payload_bytes
        .saturating_add(reflection_ir_payload_capacity_bytes)
        .saturating_add(audio_buffer_payload_bytes)
        .saturating_add(render_scratch_bytes)
        .saturating_add(propagation_delay_line_bytes)
        .saturating_add(retained_bake_bytes);

    Ok(SessionMemoryTelemetry {
        tracked_at_create_bytes: tracked,
        tracked_current_bytes: tracked,
        tracked_peak_bytes: tracked,
        snapshot_ring_payload_bytes,
        reflection_ir_payload_capacity_bytes,
        audio_buffer_payload_bytes,
        render_scratch_bytes,
        propagation_delay_line_bytes,
        retained_bake_bytes,
        steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
    })
}

fn create_world(
    mesh: &SceneMesh,
    baked: Option<&BakedProbeBatch>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    source_count: usize,
    generation: u64,
    quality_tier: QualityTier,
) -> Result<WorldGeneration, BackendError> {
    let probe_influences = baked
        .map(|baked| {
            SerializedProbeInfluences::parse(&baked.bytes, baked.metadata.probe_count)
                .map_err(BackendError::InvalidProbeBatch)
        })
        .transpose()?;
    let mut context = core::ptr::null_mut();
    let mut context_settings = ffi::IPLContextSettings::pinned_defaults();
    sdk_status(
        "iplContextCreate",
        ffi::context_create(&mut context_settings, &mut context),
    )?;
    let mut world = WorldGeneration {
        generation,
        has_baked_pathing: baked.is_some(),
        baked_data_fingerprint: baked.map_or(0, |baked| {
            u64::from_str_radix(&baked.metadata.content_sha256[..16], 16)
                .expect("validated bake SHA-256 is lowercase hexadecimal")
        }),
        context: context as usize,
        scene: 0,
        static_mesh: 0,
        probe_batch: 0,
        simulator: 0,
        sources: [0; MAX_ACTIVE_SOURCES],
        source_count: 0,
        probe_influences,
        serialized_bytes: baked.map_or_else(Vec::new, |baked| baked.bytes.clone()),
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

        let mut probe_batch = core::ptr::null_mut();
        if world.has_baked_pathing {
            let mut serialized_settings = ffi::IPLSerializedObjectSettings {
                data: world.serialized_bytes.as_mut_ptr(),
                size: world.serialized_bytes.len(),
            };
            let mut serialized = core::ptr::null_mut();
            sdk_status(
                "iplSerializedObjectCreate",
                ffi::serialized_object_create(context, &mut serialized_settings, &mut serialized),
            )?;
            let load_status = ffi::probe_batch_load(context, serialized, &mut probe_batch);
            ffi::serialized_object_release(&mut serialized);
            sdk_status("iplProbeBatchLoad", load_status)?;
        } else {
            sdk_status(
                "iplProbeBatchCreate",
                ffi::probe_batch_create(context, &mut probe_batch),
            )?;
        }
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
            maxNumSources: quality_tier.active_source_cap() as i32,
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
        if world.has_baked_pathing {
            ffi::simulator_add_probe_batch(simulator, probe_batch);
        }
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
    mut governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
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
    let applied_governor_quality = governor_quality.read();
    for (index, state) in source_states.iter_mut().enumerate() {
        let listener_centric_reflection = applied_governor_quality.reverb
            != ReverbStrategy::ListenerCentric
            || usize::from(applied_governor_quality.listener_centric_source) == index;
        state.quality_gains = source_quality_targets(
            applied_governor_quality.sources[index],
            listener_centric_reflection,
        );
        if !world.has_baked_pathing {
            state.quality_gains[1] = 0.0;
        }
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
        governor_quality,
        applied_governor_quality,
        reflection_output_gain: applied_governor_quality.reflection_output_gain,
        propagation_block_retention: (-(audio.frame_size as f32 / audio.sample_rate_hz as f32)
            / PROPAGATION_SLEW_TIME_SECONDS)
            .exp(),
        #[cfg(test)]
        governor_snapshot_reads: 0,
    })
}

fn source_quality_targets(
    quality: SourceQualityLevel,
    listener_centric_reflection: bool,
) -> [f32; 3] {
    match quality {
        SourceQualityLevel::Full => [
            1.0,
            1.0,
            if listener_centric_reflection {
                1.0
            } else {
                0.0
            },
        ],
        // "DirectOnly" is the established telemetry/API name for this
        // per-source governor rung. Baked path transport is deliberately kept
        // audible: direct-gain ranking makes occluded sources the first ones
        // selected for degradation, and suppressing their path send here would
        // remove exactly the around-corner energy required by the backend
        // contract. The no-bake override at the call sites still zeros it.
        SourceQualityLevel::DirectOnly => [1.0, 1.0, 0.0],
        SourceQualityLevel::Virtualized => [0.0, 0.0, 0.0],
    }
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
        // The 2,048 m physical cap converted at the graph's sample rate: the
        // whole ring is allocated here so the render callback never does.
        propagation_delay: {
            let mut delay =
                PropagationDelayLine::new(maximum_delay_samples, audio_settings.samplingRate);
            // A source is audible at its real distance from its first block
            // rather than swept in from zero delay.
            delay.reset_to(initial_delay_samples);
            delay
        },
        last_propagation_observation: None,
        rendered_since_reset: false,
        guard_reactivation_history: false,
        reactivation_epoch_samples: 0,
        quality_gains: [1.0; 3],
    })
}

#[cfg(test)]
#[path = "multi_source_teleport_tests.rs"]
mod teleport_tests;

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

    #[test]
    fn point_extent_builds_legacy_default_occlusion_inputs_bit_exactly() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let config = test_config();
        let descriptors = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            2.0, 3.0, 1.5,
        ))];
        let (simulation, _render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            config,
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();

        assert_eq!(
            simulation.source_occlusion_modes[0],
            config.direct_occlusion
        );
        let inputs = source_inputs(
            simulation.frame.sources[0],
            simulation.source_directivities[0],
            simulation.source_occlusion_modes[0],
            simulation.world.probe_batch(),
            config,
            simulation.governor.render_quality(),
            ffi::IPL_SIMULATIONFLAGS_DIRECT,
        )
        .unwrap();
        assert_eq!(inputs.occlusionType, ffi::IPL_OCCLUSIONTYPE_RAYCAST);
        assert_eq!(inputs.occlusionRadius.to_bits(), 0.0_f32.to_bits());
        assert_eq!(inputs.numOcclusionSamples, 0);
    }

    #[test]
    fn retained_world_rejects_degenerate_and_invalid_extents() {
        use fightbox_api::ExtentDescriptor;

        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        for extent in [
            ExtentDescriptor::MultiPoint { count: 0 },
            ExtentDescriptor::LineSegment { length_m: 0.0 },
            ExtentDescriptor::LineSegment { length_m: -1.0 },
            ExtentDescriptor::LineSegment {
                length_m: f32::INFINITY,
            },
            ExtentDescriptor::StereoImage { width_m: 0.0 },
            ExtentDescriptor::StereoImage { width_m: -1.0 },
            ExtentDescriptor::StereoImage { width_m: f32::NAN },
        ] {
            let descriptors = [
                crate::MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5))
                    .with_extent(extent),
            ];
            assert_eq!(
                validate_multi_source_config(
                    &mesh,
                    None,
                    audio,
                    test_config(),
                    &descriptors,
                    QualityTier::Desktop,
                ),
                Err(BackendError::InvalidInput(
                    "multi-source descriptor extent is invalid"
                )),
                "{extent:?}"
            );
        }
    }

    #[test]
    fn line_extent_is_fractionally_occluded_at_a_decisive_wall_edge() {
        use fightbox_api::ExtentDescriptor;

        let mesh = wall_mesh(AcousticMaterial::MASONRY);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let config = test_config();
        let source_position = ApiEnuVector3::new(5.75, 2.0, 1.5);
        let listener_position = ApiEnuVector3::new(4.0, -2.0, 1.5);
        let simulate = |extent| {
            let descriptors =
                [crate::MultiSourceDescriptor::at(source_position).with_extent(extent)];
            let (mut simulation, _render) = build_multi_source_generation(
                &mesh,
                None,
                audio,
                config,
                &descriptors,
                1,
                QualityTier::Desktop,
            )
            .unwrap();
            simulation.update_inputs(&one_source_update(true, source_position, listener_position));
            simulation.run_direct().unwrap();
            (
                simulation.snapshot.sources[0].direct.occlusion,
                simulation.source_occlusion_modes[0],
            )
        };

        let (point_occlusion, point_mode) = simulate(ExtentDescriptor::Point);
        let (line_occlusion, line_mode) = simulate(ExtentDescriptor::LineSegment { length_m: 2.0 });
        assert_eq!(point_mode, DirectOcclusionMode::Raycast);
        assert_eq!(
            line_mode,
            DirectOcclusionMode::Volumetric {
                radius_m: 1.0,
                sample_count: crate::DEFAULT_OCCLUSION_SAMPLE_COUNT,
            }
        );
        assert!(
            point_occlusion <= 0.05 || point_occlusion >= 0.95,
            "point ray was not decisive: {point_occlusion}"
        );
        assert!(
            line_occlusion > 0.05 && line_occlusion < 0.95,
            "line extent did not produce fractional occlusion: {line_occlusion}"
        );
        assert_ne!(line_occlusion.to_bits(), point_occlusion.to_bits());
        eprintln!("extent_edge_occlusion point={point_occlusion:.9} line={line_occlusion:.9}");
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

    fn directivity_capture(
        directivity: Option<Directivity>,
        source_forward: ApiEnuVector3,
    ) -> (Vec<f32>, SteamDirectParams) {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_position = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let listener_position = ApiEnuVector3::new(4.0, 6.0, 1.5);
        let mut descriptor = crate::MultiSourceDescriptor::at(source_position);
        if let Some(directivity) = directivity {
            descriptor = descriptor.with_directivity(directivity);
        }
        let descriptors = [descriptor];
        let (mut simulation, mut render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut update = one_source_update(true, source_position, listener_position);
        update.sources[0].pose.forward = source_forward;
        simulation.update_inputs(&update);
        simulation.run_direct().unwrap();
        let direct = simulation.snapshot.sources[0].direct;

        let mut captured = Vec::new();
        let mut interleaved = vec![0.0; audio.frame_size as usize * 2];
        let mut global_frame = 0_usize;
        for block in 0..24 {
            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = (TAU * 731.0 * global_frame as f32 / audio.sample_rate_hz as f32)
                        .sin()
                        * 0.125;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            render_one_source_block(&mut render, &input);
            if block >= 12 {
                render.sources[0]
                    .direct_stereo
                    .read_interleaved(&mut interleaved);
                captured.extend_from_slice(&interleaved);
            }
        }
        (captured, direct)
    }

    fn sample_bits(samples: &[f32]) -> Vec<u32> {
        samples.iter().map(|sample| sample.to_bits()).collect()
    }

    fn sample_bytes(samples: &[f32]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_bits().to_le_bytes())
            .collect()
    }

    fn energy(samples: &[f32]) -> f64 {
        samples
            .iter()
            .map(|sample| f64::from(sample * sample))
            .sum()
    }

    #[test]
    fn weight_zero_direct_render_is_bit_identical_to_the_prechange_fingerprint() {
        let toward = ApiEnuVector3::new(2.0, 3.0, 0.0);
        let (legacy_default, legacy_direct) = directivity_capture(None, toward);
        let (explicit_omni, explicit_direct) =
            directivity_capture(Some(Directivity::OMNIDIRECTIONAL), toward);
        assert_eq!(sample_bits(&explicit_omni), sample_bits(&legacy_default));
        assert_eq!(explicit_direct, legacy_direct);

        // Captured before directivity was plumbed through `source_inputs`: this
        // pins the same deterministic scene, tone, direct effect, and HRTF PCM.
        let hash = crate::sha256_hex(&sample_bytes(&explicit_omni));
        assert_eq!(
            hash,
            "e43a455e6eda686dbea905e16c434474406f55bf7db6356f2d145d24137a27ef"
        );
        eprintln!(
            "omni_prechange_sha256={hash} energy={:.12e} samples={}",
            energy(&explicit_omni),
            explicit_omni.len()
        );
    }

    #[test]
    fn directional_source_outputs_less_direct_energy_when_facing_away() {
        let directivity = Directivity {
            dipole_weight: 0.7,
            dipole_power: 2.0,
        };
        let toward_axis = ApiEnuVector3::new(2.0, 3.0, 0.0);
        let away_axis = ApiEnuVector3::new(-2.0, -3.0, 0.0);
        let (toward, toward_direct) = directivity_capture(Some(directivity), toward_axis);
        let (away, away_direct) = directivity_capture(Some(directivity), away_axis);
        let toward_energy = energy(&toward);
        let away_energy = energy(&away);

        assert!(toward_energy > 0.0);
        assert!(
            away_energy < toward_energy * 0.1,
            "away energy {away_energy:.12e} was not measurably below toward energy {toward_energy:.12e}"
        );
        assert!(away_direct.directivity < toward_direct.directivity);
        assert!(predicted_direct_gain(away_direct) < predicted_direct_gain(toward_direct));
        eprintln!(
            "directivity_energy toward={toward_energy:.12e} away={away_energy:.12e} ratio={:.12e} toward_gain={:.9} away_gain={:.9}",
            away_energy / toward_energy,
            toward_direct.directivity,
            away_direct.directivity,
        );
    }

    #[test]
    fn directivity_is_source_local_within_one_retained_session() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_position = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let listener_position = ApiEnuVector3::new(4.0, 6.0, 1.5);
        let descriptors = [
            crate::MultiSourceDescriptor::at(source_position).with_directivity(Directivity {
                dipole_weight: 0.7,
                dipole_power: 2.0,
            }),
            crate::MultiSourceDescriptor::at(source_position),
        ];
        let (mut simulation, _render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        let away_pose = Pose {
            position: source_position,
            forward: ApiEnuVector3::new(-2.0, -3.0, 0.0),
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        };
        for source in &mut sources[..2] {
            *source = SourceMotion {
                active: true,
                pose: away_pose,
                linear_velocity_mps: ApiEnuVector3::default(),
            };
        }
        simulation.update_inputs(&SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(listener_position),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        });
        simulation.run_direct().unwrap();

        let directional = simulation.snapshot.sources[0].direct.directivity;
        let omni = simulation.snapshot.sources[1].direct.directivity;
        assert!(directional < 0.2, "directional gain was {directional}");
        assert!((omni - 1.0).abs() < 1.0e-5, "omni gain was {omni}");
    }

    #[test]
    fn snapshot_publishes_validated_source_and_listener_velocities() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_position = ApiEnuVector3::new(10.0, 0.0, 1.5);
        let listener_position = ApiEnuVector3::new(0.0, 0.0, 1.5);
        let descriptor = [crate::MultiSourceDescriptor::at(source_position)];
        let (mut simulation, _render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &descriptor,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let source_velocity = ApiEnuVector3::new(20.0, 3.0, -4.0);
        let listener_velocity = ApiEnuVector3::new(5.0, -2.0, 1.0);
        let mut update = one_source_update(true, source_position, listener_position);
        update.sources[0].linear_velocity_mps = source_velocity;
        update.listener.linear_velocity_mps = listener_velocity;

        simulation.update_inputs(&update);
        simulation.run_direct().unwrap();

        assert_eq!(
            simulation.snapshot.sources[0].linear_velocity_mps,
            api_enu_to_steam(source_velocity)
        );
        assert_eq!(
            simulation.snapshot.listener_linear_velocity_mps,
            api_enu_to_steam(listener_velocity)
        );
        assert_eq!(
            radial_velocity_mps(
                simulation.snapshot.sources[0].source_position,
                simulation.snapshot.sources[0].linear_velocity_mps,
                simulation.snapshot.listener_position,
                simulation.snapshot.listener_linear_velocity_mps,
            )
            .to_bits(),
            15.0_f32.to_bits()
        );

        update.sources[0].linear_velocity_mps.east_m = f32::NAN;
        simulation.update_inputs(&update);
        assert!(matches!(
            simulation.run_direct(),
            Err(SimulationError::InvalidUpdate)
        ));
    }

    #[test]
    fn governor_virtualization_advances_transport_and_reads_one_snapshot_per_block() {
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
        let descriptor = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0))
                .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter {
                    db_spl: -20.0,
                }),
        ];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptor).unwrap();

        let zeros = vec![0.0; audio.frame_size as usize];
        let ones = vec![1.0; audio.frame_size as usize];
        let reads_before = render.governor_snapshot_reads;
        assert_eq!(
            render.sources[0].quality_gains, [0.0; 3],
            "the first render block must inherit the conservative snapshot"
        );
        render_one_source_block(&mut render, &zeros);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        assert_eq!(
            simulation.quality_governor_telemetry().sources[0].quality,
            SourceQualityLevel::Virtualized
        );

        assert_eq!(render.governor_snapshot_reads - reads_before, 3);
        assert_eq!(render.sources[0].quality_gains, [0.0; 3]);
        assert!(
            render.mono_work.iter().any(|sample| *sample > 0.5),
            "the delay/transport path stopped while backend DSP was virtualized"
        );

        // Sustained headroom earns the preceding reverb and order rungs, then
        // restores this source. The next block ramps from silence to full
        // quality without rewinding transport.
        for _ in 0..5_000 {
            simulation.observe_render_timing(100_000);
            if simulation.quality_governor_telemetry().sources[0].quality
                == SourceQualityLevel::Full
            {
                break;
            }
        }
        assert_eq!(
            simulation.quality_governor_telemetry().sources[0].quality,
            SourceQualityLevel::Full
        );
        render_one_source_block(&mut render, &ones);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        assert_eq!(render.governor_snapshot_reads - reads_before, 6);
        assert_eq!(render.sources[0].quality_gains, [1.0; 3]);
    }

    #[test]
    fn governor_gain_ramp_reaches_both_endpoints() {
        let down = GainRamp::new(1.0, 0.0, 128);
        assert_eq!(down.at(0), 1.0);
        assert_eq!(down.at(127), 0.0);
        let up = GainRamp::new(0.0, 1.0, 128);
        assert_eq!(up.at(0), 0.0);
        assert_eq!(up.at(127), 1.0);
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

    /// A large reflective ground plane. A source and listener both 2 m above
    /// it get one clean specular bounce whose path length is known exactly.
    fn reflective_ground_mesh() -> SceneMesh {
        SceneMesh {
            vertices_enu_m: vec![
                EnuVector3::new(-80.0, -80.0, 0.0),
                EnuVector3::new(80.0, -80.0, 0.0),
                EnuVector3::new(80.0, 80.0, 0.0),
                EnuVector3::new(-80.0, 80.0, 0.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
            material_indices: vec![0; 4],
            materials: vec![AcousticMaterial::MASONRY],
        }
    }

    /// Onset of a simulated reflection IR, measured through a standalone
    /// effect so the render graph's propagation delay is not in the path.
    fn reflection_ir_onset(source_distance_m: f32) -> usize {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        // Denser than `test_config`: this measurement needs an IR with a
        // clearly located first arrival, not merely a nonzero one.
        let config = S3SimulationConfig {
            reflection_rays: 4_096,
            diffuse_samples: 32,
            reflection_bounces: 2,
            reflection_duration_s: 0.15,
            reflection_order: 1,
            ..S3SimulationConfig::default()
        };
        let listener_position = ApiEnuVector3::new(0.0, 0.0, 2.0);
        let source_position = ApiEnuVector3::new(source_distance_m, 0.0, 2.0);
        let descriptor = [crate::MultiSourceDescriptor::at(source_position)];
        let (mut simulation, render) = build_multi_source_generation(
            &reflective_ground_mesh(),
            None,
            audio,
            config,
            &descriptor,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        // The governor starts conservative; without earning full quality first
        // the simulator returns a first-order stub IR carrying no geometry.
        for _ in 0..20_000 {
            simulation.observe_render_timing(100_000);
        }
        simulation.update_inputs(&one_source_update(true, source_position, listener_position));
        for _ in 0..4 {
            simulation.run_reflections().unwrap();
        }
        let reflection = simulation.snapshot.sources[0].reflections;
        assert!(reflection.ir != 0, "reflection simulation produced no IR");

        let context: ffi::IPLContext = handle(render.world.context);
        let mut audio_settings = raw_audio_settings(audio);
        // The governor, not the config, decides the order and duration the
        // simulator actually produced. An effect built to any other shape
        // reads the IR wrongly and returns near-silence.
        let mut settings = ffi::IPLReflectionEffectSettings {
            type_: reflection_effect_ffi_type(config.reflection_effect.effect_type).unwrap(),
            irSize: reflection.ir_size,
            numChannels: reflection.num_channels,
        };
        let mut effect = core::ptr::null_mut();
        assert_eq!(
            ffi::reflection_effect_create(context, &mut audio_settings, &mut settings, &mut effect),
            ffi::IPL_STATUS_SUCCESS
        );
        let mut input = OwnedAudioBuffer::allocate(context, 1, audio.frame_size).unwrap();
        let mut output =
            OwnedAudioBuffer::allocate(context, settings.numChannels, audio.frame_size).unwrap();
        let mut interleaved = vec![0.0; (settings.numChannels * audio.frame_size) as usize];
        // Capture the whole response first, then locate its onset relative to
        // its own peak: absolute thresholds cannot be shared across two
        // source distances whose reflected levels differ by 20 dB or more.
        let blocks = (2.0 * audio.sample_rate_hz as f32 / audio.frame_size as f32).ceil() as usize;
        let mut response = Vec::with_capacity(blocks * audio.frame_size as usize);
        for block in 0..blocks {
            let mut samples = vec![0.0; audio.frame_size as usize];
            if block == 0 {
                samples[0] = 1.0;
            }
            input.write_mono(&mut samples);
            let mut input_raw = input.raw();
            let mut output_raw = output.raw();
            let mut params = reflection_effect_params(reflection, config);
            ffi::reflection_effect_apply(effect, &mut params, &mut input_raw, &mut output_raw);
            output.read_interleaved(&mut interleaved);
            response.extend(
                interleaved
                    .chunks_exact(settings.numChannels as usize)
                    .map(|frame| frame.iter().fold(0.0_f32, |peak, s| peak.max(s.abs()))),
            );
        }
        ffi::reflection_effect_release(&mut effect);

        let peak = response.iter().copied().fold(0.0_f32, f32::max);
        assert!(peak > 0.0, "reflection effect produced no output at all");
        let onset = response
            .iter()
            .position(|sample| *sample > peak * 1.0e-3)
            .expect("a nonzero response must have an onset");
        println!(
            "reflection IR probe: source {source_position:?} irSize {} channels {} \
             peak {peak:e} onset {onset}",
            reflection.ir_size, reflection.num_channels
        );
        onset
    }

    /// Establishes which side of the seam owns source-distance time of flight.
    ///
    /// If Steam Audio's simulated IR already carried the source-to-listener
    /// flight time, delaying the reflection send as well would double it. The
    /// geometry here separates the two possibilities cleanly. Source and
    /// listener sit 2 m above a reflective plane, so the specular path is
    /// `sqrt(d^2 + 16)` against a direct path of `d`:
    ///
    /// | source distance | path length | if IR starts at emission | measured onset |
    /// |---|---|---|---|
    /// | 2 m   | 4.47 m  | 626 samples   | 1 sample |
    /// | 40 m  | 40.20 m | 5,626 samples | 1 sample |
    ///
    /// Both IRs begin immediately, 5,000 samples apart from what an
    /// emission-referenced IR would give, while their peaks differ by 29 dB in
    /// the direction distance attenuation predicts — so the IRs are real and
    /// simply carry no absolute flight time. Steam Audio references them to
    /// the listener; the render graph owns the source-distance delay.
    #[test]
    fn linked_reflection_ir_does_not_encode_source_distance() {
        let near_onset = reflection_ir_onset(2.0);
        let far_onset = reflection_ir_onset(40.0);

        // Emission-referenced would put the far onset ~5,000 samples later.
        assert!(
            far_onset < near_onset + 1_000,
            "the far source's reflection IR started {far_onset} samples in \
             against {near_onset} near, which tracks absolute source distance: \
             Steam Audio would already be encoding time of flight and the \
             render graph would be double-delaying the reflection send"
        );
    }

    #[test]
    fn linked_source_teleport_crossfades_instead_of_sweeping_pitch() {
        const TONE_HZ: f32 = 1_000.0;
        const WARMUP_BLOCKS: usize = 150;
        const TOTAL_BLOCKS: usize = 500;
        const SETTLE_BLOCKS: usize = 200;
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
            5.0, 0.0, 0.0,
        ))];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptor).unwrap();

        let frames = audio.frame_size as usize;
        let mut global_frame = 0_usize;
        let mut before = Vec::new();
        let mut after = Vec::new();
        for block in 0..TOTAL_BLOCKS {
            if block == WARMUP_BLOCKS {
                // 5 m to 60 m in one update: 7,700 samples of delay, far past
                // the 50 ms discontinuity threshold.
                let mut snapshot = simulation.snapshot;
                snapshot.sequence = snapshot.sequence.wrapping_add(1);
                snapshot.sources[0].source_position = SteamVector3::new(60.0, 0.0, 0.0);
                simulation.publication.publish(snapshot);
            }
            let input = (0..frames)
                .map(|_| {
                    let sample =
                        (TAU * TONE_HZ * global_frame as f32 / audio.sample_rate_hz as f32).sin();
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            render_one_source_block(&mut render, &input);
            if (100..WARMUP_BLOCKS).contains(&block) {
                before.extend_from_slice(&render.mono_work);
            } else if block >= SETTLE_BLOCKS {
                after.extend_from_slice(&render.mono_work);
            }
        }

        // The 50 ms crossfade is 19 blocks; by block 200 it is long finished.
        assert!(
            !render.sources[0].propagation_delay.is_crossfading(),
            "teleport crossfade did not complete within its window"
        );
        let expected_delay = 60.0 * 48_000.0 / 343.0;
        assert!(
            (render.sources[0].propagation_delay.current_delay_samples() - expected_delay).abs()
                < 1.0,
            "delay did not land on the post-teleport distance"
        );

        // A slewed teleport would transpose the tone for the whole glide. Both
        // windows must instead carry the undisplaced source frequency.
        let crossings = |samples: &[f32]| {
            samples
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count() as f32
                * audio.sample_rate_hz as f32
                / samples.len() as f32
        };
        let before_hz = crossings(&before);
        let after_hz = crossings(&after);
        assert!(
            (before_hz - TONE_HZ).abs() < 3.0,
            "pre-teleport tone measured {before_hz} Hz"
        );
        assert!(
            (after_hz - TONE_HZ).abs() < 3.0,
            "post-teleport tone measured {after_hz} Hz, so the jump was \
             gliding rather than crossfading"
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
    fn volumetric_edge_crossing_has_partial_visibility_and_a_bounded_render_slew() {
        let mesh = wall_mesh(AcousticMaterial::MASONRY);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let config = S3SimulationConfig {
            direct_occlusion: DirectOcclusionMode::Volumetric {
                radius_m: crate::DEFAULT_OCCLUSION_SOURCE_RADIUS_METERS,
                sample_count: crate::DEFAULT_OCCLUSION_SAMPLE_COUNT,
            },
            ..test_config()
        };
        let initial_source = ApiEnuVector3::new(4.0, 2.0, 1.5);
        let listener = ApiEnuVector3::new(4.0, -2.0, 1.5);
        let descriptors = [crate::MultiSourceDescriptor::at(initial_source)];
        let (mut simulation, _render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            config,
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let retention = (-(audio.frame_size as f32 / audio.sample_rate_hz as f32)
            / PROPAGATION_SLEW_TIME_SECONDS)
            .exp();
        let maximum_endpoint_step = 1.0 - retention;
        let mut smoother = SourcePropagationSmoother::default();
        let mut raw = Vec::new();
        let mut applied = Vec::new();

        for position_index in 0..=32 {
            let source = ApiEnuVector3::new(4.0 + position_index as f32 * 0.125, 2.0, 1.5);
            simulation.update_inputs(&one_source_update(true, source, listener));
            simulation.run_direct().unwrap();
            let propagation = simulation.snapshot.sources[0];
            raw.push(propagation.direct.occlusion);
            applied.push(
                smoother
                    .advance(
                        propagation,
                        simulation.snapshot.listener_position,
                        retention,
                    )
                    .endpoint()
                    .direct
                    .occlusion,
            );
        }

        assert!(
            raw.iter().any(|value| *value > 0.0 && *value < 1.0),
            "volumetric edge crossing never reported partial visibility: {raw:?}"
        );
        let maximum_applied_step = applied
            .windows(2)
            .map(|values| (values[1] - values[0]).abs())
            .fold(0.0_f32, f32::max);
        assert!(
            maximum_applied_step <= maximum_endpoint_step + f32::EPSILON,
            "smoothed endpoint step {maximum_applied_step} exceeded {maximum_endpoint_step}: {applied:?}"
        );
        eprintln!(
            "volumetric_edge_crossing raw={raw:?} applied={applied:?} max_applied_step={maximum_applied_step}"
        );
    }

    #[test]
    fn source_diagnostics_read_the_requested_source_not_source_zero() {
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
        let descriptors = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5)),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5)),
        ];
        let (mut simulation, _render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptors).unwrap();
        simulation.update_inputs(&update(true, true));
        simulation.run_direct().unwrap();
        simulation.run_pathing().unwrap();
        simulation.run_reflections().unwrap();

        let zero = simulation
            .source_diagnostics(0)
            .expect("source zero exists");
        let one = simulation.source_diagnostics(1).expect("source one exists");
        assert_eq!(zero.source_index, 0);
        assert_eq!(one.source_index, 1);
        for (diagnostics, snapshot) in [(zero, 0), (one, 1)] {
            let snapshot = simulation.snapshot.sources[snapshot];
            assert_eq!(
                diagnostics.distance_attenuation.to_bits(),
                snapshot.direct.distance_attenuation.to_bits()
            );
            assert_eq!(
                diagnostics.occlusion.to_bits(),
                snapshot.direct.occlusion.to_bits()
            );
            assert_eq!(
                diagnostics.transmission.map(f32::to_bits),
                snapshot.direct.transmission.map(f32::to_bits)
            );
            assert_eq!(
                diagnostics.path_eq.map(f32::to_bits),
                snapshot.path_eq.map(f32::to_bits)
            );
            assert_eq!(diagnostics.reflection_ir_size, snapshot.reflections.ir_size);
        }
        // The two sources sit at different distances from the listener, so a
        // reader that silently returned source zero would be caught here.
        assert_ne!(
            zero.distance_attenuation.to_bits(),
            one.distance_attenuation.to_bits(),
            "both sources reported the same distance attenuation: {zero:?} {one:?}"
        );

        simulation.update_inputs(&update(true, false));
        simulation.run_direct().unwrap();
        assert!(simulation.source_diagnostics(0).unwrap().active);
        assert!(!simulation.source_diagnostics(1).unwrap().active);

        // The snapshot array is MAX_ACTIVE_SOURCES wide regardless of how many
        // sources this session configured; unconfigured slots are not sources.
        assert!(simulation.source_diagnostics(2).is_none());
        assert!(simulation.source_diagnostics(MAX_ACTIVE_SOURCES).is_none());
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

    #[test]
    fn occluded_moving_source_retains_audible_baked_path_send_at_direct_only_quality() {
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
        let descriptors = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            1.0, 0.0, 1.5,
        ))];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptors).unwrap();
        assert_eq!(
            simulation.quality_governor_telemetry().sources[0].quality,
            SourceQualityLevel::DirectOnly
        );
        assert_eq!(render.sources[0].quality_gains, [1.0, 1.0, 0.0]);

        let mut stage_gains = render.take_stage_output_gain_writer().unwrap();
        stage_gains.publish(StageOutputGains {
            direct: 0.0,
            pathing: 1.0,
            reflections: 0.0,
        });

        let mut global_frame = 0_usize;
        let mut path_energy = 0.0_f64;
        for block in 0..16 {
            let mut snapshot = simulation.snapshot;
            snapshot.sequence = snapshot.sequence.wrapping_add(1);
            snapshot.listener_position = api_enu_to_steam(ApiEnuVector3::new(0.0, 0.0, 1.5));
            let source = &mut snapshot.sources[0];
            source.active = true;
            source.source_position =
                api_enu_to_steam(ApiEnuVector3::new(1.0 + block as f32 * 0.05, 0.0, 1.5));
            source.direct.occlusion = 0.0;
            source.direct.transmission = [0.0; 3];
            source.path_eq = [1.0; 3];
            source.path_sh = [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS];
            source.path_sh[0] = 1.0;
            source.configured_pathing_order = 1;
            simulation.snapshot = snapshot;
            simulation.publication.publish(snapshot);

            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = (TAU * 440.0 * global_frame as f32 / audio.sample_rate_hz as f32)
                        .sin()
                        * 0.1;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            let (left, right) = render_one_source_block(&mut render, &input);
            path_energy += left
                .into_iter()
                .chain(right)
                .map(|sample| f64::from(sample * sample))
                .sum::<f64>();
        }

        assert!(
            path_energy > 1.0e-8,
            "path-only output was silent for an occluded moving source: {path_energy:.12e}"
        );
        eprintln!("occluded_moving_source path_only_energy={path_energy:.12e}");
        assert_eq!(render.sources[0].quality_gains, [1.0, 1.0, 0.0]);
    }

    #[test]
    fn render_rejects_a_snapshot_from_any_other_world_generation() {
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
        let descriptors = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            2.0, 3.0, 1.5,
        ))];
        let (mut simulation, mut render) = build_multi_source_generation(
            &mesh,
            Some(&baked),
            audio,
            test_config(),
            &descriptors,
            41,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut wrong_generation = simulation.snapshot;
        wrong_generation.world_generation = 42;
        simulation.publication.publish(wrong_generation);

        let input = vec![0.0; audio.frame_size as usize];
        let sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: &input,
        }];
        let mut left = vec![0.0; input.len()];
        let mut right = vec![0.0; input.len()];
        assert_eq!(
            render.render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            }),
            Err(BackendRenderError::InactiveGraph)
        );
    }
}
