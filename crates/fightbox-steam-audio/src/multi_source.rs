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
    SteamWidthState, api_enu_to_steam, fixed_path_sh, path_coefficient_count,
};
use crate::echo_sidecar::{
    CORNER_LOSS_DB_HIGH, CORNER_LOSS_DB_LOW, CORNER_LOSS_DB_MID, EchoDelayRing, EchoLoopScheduler,
    EchoPathKind, EchoPlanError, EchoPrimary, EchoProfile, EchoSourcePlan, EchoTapPlan,
    EchoTrigger, EchoTriggerGenerations, ExternalEchoPlan, MAX_ECHO_TAPS_PER_SOURCE,
    PlannedEchoTap, PlannedEchoTaps, delivered_tap_counts,
};
use crate::governor::{
    GovernorRenderSnapshot, GovernorSimulationPass, QualityGovernor, QualityGovernorTelemetry,
    ReflectionQualityLevel, ReverbStrategy, SourceQualityLevel,
};
use crate::impulse_shaping::ImpulseShaper;
use crate::motion_smoothing::{
    PROPAGATION_SLEW_TIME_SECONDS, SPEED_OF_SOUND_METERS_PER_SECOND, SourcePropagationSmoother,
    maximum_propagation_delay_samples, uncapped_propagation_delay_samples,
};
use crate::neutral_environment::{
    MAX_NEUTRAL_ENVIRONMENT_CHANNELS, STEAM_TO_NEUTRAL_ACN_INDICES, STEAM_TO_NEUTRAL_N3D_GAINS,
    active_channel_count,
};
use crate::route_voicing::{
    PATH_SH_Y00, RouteAirVoicing, RouteDelayTarget, RouteReadHead, realizable_eq,
};

#[path = "steady_silent_pair.rs"]
mod steady_silent_pair;
use steady_silent_pair::{SteadyBinaural, SteadyPath, SteadySilentPair};
#[path = "reflection_adoption.rs"]
mod reflection_adoption;
use reflection_adoption::ReflectionAdoption;

#[path = "reflection_worker.rs"]
mod reflection_worker;
use reflection_worker::{ReflectionCompletion, ReflectionJob, ReflectionJobGroup, ReflectionWorker};

#[path = "full_spatial_export.rs"]
mod full_spatial_export;
pub(crate) use full_spatial_export::FullSpatialExportGraph;
use full_spatial_export::FullSpatialExportTap;

// The environment-bank plane-wise hoist assumes the frozen Steam->neutral
// transform is the identity (channel i maps to plane i with unit gain). Fail
// the build if that frozen table ever changes.
const _: () = {
    let mut identity = true;
    let mut channel = 0;
    while channel < MAX_NEUTRAL_ENVIRONMENT_CHANNELS {
        if STEAM_TO_NEUTRAL_ACN_INDICES[channel] != channel
            || STEAM_TO_NEUTRAL_N3D_GAINS[channel] != 1.0
        {
            identity = false;
        }
        channel += 1;
    }
    assert!(identity);
};
use crate::probe_influence::SerializedProbeInfluences;
use crate::propagation_delay::{
    PropagationDelayLine, StereoProgramPropagationDelay, TELEPORT_DELAY_STEP_SECONDS,
    bandlimited_kernel_payload_bytes, delay_history_len,
};
use crate::width_render::WIDTH_RENDERER_REVISION;
use crate::width_render::{DECLARED_LATENCY_SAMPLES, LineWidthRenderer, line_geometry};
use crate::{
    MemoryTrackingStatus, QualityTier, ReflectionDelivery, SessionMemoryTelemetry,
    SourceReflectionBudget,
};
use fightbox_api::{Directivity, EnuVector3 as ApiEnuVector3, ExtentDescriptor, Pose};
use fightbox_runtime::SnapshotPublication;
use fightbox_runtime::backend::{
    BackendRenderError, ListenerOrientation, MAX_ACTIVE_SOURCES,
    MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS,
    MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE, SimulationError,
    SimulationUpdate, SpatialAmbisonicChannelOrder, SpatialAmbisonicNormalization,
    SpatialAmbisonicOrder, SpatialBackendRenderError, SpatialBackendRenderGraph,
    SpatialBackendSourceBlock,
    SpatialEnvironmentalBasis, SpatialFeedPlacement, SpatialOutputValidity,
    SpatialPresentationComponent, SpatialPresentationFeedMetadata, SpatialPropagationRenderBlock,
    SpatialTailRetirementState,
};
#[cfg(test)]
use fightbox_runtime::backend::PropagationRenderBlock;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq)]
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

const PATH_GATE_MISS_THRESHOLD: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PathGateState {
    consecutive_misses: u8,
    route_air: RouteAirVoicing,
}

#[derive(Clone, Copy)]
struct NeutralSimulationIndirectPolicy {
    pathing: [bool; MAX_ACTIVE_SOURCES],
    reflections: [bool; MAX_ACTIVE_SOURCES],
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SimulationWorkCounters {
    pub vendor_pass_runs: [u64; 3],
    pub skipped_vendor_passes: [u64; 3],
    pub source_output_queries: [u64; 3],
}

const NO_REFLECTION_GROUP: u8 = u8::MAX;
const MAX_REFLECTION_GROUPS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ReflectionBudgetGroup {
    requested: SourceReflectionBudget,
    inherit_shared_quality: bool,
    tick: u64,
}

impl Default for ReflectionBudgetGroup {
    fn default() -> Self {
        Self {
            requested: SourceReflectionBudget::OFF,
            inherit_shared_quality: false,
            tick: 0,
        }
    }
}

impl ReflectionBudgetGroup {
    fn delivered(self, tier: QualityTier, quality: GovernorRenderSnapshot) -> SourceReflectionBudget {
        if self.inherit_shared_quality {
            SourceReflectionBudget::realtime(self.requested.rays.min(quality.reflections.rays),
                self.requested.bounces.min(quality.reflections.bounces),
                self.requested.duration_s.min(quality.reflections.ir_duration_s),
                quality.ambisonic_order, quality.reflections.cadence_divisor.max(1))
        } else {
            delivered_reflection_budget(self.requested, tier, quality)
        }
    }
}


#[derive(Clone, Copy, Debug, PartialEq)]
struct ReflectionBudgetPlan {
    groups: [ReflectionBudgetGroup; MAX_REFLECTION_GROUPS],
    group_count: u8,
    source_groups: [u8; MAX_ACTIVE_SOURCES],
    quality_tier: QualityTier,
}

impl ReflectionBudgetPlan {
    fn source_in_group(self, source_index: usize, group_index: usize) -> bool {
        self.source_groups[source_index] == group_index as u8
    }
}

impl PathGateState {
    fn resolve(
        &mut self,
        propagation: &mut SteamSourcePropagation,
        path_eq: [f32; 3],
        path_sh: [f32; crate::backend_snapshot::MAX_PATH_SH_COEFFS],
    ) {
        self.consecutive_misses = 0;
        propagation.path_eq = path_eq;
        propagation.path_sh = path_sh;
    }

    /// Holds the last valid path for two misses, then publishes the silent
    /// target consumed by `SourcePropagationSmoother` on the third.
    fn miss(&mut self, propagation: &mut SteamSourcePropagation) -> bool {
        self.consecutive_misses = self
            .consecutive_misses
            .saturating_add(1)
            .min(PATH_GATE_MISS_THRESHOLD);
        if self.consecutive_misses < PATH_GATE_MISS_THRESHOLD {
            return false;
        }
        silence_path(propagation);
        true
    }

    /// Bypasses miss hysteresis for discontinuities whose old path is no
    /// longer spatially meaningful. The target becomes silent immediately;
    /// an initialized render graph removes the residual through its existing
    /// 80 ms propagation smoother.
    fn invalidate(&mut self, propagation: &mut SteamSourcePropagation) {
        self.consecutive_misses = 0;
        self.route_air.reset();
        silence_path(propagation);
    }
}

/// The EQ holds, so the fade keeps the route's air-darkened timbre instead of
/// brightening toward a neutral EQ. A path that never resolved has no EQ to
/// hold, only the snapshot's empty default, and goes neutral.
fn silence_path(propagation: &mut SteamSourcePropagation) {
    propagation.path_sh = [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS];
    if !propagation
        .path_eq
        .into_iter()
        .all(|gain| gain.is_finite() && gain > 0.0)
    {
        propagation.path_eq = [1.0; 3];
    }
}

fn endpoint_teleported(previous: SteamVector3, current: SteamVector3) -> bool {
    let x = current.x - previous.x;
    let y = current.y - previous.y;
    let z = current.z - previous.z;
    let threshold_m = TELEPORT_DELAY_STEP_SECONDS * SPEED_OF_SOUND_METERS_PER_SECOND;
    x * x + y * y + z * z > threshold_m * threshold_m
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

/// Uniform spatial index over the serialized probe-influence sphere AABBs.
///
/// Built once on the control thread when a world generation is adopted; the
/// spheres are immutable afterwards. A query visits only the grid cell that
/// contains the point, so cost no longer scales with the probe count, and
/// every candidate is re-tested with the exact serialized containment
/// predicate (same f32 operations on identical bits), which makes the boolean
/// result identical to the full linear scan for every input point.
struct ProbeInfluenceGrid {
    origin: [f64; 3],
    cell_size: f64,
    dims: [usize; 3],
    /// Prefix offsets into `sphere_indices`; `cells + 1` entries.
    cell_offsets: Vec<u32>,
    sphere_indices: Vec<u32>,
    /// Decoded `(x, y, z, radius)` per sphere; bit-identical to the serialized
    /// little-endian payload.
    spheres: Vec<f32>,
}

/// Upper bound on allocated grid cells per world generation.
const PROBE_GRID_MAX_CELLS: usize = 1 << 18;

impl ProbeInfluenceGrid {
    fn build(influences: SerializedProbeInfluences, bytes: &[u8]) -> Option<Self> {
        let sphere_count = influences.probe_count();
        if sphere_count == 0 {
            return None;
        }
        let mut spheres = Vec::with_capacity(sphere_count * 4);
        let mut min = [f64::INFINITY; 3];
        let mut max = [f64::NEG_INFINITY; 3];
        let mut max_radius = 0.0_f64;
        for (center, radius) in influences.spheres(bytes) {
            let coords = [
                f64::from(center.x),
                f64::from(center.y),
                f64::from(center.z),
            ];
            let radius_f64 = f64::from(radius);
            if !coords.into_iter().all(f64::is_finite) || !radius_f64.is_finite() {
                return None;
            }
            // Grid geometry uses the exact f64 bounds; per-sphere insertion
            // below widens its own range by a few ulps so f64 rounding can
            // never exclude a sphere whose real volume contains a point.
            for (axis, &coordinate) in coords.iter().enumerate() {
                min[axis] = min[axis].min(coordinate - radius_f64);
                max[axis] = max[axis].max(coordinate + radius_f64);
            }
            max_radius = max_radius.max(radius_f64);
            spheres.extend_from_slice(&[center.x, center.y, center.z, radius]);
        }
        let extent = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
        if !extent.into_iter().all(f64::is_finite) {
            return None;
        }
        // A cell at least twice the largest radius keeps every sphere inside
        // at most two cells per axis (at most eight cells overall).
        let longest_extent = extent.into_iter().fold(0.0_f64, f64::max);
        let mut cell_size = (2.0 * max_radius).max(longest_extent / 256.0);
        if !(cell_size > 0.0) {
            cell_size = 1.0;
        }
        let mut dims = [1_usize; 3];
        let cells = loop {
            for (axis, extent_axis) in extent.into_iter().enumerate() {
                dims[axis] = (extent_axis / cell_size).ceil().max(1.0) as usize;
            }
            let cells = dims[0].saturating_mul(dims[1]).saturating_mul(dims[2]);
            if cells <= PROBE_GRID_MAX_CELLS {
                break cells;
            }
            cell_size *= 2.0;
        };
        let mut counts = vec![0_u32; cells];
        for (center, radius) in influences.spheres(bytes) {
            let (lo, hi) = Self::sphere_cell_range(&min, cell_size, dims, center, radius);
            for ix in lo[0]..=hi[0] {
                for iy in lo[1]..=hi[1] {
                    for iz in lo[2]..=hi[2] {
                        counts[(ix * dims[1] + iy) * dims[2] + iz] += 1;
                    }
                }
            }
        }
        let mut cell_offsets = Vec::with_capacity(cells + 1);
        let mut running = 0_u32;
        for count in &counts {
            cell_offsets.push(running);
            running += *count;
        }
        cell_offsets.push(running);
        drop(counts);
        let mut sphere_indices = vec![0_u32; running as usize];
        let mut cursor = cell_offsets[..cells].to_vec();
        for (sphere_index, (center, radius)) in influences.spheres(bytes).enumerate() {
            let (lo, hi) = Self::sphere_cell_range(&min, cell_size, dims, center, radius);
            for ix in lo[0]..=hi[0] {
                for iy in lo[1]..=hi[1] {
                    for iz in lo[2]..=hi[2] {
                        let cell = (ix * dims[1] + iy) * dims[2] + iz;
                        sphere_indices[cursor[cell] as usize] = sphere_index as u32;
                        cursor[cell] += 1;
                    }
                }
            }
        }
        Some(Self {
            origin: min,
            cell_size,
            dims,
            cell_offsets,
            sphere_indices,
            spheres,
        })
    }

    /// Conservative widened-AABB cell range for one sphere.
    fn sphere_cell_range(
        origin: &[f64; 3],
        cell_size: f64,
        dims: [usize; 3],
        center: SteamVector3,
        radius: f32,
    ) -> ([usize; 3], [usize; 3]) {
        let coords = [
            f64::from(center.x),
            f64::from(center.y),
            f64::from(center.z),
        ];
        let radius = f64::from(radius);
        let slack = f64::EPSILON
            * 8.0
            * (coords[0].abs() + coords[1].abs() + coords[2].abs() + radius + 1.0);
        let mut lo = [0_usize; 3];
        let mut hi = [0_usize; 3];
        for axis in 0..3 {
            let lower = ((coords[axis] - radius - slack - origin[axis]) / cell_size)
                .floor()
                .clamp(0.0, (dims[axis] - 1) as f64) as usize;
            let upper = ((coords[axis] + radius + slack - origin[axis]) / cell_size)
                .floor()
                .clamp(0.0, (dims[axis] - 1) as f64) as usize;
            lo[axis] = lower;
            hi[axis] = upper.max(lower);
        }
        (lo, hi)
    }

    fn contains(&self, point: SteamVector3) -> bool {
        let coords = [f64::from(point.x), f64::from(point.y), f64::from(point.z)];
        if !coords.into_iter().all(f64::is_finite) {
            // With any non-finite coordinate every serialized predicate
            // degenerates to NaN/inf <= finite, which is always false.
            return false;
        }
        let mut cell = 0_usize;
        for (axis, &coordinate) in coords.iter().enumerate() {
            let index = ((coordinate - self.origin[axis]) / self.cell_size)
                .floor()
                .clamp(0.0, (self.dims[axis] - 1) as f64) as usize;
            cell = cell * self.dims[axis] + index;
        }
        let start = self.cell_offsets[cell] as usize;
        let end = self.cell_offsets[cell + 1] as usize;
        self.sphere_indices[start..end].iter().any(|&index| {
            let base = 4 * index as usize;
            let x = self.spheres[base] - point.x;
            let y = self.spheres[base + 1] - point.y;
            let z = self.spheres[base + 2] - point.z;
            let radius = self.spheres[base + 3];
            x * x + y * y + z * z <= radius * radius
        })
    }
}

struct WorldGeneration {
    generation: u64,
    has_baked_pathing: bool,
    roof_profile: crate::over_roof::RoofProfile,
    baked_data_fingerprint: u64,
    context: usize,
    scene: usize,
    static_mesh: usize,
    probe_batch: usize,
    simulator: usize,
    sources: [usize; MAX_ACTIVE_SOURCES],
    source_simulation_flags: [i32; MAX_ACTIVE_SOURCES],
    source_count: usize,
    reflection_worker_enabled: AtomicBool,
    reflection_acknowledged: [AtomicU64; MAX_ACTIVE_SOURCES],
    reflection_worker_hold_source: AtomicUsize,
    reflection_worker_hold_ir: AtomicUsize,
    probe_influences: Option<SerializedProbeInfluences>,
    /// Uniform spatial index over the influence spheres, built once on the
    /// control thread when this generation is adopted. `None` when there are
    /// no spheres or the layout degenerates; the fallback below stays exact.
    probe_grid: Option<ProbeInfluenceGrid>,
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
        if let Some(grid) = &self.probe_grid {
            return grid.contains(position);
        }
        self.probe_influences
            .is_some_and(|probes| probes.contains(&self.serialized_bytes, position))
    }

    /// Capacity-based payload of the Rust-owned probe influence index
    /// retained for the lifetime of this generation: the cell-offset prefix
    /// table, the per-cell sphere index list, and the decoded sphere records.
    /// Capacity matches the `serialized_bytes.capacity()` convention because
    /// all three vectors are built once with their final extents and never
    /// shrink while the generation lives.
    fn probe_grid_payload_capacity_bytes(&self) -> u64 {
        self.probe_grid.as_ref().map_or(0, |grid| {
            (grid.cell_offsets.capacity() + grid.sphere_indices.capacity()) as u64
                * size_of::<u32>() as u64
                + grid.spheres.capacity() as u64 * size_of::<f32>() as u64
        })
    }
}

impl Drop for WorldGeneration {
    fn drop(&mut self) {
        let hold_source = self.reflection_worker_hold_source.load(Ordering::Relaxed);
        if hold_source != 0 {
            let mut source = handle(hold_source);
            ffi::source_release(&mut source);
        }
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
    scene_air_writer: Option<fightbox_runtime::SnapshotWriter<[f32; 3]>>,
    scene_air: fightbox_runtime::SnapshotReader<[f32; 3]>,
    source_directivities: [Directivity; MAX_ACTIVE_SOURCES],
    source_occlusion_modes: [DirectOcclusionMode; MAX_ACTIVE_SOURCES],
    source_extents: [ExtentDescriptor; MAX_ACTIVE_SOURCES],
    source_echo_profiles: [EchoProfile; MAX_ACTIVE_SOURCES],
    source_reflection_sends: [bool; MAX_ACTIVE_SOURCES],
    source_pathing_sends: [bool; MAX_ACTIVE_SOURCES],
    baked_path_available: [Option<bool>; MAX_ACTIVE_SOURCES],
    source_reflection_update_divisors: [u8; MAX_ACTIVE_SOURCES],
    source_reflection_share_radii: [f32; MAX_ACTIVE_SOURCES],
    source_reflection_capacities: [i32; MAX_ACTIVE_SOURCES],
    reflection_budget_plan: Option<ReflectionBudgetPlan>,
    reflection_forced_due: [bool; MAX_ACTIVE_SOURCES],
    reflection_worker: Option<ReflectionWorker>,
    reflection_worker_busy: bool,
    reflection_min_interval_ns: u64,
    reflection_revisions: [u64; MAX_ACTIVE_SOURCES],
    frame: SimulationFrame,
    valid_update: bool,
    snapshot: SteamPropagationSnapshot,
    publication: fightbox_runtime::SnapshotWriter<SteamPropagationSnapshot>,
    governor: QualityGovernor,
    reflection_cadence_tick: u64,
    pass_cadences: [SimulationPassCadence; 3],
    last_direct_frame: Option<SimulationFrame>,
    path_gates: [PathGateState; MAX_ACTIVE_SOURCES],
    /// Per-source single-entry memo for the analytic megablock echo plan.
    echo_plan_cache: [Option<EchoPlanMemo>; MAX_ACTIVE_SOURCES],
    roof_caches: [crate::over_roof::RoofCache; MAX_ACTIVE_SOURCES],
    /// Absent for the immutable legacy graph. The neutral builder installs a
    /// shape-derived mask so guaranteed-unused stereo indirect work is never
    /// submitted to Steam's simulation workers.
    neutral_indirect_policy: Option<NeutralSimulationIndirectPolicy>,
    work_counters: SimulationWorkCounters,
    started: Instant,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SimulationPassCadence {
    last_started_ns: Option<u64>,
    last_target_interval_ns: Option<u64>,
}

impl SimulationPassCadence {
    fn observe_target_interval(&mut self, target_interval_ns: u64) {
        if self.last_target_interval_ns != Some(target_interval_ns) {
            // Quality transitions can change a reflection lane's cadence on
            // an invocation that deliberately skips the vendor pass. Clear
            // the prior start here so a later return to the same cadence does
            // not turn that policy-owned gap into false scheduler lateness.
            self.last_started_ns = None;
            self.last_target_interval_ns = Some(target_interval_ns);
        }
    }

    fn observe_start(&mut self, started_ns: u64, target_interval_ns: u64) -> u64 {
        self.observe_target_interval(target_interval_ns);
        let lateness_ns = match (self.last_started_ns, self.last_target_interval_ns) {
            (Some(previous_started_ns), Some(previous_target_interval_ns))
                if previous_target_interval_ns == target_interval_ns =>
            {
                started_ns
                    .saturating_sub(previous_started_ns)
                    .saturating_sub(target_interval_ns)
            }
            _ => 0,
        };
        self.last_started_ns = Some(started_ns);
        self.last_target_interval_ns = Some(target_interval_ns);
        lateness_ns
    }
}

impl MultiSourceSimulation {
    pub(crate) fn enable_reflection_worker(&mut self, minimum_interval_ns: u64) -> Result<(), SimulationError> {
        if minimum_interval_ns == 0 { return Err(SimulationError::InvalidUpdate); }
        if reflection_effect_uses_ir(self.config.reflection_effect.effect_type)
            && self.world.reflection_worker_hold_ir.load(Ordering::Relaxed) == 0
        {
            let inert = (0..self.world.source_count).find(|index| !self.source_reflection_sends[*index]
                && self.world.source_simulation_flags[*index] & ffi::IPL_SIMULATIONFLAGS_REFLECTIONS != 0);
            let source = if let Some(index) = inert {
                self.world.source(index)
            } else {
                let mut source = core::ptr::null_mut();
                let mut settings = ffi::IPLSourceSettings { flags: ffi::IPL_SIMULATIONFLAGS_REFLECTIONS };
                if ffi::source_create(self.world.simulator(), &mut settings, &mut source) != ffi::IPL_STATUS_SUCCESS {
                    return Err(SimulationError::KernelFailure);
                }
                // Never add this source: its mailbox cannot receive an IR.
                self.world.reflection_worker_hold_source.store(source as usize, Ordering::Relaxed);
                source
            };
            let mut outputs = ffi::IPLSimulationOutputs::zeroed();
            ffi::source_get_outputs(source, ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut outputs);
            if outputs.reflections.ir.is_null() { return Err(SimulationError::KernelFailure); }
            self.world.reflection_worker_hold_ir.store(outputs.reflections.ir as usize, Ordering::Relaxed);
        }
        if self.reflection_worker.is_none() {
            self.reflection_worker = Some(ReflectionWorker::new(Arc::clone(&self.world), self.audio)?);
        }
        self.world.reflection_worker_enabled.store(true, Ordering::Release);
        self.reflection_min_interval_ns = minimum_interval_ns;
        Ok(())
    }

    pub(crate) fn reflection_worker_interval(&self) -> Option<u64> {
        self.reflection_worker.as_ref().map(|_| self.reflection_min_interval_ns)
    }

    pub(crate) fn audio_config(&self) -> AudioConfig {
        self.audio
    }

    pub(crate) fn source_count(&self) -> usize {
        self.world.source_count
    }

    /// Correlates the latest fully published direct state with the separate
    /// Runtime activity snapshot. Optional later passes never change it.
    #[cfg(test)]
    pub(crate) fn roof_evidence_snapshot(&self) -> SteamPropagationSnapshot { self.snapshot }

    pub(crate) const fn latest_direct_sequence(&self) -> u64 {
        self.snapshot.direct_sequence
    }

    /// Adopts the incumbent generation's logical direct-publication sequence
    /// before a prepared world enters a synchronized two-world handoff.
    /// Control-thread only; no SDK work or allocation is performed.
    pub(crate) fn align_direct_sequence_for_world_swap(&mut self, sequence: u64) {
        self.snapshot.direct_sequence = sequence;
        self.publication.publish(self.snapshot);
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
            vendor_pass_runs: self.work_counters.vendor_pass_runs,
            skipped_vendor_passes: self.work_counters.skipped_vendor_passes,
            source_output_queries: self.work_counters.source_output_queries,
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

    pub(crate) fn pin_replay_full_quality(&mut self) {
        self.governor.pin_replay_full_quality();
    }

    pub(crate) fn observe_render_timing(&mut self, elapsed_ns: u64) {
        self.governor.observe_block_timing(elapsed_ns);
    }

    pub(crate) fn observe_simulation_lateness(
        &mut self,
        pass: GovernorSimulationPass,
        lateness_ns: u64,
    ) {
        if pass == GovernorSimulationPass::Reflections && self.reflection_worker.is_some() {
            // This is queue-start spill on the direct/path worker, not work
            // performed by the separate reflection lane.
            self.governor.observe_simulation_interval_lateness(pass, lateness_ns,
                self.reflection_min_interval_ns);
        } else {
            self.governor.observe_simulation_lateness(pass, lateness_ns);
        }
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
        let listener_teleported =
            endpoint_teleported(self.frame.listener.position, listener.position);
        let mut path_invalidated = false;
        for index in 0..self.world.source_count {
            let source_teleported =
                endpoint_teleported(self.frame.sources[index].position, sources[index].position);
            let activation_changed = self.frame.active[index] != active[index];
            let throttled_geometry_changed = self.source_reflection_update_divisors[index] > 1
                && (self.frame.listener != listener || self.frame.sources[index] != sources[index]);
            if listener_teleported || source_teleported || activation_changed || throttled_geometry_changed {
                self.reflection_forced_due[index] = true;
                self.reflection_revisions[index] = self.reflection_revisions[index].wrapping_add(1);
            }
            if !self.frame.active[index]
                && active[index]
                && self.governor.telemetry().sources[index].priority_class
                    == crate::SourcePriorityClass::TransientEvent
            {
                // `begin_source_transient` rejects ordinary steady slots, so
                // only descriptors pre-classified as transient may reach this
                // path. Re-assert the class at the activation seam, beside the
                // coherent pose/active update, then arm its existing window.
                self.governor
                    .set_source_priority(index, crate::SourcePriorityClass::TransientEvent);
                self.governor.begin_source_transient(index);
            }
            if listener_teleported || source_teleported || activation_changed {
                self.path_gates[index].invalidate(&mut self.snapshot.sources[index]);
                path_invalidated = true;
            }
        }
        if path_invalidated {
            // Publish the zero target before a potentially blocking SDK pass.
            // Endpoint poses and acoustic results remain the preceding coherent
            // simulation frame until that pass publishes their replacements.
            self.snapshot.sequence = self.snapshot.sequence.wrapping_add(1);
            self.publication.publish(self.snapshot);
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

    /// Query-only listener update that preserves the session's immutable source.
    pub(crate) fn update_listener(&mut self, position: ApiEnuVector3) {
        let Some(listener) = SteamPose::from_api(default_api_pose(position)) else {
            self.valid_update = false;
            return;
        };
        let teleported = endpoint_teleported(self.frame.listener.position, listener.position);
        if !teleported && self.frame.listener != listener {
            for index in 0..self.world.source_count {
                if self.source_reflection_update_divisors[index] > 1 {
                    self.reflection_forced_due[index] = true;
                    self.reflection_revisions[index] = self.reflection_revisions[index].wrapping_add(1);
                }
            }
        }
        if teleported {
            for index in 0..self.world.source_count {
                self.path_gates[index].invalidate(&mut self.snapshot.sources[index]);
                self.reflection_forced_due[index] = true;
                self.reflection_revisions[index] = self.reflection_revisions[index].wrapping_add(1);
            }
            self.snapshot.sequence = self.snapshot.sequence.wrapping_add(1);
            self.publication.publish(self.snapshot);
        }
        self.frame.listener = listener;
        self.frame.listener_linear_velocity_mps = SteamVector3::default();
        self.valid_update = true;
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.poll_reflection_worker()?;
        self.run_pass(
            ffi::IPL_SIMULATIONFLAGS_DIRECT,
            GovernorSimulationPass::Direct,
        )
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.poll_reflection_worker()?;
        let has_enrolled_path_source = self.neutral_indirect_policy.is_none_or(|policy| {
            policy.pathing[..self.world.source_count]
                .iter()
                .copied()
                .any(core::convert::identity)
        });
        if !self.world.has_baked_pathing && has_enrolled_path_source {
            return Err(SimulationError::KernelFailure);
        }
        self.run_pass(
            ffi::IPL_SIMULATIONFLAGS_PATHING,
            GovernorSimulationPass::Pathing,
        )
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        if self.reflection_worker.is_some() {
            return self.schedule_reflections();
        }
        if self.reflection_budget_plan.is_some() {
            return self.run_budgeted_reflections(true, false);
        }
        let cadence_divisor = u64::from(self.governor.render_quality().reflections.cadence_divisor);
        let target_interval_ns = (1_000_000_000 / 5) * cadence_divisor;
        self.pass_cadences[GovernorSimulationPass::Reflections.index()]
            .observe_target_interval(target_interval_ns);
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

    /// Runs one actual reflection pass for control-thread real-time
    /// preparation, regardless of the ordinary cadence divisor.
    ///
    /// The scheduler tick is deliberately unchanged so this one-time forced
    /// pass does not consume or shift the normal reflection cadence.
    pub(crate) fn run_reflections_for_realtime_prepare(&mut self) -> Result<(), SimulationError> {
        // This explicit control-side barrier may wait; the audio callback never
        // calls it. No reflection input/output may overlap an in-flight pass.
        while self.reflection_worker_busy {
            self.poll_reflection_worker()?;
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        if self.world.reflection_worker_enabled.load(Ordering::Acquire)
            && (0..self.world.source_count).any(|index| !self.reflection_publication_acknowledged(index))
        {
            return Ok(());
        }
        if self.reflection_budget_plan.is_some() {
            return self.run_budgeted_reflections(false, true);
        }
        self.run_pass_with_accounting(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
            false,
        )
    }

    /// Publishes exact current direct, pathing, and reflection truth for the
    /// one-time control-thread graph preparation barrier.
    ///
    /// These forced passes retain direct audibility/ranking updates but do not
    /// consume ordinary cadence ticks or contribute scheduling lateness and
    /// overrun evidence to the adaptive governor.
    pub(crate) fn prepare_simulation_for_realtime(
        &mut self,
        update: &SimulationUpdate,
    ) -> Result<(), SimulationError> {
        let interval = self.reflection_worker_interval();
        // Explicit control-side preparation excludes both run lanes. Dropping
        // the sole job sender and joining also drains a pending native pass.
        while self.reflection_worker_busy {
            self.poll_reflection_worker()?;
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
        drop(self.reflection_worker.take());
        self.reflection_worker_busy = false;
        let result = self.prepare_simulation_with_worker_stopped(update);
        if let Some(interval_ns) = interval {
            result.and(self.enable_reflection_worker(interval_ns))
        } else {
            result
        }
    }

    fn prepare_simulation_with_worker_stopped(
        &mut self,
        update: &SimulationUpdate,
    ) -> Result<(), SimulationError> {
        self.update_inputs(update);
        self.run_pass_with_accounting(
            ffi::IPL_SIMULATIONFLAGS_DIRECT,
            GovernorSimulationPass::Direct,
            false,
        )?;
        let has_enrolled_path_source = self.neutral_indirect_policy.is_none_or(|policy| {
            policy.pathing[..self.world.source_count]
                .iter()
                .copied()
                .any(core::convert::identity)
        });
        if !self.world.has_baked_pathing && has_enrolled_path_source {
            return Err(SimulationError::KernelFailure);
        }
        self.run_pass_with_accounting(
            ffi::IPL_SIMULATIONFLAGS_PATHING,
            GovernorSimulationPass::Pathing,
            false,
        )?;
        self.run_reflections_for_realtime_prepare()
    }

    fn run_pass(&mut self, flag: i32, pass: GovernorSimulationPass) -> Result<(), SimulationError> {
        self.run_pass_with_accounting(flag, pass, true)
    }

    fn poll_reflection_worker(&mut self) -> Result<(), SimulationError> {
        if !self.reflection_worker_busy { return Ok(()); }
        let polled = self.reflection_worker.as_ref().expect("busy worker exists").poll();
        if polled.is_err() { self.reflection_worker_busy = false; }
        let Some(completion) = polled? else {
            return Ok(());
        };
        self.publish_reflection_completion(completion);
        Ok(())
    }

    fn publish_reflection_completion(&mut self, completion: ReflectionCompletion) {
        self.reflection_worker_busy = false;
        let pass = GovernorSimulationPass::Reflections;
        self.governor.observe_simulation_work(pass, completion.elapsed_ns,
            completion.interval_ns, completion.quality.reflections);
        if completion.quality.reflections == self.governor.render_quality().reflections {
            self.governor.observe_simulation_pass_overrun(pass,
                completion.elapsed_ns.saturating_sub(completion.interval_ns));
        }
        self.work_counters.vendor_pass_runs[pass.index()] = self.work_counters.vendor_pass_runs[pass.index()]
            .saturating_add(completion.vendor_runs);
        self.work_counters.source_output_queries[pass.index()] = self.work_counters.source_output_queries[pass.index()]
            .saturating_add(completion.output_queries);
        let mut changed = false;
        for index in 0..self.world.source_count {
            if let Some(params) = completion.params[index] {
                // Steam's triple_buffer.h:42 skips publication while an IR is
                // unread. Accept even a pre-teleport completion with its own
                // metadata, then force a fresh pass after render acknowledges
                // it; no later job may overtake this native publication.
                self.snapshot.sources[index].reflections = params;
                self.snapshot.sources[index].reflection_sequence =
                    self.snapshot.sources[index].reflection_sequence.wrapping_add(1);
                self.reflection_forced_due[index] =
                    completion.revisions[index] != self.reflection_revisions[index];
                changed = true;
            }
        }
        if changed {
            self.snapshot.sequence = self.snapshot.sequence.wrapping_add(1);
            self.snapshot.simulated_at_ns = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            self.publication.publish(self.snapshot);
        }
    }

    fn reflection_publication_acknowledged(&self, index: usize) -> bool {
        self.world.reflection_acknowledged[index].load(Ordering::Acquire)
            == self.snapshot.sources[index].reflection_sequence
    }

    fn reflection_update_is_due(&self, index: usize, tick: u64, shared_cadence: u8) -> bool {
        let divisor = self.source_reflection_update_divisors[index];
        // The default keeps legacy group-forced piggyback updates intact.
        divisor == 1 || self.reflection_forced_due[index]
            || tick.is_multiple_of(u64::from(shared_cadence.max(1)) * u64::from(divisor))
    }

    fn shared_reflection_targets(&self, quality: GovernorRenderSnapshot) -> [usize; MAX_ACTIVE_SOURCES] {
        if self.neutral_indirect_policy.is_some() {
            return std::array::from_fn(|index| index);
        }
        shared_reflection_targets(
            &std::array::from_fn(|i| self.frame.sources[i].position),
            &self.frame.active, &quality.sources,
            &self.source_reflection_share_radii, &self.source_reflection_capacities,
            self.world.source_count,
        )
    }

    fn schedule_reflections(&mut self) -> Result<(), SimulationError> {
        self.poll_reflection_worker()?;
        if self.reflection_worker_busy { return Ok(()); }
        if !self.valid_update { return Err(SimulationError::InvalidUpdate); }
        self.adopt_scene_air();
        let quality = self.governor.render_quality();
        let shared_targets = self.shared_reflection_targets(quality);
        let mut groups = [ReflectionJobGroup::default(); MAX_REFLECTION_GROUPS];
        let mut group_count = 0;
        let mut minimum_cadence = u8::MAX;
        if let Some(mut plan) = self.reflection_budget_plan {
            for group_index in 0..usize::from(plan.group_count) {
                let delivered = plan.groups[group_index].delivered(plan.quality_tier, quality);
                let tick = plan.groups[group_index].tick;
                plan.groups[group_index].tick = tick.wrapping_add(1);
                let mut enabled = [false; MAX_ACTIVE_SOURCES];
                let mut forced_due = false;
                for (index, enabled) in enabled.iter_mut().enumerate().take(self.world.source_count) {
                    *enabled = plan.source_in_group(index, group_index)
                        && self.reflection_group_source_is_eligible(plan.groups[group_index], index, quality)
                        && self.reflection_publication_acknowledged(index)
                        && self.reflection_update_is_due(index, tick, delivered.cadence_divisor)
                        && shared_targets[index] == index;
                    forced_due |= *enabled && self.reflection_forced_due[index];
                }
                if !enabled.iter().any(|enabled| *enabled)
                    || (!forced_due && !tick.is_multiple_of(u64::from(delivered.cadence_divisor.max(1))))
                { continue; }
                minimum_cadence = minimum_cadence.min(delivered.cadence_divisor.max(1));
                groups[group_count] = ReflectionJobGroup { budget: delivered, enabled };
                group_count += 1;
            }
            self.reflection_budget_plan = Some(plan);
        } else {
            let cadence_divisor = quality.reflections.cadence_divisor.max(1);
            let tick = self.reflection_cadence_tick;
            self.reflection_cadence_tick = tick.wrapping_add(1);
            let mut enabled = [false; MAX_ACTIVE_SOURCES];
            let mut forced_due = false;
            for (index, enabled) in enabled.iter_mut().enumerate().take(self.world.source_count) {
                *enabled = self.source_reflection_sends[index]
                    && self.neutral_indirect_policy.is_none_or(|policy| policy.reflections[index])
                    && self.reflection_publication_acknowledged(index)
                    && self.reflection_update_is_due(index, tick, cadence_divisor)
                    && shared_targets[index] == index;
                forced_due |= *enabled && self.reflection_forced_due[index];
            }
            if !forced_due && !tick.is_multiple_of(u64::from(cadence_divisor)) { return Ok(()); }
            if enabled.iter().any(|enabled| *enabled) {
                groups[0] = ReflectionJobGroup {
                    budget: SourceReflectionBudget::realtime(quality.reflections.rays,
                        quality.reflections.bounces, quality.reflections.ir_duration_s,
                        quality.ambisonic_order, cadence_divisor), enabled,
                };
                group_count = 1;
                minimum_cadence = cadence_divisor;
            }
        }
        if group_count == 0 { return Ok(()); }
        self.reflection_worker.as_ref().expect("worker exists").submit(ReflectionJob {
            groups, group_count, frame: self.frame, quality, config: self.config,
            directivities: self.source_directivities, occlusion_modes: self.source_occlusion_modes,
            revisions: self.reflection_revisions,
            interval_ns: self.reflection_min_interval_ns.saturating_mul(u64::from(minimum_cadence)),
        })?;
        self.reflection_worker_busy = true;
        Ok(())
    }

    fn run_budgeted_reflections(
        &mut self,
        record_schedule_evidence: bool,
        force_all_due: bool,
    ) -> Result<(), SimulationError> {
        if !self.valid_update {
            return Err(SimulationError::InvalidUpdate);
        }
        let mut plan = self
            .reflection_budget_plan
            .expect("budgeted reflection path requires an explicit plan");
        let quality = self.governor.render_quality();
        let pass = GovernorSimulationPass::Reflections;
        let pass_index = pass.index();
        self.adopt_scene_air();
        let pass_started = Instant::now();
        let pass_started_ns = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let minimum_cadence = plan.groups[..usize::from(plan.group_count)]
            .iter()
            .map(|group| {
                delivered_reflection_budget(group.requested, plan.quality_tier, quality)
                    .cadence_divisor
            })
            .min()
            .unwrap_or(1);
        let target_interval_ns = (1_000_000_000 / 5) * u64::from(minimum_cadence);
        self.pass_cadences[pass_index].observe_target_interval(target_interval_ns);
        if record_schedule_evidence {
            let lateness_ns =
                self.pass_cadences[pass_index].observe_start(pass_started_ns, target_interval_ns);
            self.governor.observe_simulation_interval_lateness(
                pass,
                lateness_ns,
                target_interval_ns,
            );
        }

        let mut next_snapshot = self.snapshot;
        let mut ran_group = false;
        let update_flags = ffi::IPL_SIMULATIONFLAGS_REFLECTIONS;
        for group_index in 0..usize::from(plan.group_count) {
            let requested = plan.groups[group_index].requested;
            let delivered = delivered_reflection_budget(requested, plan.quality_tier, quality);
            let tick = plan.groups[group_index].tick;
            if record_schedule_evidence {
                plan.groups[group_index].tick = tick.wrapping_add(1);
            }
            let cadence_due =
                force_all_due || tick.is_multiple_of(u64::from(delivered.cadence_divisor.max(1)));
            let forced_due = (0..self.world.source_count).any(|source_index| {
                plan.source_in_group(source_index, group_index)
                    && self.reflection_forced_due[source_index]
                    && self.reflection_source_is_eligible(source_index, quality)
            });
            if !cadence_due && !forced_due {
                continue;
            }

            let group_has_sources = (0..self.world.source_count).any(|source_index| {
                plan.source_in_group(source_index, group_index)
                    && self.reflection_source_is_eligible(source_index, quality)
            });
            if !group_has_sources {
                continue;
            }

            let mut shared = shared_inputs(self.frame.listener, quality)
                .ok_or(SimulationError::InvalidUpdate)?;
            shared.numRays = delivered.rays;
            shared.numBounces = delivered.bounces;
            shared.duration = delivered.duration_s;
            shared.order = delivered.order;
            ffi::simulator_set_shared_inputs(self.world.simulator(), update_flags, &mut shared);

            for source_index in 0..self.world.source_count {
                let enabled = plan.source_in_group(source_index, group_index)
                    && self.reflection_source_is_eligible(source_index, quality);
                let flags = if enabled {
                    ffi::IPL_SIMULATIONFLAGS_REFLECTIONS
                } else {
                    0
                };
                let mut inputs = source_inputs(
                    self.frame.sources[source_index],
                    self.source_directivities[source_index],
                    self.source_occlusion_modes[source_index],
                    self.world.probe_batch(),
                    self.config,
                    quality,
                    flags,
                )
                .ok_or(SimulationError::InvalidUpdate)?;
                inputs.flags = flags;
                ffi::source_set_inputs(self.world.source(source_index), update_flags, &mut inputs);
            }

            ffi::simulator_run_reflections(self.world.simulator());
            self.work_counters.vendor_pass_runs[pass_index] =
                self.work_counters.vendor_pass_runs[pass_index].saturating_add(1);
            ran_group = true;

            for source_index in 0..self.world.source_count {
                if !plan.source_in_group(source_index, group_index)
                    || !self.reflection_source_is_eligible(source_index, quality)
                {
                    continue;
                }
                let mut outputs = ffi::IPLSimulationOutputs::zeroed();
                ffi::source_get_outputs(
                    self.world.source(source_index),
                    ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
                    &mut outputs,
                );
                self.work_counters.source_output_queries[pass_index] =
                    self.work_counters.source_output_queries[pass_index].saturating_add(1);
                let uses_ir = reflection_effect_uses_ir(self.config.reflection_effect.effect_type);
                let expected_channels = ambisonics_channel_count(delivered.order)
                    .map_err(|_| SimulationError::KernelFailure)?;
                let maximum_ir_size =
                    reflection_ir_size(delivered.duration_s, self.audio.sample_rate_hz)
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
                next_snapshot.sources[source_index].reflections = SteamReflectionParams {
                    ir: outputs.reflections.ir as usize,
                    reverb_times: outputs.reflections.reverbTimes,
                    eq: outputs.reflections.eq,
                    delay: outputs.reflections.delay,
                    num_channels: outputs.reflections.numChannels,
                    ir_size: outputs.reflections.irSize,
                    tan_slot: outputs.reflections.tanSlot,
                };
                next_snapshot.sources[source_index].reflection_sequence = next_snapshot.sources[source_index].reflection_sequence.wrapping_add(1);
                self.reflection_forced_due[source_index] = false;
            }
        }
        self.reflection_budget_plan = Some(plan);

        if ran_group {
            next_snapshot.sequence = next_snapshot.sequence.wrapping_add(1);
            next_snapshot.simulated_at_ns =
                self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            self.snapshot = next_snapshot;
            self.publication.publish(next_snapshot);
        } else {
            self.work_counters.skipped_vendor_passes[pass_index] =
                self.work_counters.skipped_vendor_passes[pass_index].saturating_add(1);
        }
        if record_schedule_evidence {
            let elapsed_ns = pass_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if ran_group {
                self.governor.observe_simulation_work(pass, elapsed_ns, target_interval_ns, quality.reflections);
            }
            self.governor.observe_simulation_pass_overrun(
                pass,
                elapsed_ns.saturating_sub(target_interval_ns),
            );
        }
        Ok(())
    }

    fn reflection_group_source_is_eligible(
        &self, group: ReflectionBudgetGroup, source_index: usize, quality: GovernorRenderSnapshot,
    ) -> bool {
        if group.inherit_shared_quality {
            self.source_reflection_sends[source_index]
                && self.neutral_indirect_policy.is_none_or(|policy| policy.reflections[source_index])
        } else {
            self.reflection_source_is_eligible(source_index, quality)
        }
    }

    fn reflection_source_is_eligible(
        &self,
        source_index: usize,
        quality: GovernorRenderSnapshot,
    ) -> bool {
        self.frame.active[source_index]
            && self.source_reflection_sends[source_index]
            && quality.sources[source_index] == SourceQualityLevel::Full
            && self
                .neutral_indirect_policy
                .is_none_or(|policy| policy.reflections[source_index])
    }

    fn adopt_scene_air(&mut self) {
        let air = self.scene_air.read();
        if air != self.config.air_pressure_exponents_per_m {
            self.config.air_pressure_exponents_per_m = air;
            self.echo_plan_cache = [None; MAX_ACTIVE_SOURCES];
        }
    }

    pub(crate) fn take_scene_air_writer(&mut self) -> Option<fightbox_runtime::SnapshotWriter<[f32; 3]>> {
        self.scene_air_writer.take()
    }

    fn run_pass_with_accounting(
        &mut self,
        flag: i32,
        pass: GovernorSimulationPass,
        record_schedule_evidence: bool,
    ) -> Result<(), SimulationError> {
        if !self.valid_update {
            return Err(SimulationError::InvalidUpdate);
        }
        self.adopt_scene_air();
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
        if record_schedule_evidence {
            let lateness_ns =
                self.pass_cadences[pass_index].observe_start(pass_started_ns, target_interval_ns);
            self.governor.observe_simulation_interval_lateness(
                pass,
                lateness_ns,
                target_interval_ns,
            );
        }
        // Each native pass owns exactly its flag's inputs (phonon.h:4178-4182,
        // 4272-4276); writing the other indirect flag would race its worker.
        let input_flags = flag;
        let vendor_pass_has_sources =
            self.neutral_indirect_policy
                .is_none_or(|policy| match flag {
                    ffi::IPL_SIMULATIONFLAGS_DIRECT => true,
                    ffi::IPL_SIMULATIONFLAGS_PATHING => policy.pathing[..self.world.source_count]
                        .iter()
                        .copied()
                        .any(core::convert::identity),
                    ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => policy.reflections
                        [..self.world.source_count]
                        .iter()
                        .copied()
                        .any(core::convert::identity),
                    _ => false,
                });
        if vendor_pass_has_sources {
            let mut shared = shared_inputs(self.frame.listener, quality)
                .ok_or(SimulationError::InvalidUpdate)?;
            ffi::simulator_set_shared_inputs(self.world.simulator(), input_flags, &mut shared);
            for index in 0..self.world.source_count {
                let mut source_input_flags = input_flags;
                if !self.source_reflection_sends[index] {
                    source_input_flags &= !ffi::IPL_SIMULATIONFLAGS_REFLECTIONS;
                }
                if let Some(policy) = self.neutral_indirect_policy {
                    if !policy.pathing[index] {
                        source_input_flags &= !ffi::IPL_SIMULATIONFLAGS_PATHING;
                    }
                    if !policy.reflections[index] {
                        source_input_flags &= !ffi::IPL_SIMULATIONFLAGS_REFLECTIONS;
                    }
                }
                if source_input_flags == 0 {
                    continue;
                }
                let mut inputs = source_inputs(
                    self.frame.sources[index],
                    self.source_directivities[index],
                    self.source_occlusion_modes[index],
                    self.world.probe_batch(),
                    self.config,
                    quality,
                    source_input_flags,
                )
                .ok_or(SimulationError::InvalidUpdate)?;
                let update_flags = if self.neutral_indirect_policy.is_some() {
                    source_input_flags
                } else {
                    input_flags
                };
                ffi::source_set_inputs(self.world.source(index), update_flags, &mut inputs);
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
            self.work_counters.vendor_pass_runs[pass_index] =
                self.work_counters.vendor_pass_runs[pass_index].saturating_add(1);
        } else {
            self.work_counters.skipped_vendor_passes[pass_index] =
                self.work_counters.skipped_vendor_passes[pass_index].saturating_add(1);
        }
        let result = self.copy_and_publish(flag, quality);
        if record_schedule_evidence {
            let elapsed_ns = pass_started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if vendor_pass_has_sources {
                self.governor.observe_simulation_work(pass, elapsed_ns, target_interval_ns, quality.reflections);
            }
            self.governor.observe_simulation_pass_overrun(
                pass,
                elapsed_ns.saturating_sub(target_interval_ns),
            );
        }
        result
    }

    /// Memoized wrapper around `analytic_megablock_echo_plan`. The enumerator
    /// is a pure function of (context, profile, endpoint positions, direct
    /// occlusion, sample rate); profile and sample rate are fixed per source
    /// slot for this simulation's lifetime, so the memo keys on the context
    /// handle plus exact f32 bit patterns of the remaining inputs. A hit
    /// re-stamps the current publication sequence into the plan header,
    /// reproducing identical plan bytes without re-enumerating 36 specular
    /// candidates per active source per direct pass.
    fn analytic_echo_plan_for_source(
        &mut self,
        index: usize,
        occlusion: f32,
        sequence: u64,
    ) -> EchoSourcePlan {
        let profile = self.source_echo_profiles[index];
        if !profile.is_enabled() {
            return EchoSourcePlan::default();
        }
        let source_position = self.frame.sources[index].position;
        let listener_position = self.frame.listener.position;
        let context = self.world.context;
        let source_bits = [
            source_position.x.to_bits(),
            source_position.y.to_bits(),
            source_position.z.to_bits(),
        ];
        let listener_bits = [
            listener_position.x.to_bits(),
            listener_position.y.to_bits(),
            listener_position.z.to_bits(),
        ];
        let occlusion_bits = occlusion.to_bits();
        if let Some(memo) = &mut self.echo_plan_cache[index] {
            if memo.context == context
                && memo.source_bits == source_bits
                && memo.listener_bits == listener_bits
                && memo.occlusion_bits == occlusion_bits
            {
                memo.plan.generation = sequence;
                return memo.plan;
            }
        }
        let plan = analytic_megablock_echo_plan(
            self.world.context(),
            profile,
            source_position,
            listener_position,
            occlusion,
            self.audio.sample_rate_hz,
            sequence,
            self.config.air_pressure_exponents_per_m,
        );
        self.echo_plan_cache[index] = Some(EchoPlanMemo {
            context,
            source_bits,
            listener_bits,
            occlusion_bits,
            plan,
        });
        plan
    }

    fn copy_and_publish(
        &mut self,
        flag: i32,
        quality: GovernorRenderSnapshot,
    ) -> Result<(), SimulationError> {
        let pass_index = match flag {
            ffi::IPL_SIMULATIONFLAGS_DIRECT => GovernorSimulationPass::Direct.index(),
            ffi::IPL_SIMULATIONFLAGS_PATHING => GovernorSimulationPass::Pathing.index(),
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => GovernorSimulationPass::Reflections.index(),
            _ => return Err(SimulationError::KernelFailure),
        };
        // Stage the complete next publication by value. A rejected SDK output
        // must not leak partially copied direct state through a later optional
        // path/reflection publication under the preceding direct token.
        let mut next_snapshot = self.snapshot;
        next_snapshot.sequence = next_snapshot.sequence.wrapping_add(1);
        next_snapshot.simulated_at_ns =
            self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let direct_pass = flag == ffi::IPL_SIMULATIONFLAGS_DIRECT;
        if direct_pass {
            // The direct token is the coherence boundary for callback-visible
            // motion. Optional passes may publish newer stage payloads under
            // that token, but must not leak a later simulation frame's
            // endpoints into propagation or placement before Direct accepts
            // that frame.
            next_snapshot.listener_position = self.frame.listener.position;
            next_snapshot.listener_linear_velocity_mps = self.frame.listener_linear_velocity_mps;
        }
        let listener_has_probe = flag == ffi::IPL_SIMULATIONFLAGS_PATHING
            && self
                .world
                .has_influencing_probe(self.frame.listener.position);

        for index in 0..self.world.source_count {
            let source_snapshot = &mut next_snapshot.sources[index];
            if direct_pass {
                source_snapshot.active = self.frame.active[index];
                source_snapshot.source_position = self.frame.sources[index].position;
                source_snapshot.source_forward = self.frame.sources[index].forward;
                source_snapshot.source_up = self.frame.sources[index].up;
                source_snapshot.linear_velocity_mps =
                    self.frame.source_linear_velocities_mps[index];
                source_snapshot.width = width_snapshot(
                    self.source_extents[index],
                    self.frame.sources[index],
                    self.frame.listener.position,
                );
            }
            let suppressed = (flag == ffi::IPL_SIMULATIONFLAGS_REFLECTIONS
                && !self.source_reflection_sends[index]) || self
                .neutral_indirect_policy
                .is_some_and(|policy| match flag {
                    ffi::IPL_SIMULATIONFLAGS_DIRECT => false,
                    ffi::IPL_SIMULATIONFLAGS_PATHING => !policy.pathing[index],
                    ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => !policy.reflections[index],
                    _ => false,
                });
            if suppressed {
                match flag {
                    ffi::IPL_SIMULATIONFLAGS_PATHING => {
                        silence_path(source_snapshot);
                        source_snapshot.configured_pathing_order = self.config.pathing_order as u8;
                    }
                    ffi::IPL_SIMULATIONFLAGS_REFLECTIONS => {
                        source_snapshot.reflections = SteamReflectionParams::default();
                    }
                    _ => {}
                }
                continue;
            }
            let mut outputs = ffi::IPLSimulationOutputs::zeroed();
            ffi::source_get_outputs(self.world.source(index), flag, &mut outputs);
            self.work_counters.source_output_queries[pass_index] =
                self.work_counters.source_output_queries[pass_index].saturating_add(1);
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
                    let enrolled = self.source_pathing_sends[index]
                        && self.neutral_indirect_policy.is_none_or(|p| p.pathing[index])
                        && self.world.source_simulation_flags[index] & ffi::IPL_SIMULATIONFLAGS_PATHING != 0;
                    let baked_owns = enrolled && self.world.has_baked_pathing
                        && self.baked_path_available[index].unwrap_or(true)
                        && self.world.has_influencing_probe(self.frame.listener.position)
                        && self.world.has_influencing_probe(self.frame.sources[index].position);
                    source_snapshot.over_roof = self.roof_caches[index].update(
                        &self.world.roof_profile, self.frame.sources[index].position, self.frame.listener.position,
                        direct, self.config.air_pressure_exponents_per_m, baked_owns,
                    );
                    if source_snapshot.over_roof.active {
                        silence_path(source_snapshot);
                    }
                    source_snapshot.echo = self.analytic_echo_plan_for_source(
                        index,
                        direct.occlusion,
                        next_snapshot.sequence,
                    );
                    self.governor
                        .observe_source_gain(index, predicted_direct_gain(source_snapshot.over_roof.direct(direct)));
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
                    // probe spheres. Brief coverage misses retain the last
                    // valid target; the third consecutive miss publishes
                    // silence for the existing propagation smoother to fade.
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
                        let path_sh = fixed_path_sh(self.config.pathing_order, &copied)
                            .map_err(|_| SimulationError::KernelFailure)?;
                        let straight_m = smoothed_source_distance_m(
                            self.frame.sources[index].position,
                            self.frame.listener.position,
                        );
                        let path_eq = self.path_gates[index].route_air.voice(
                            outputs.pathing.eqCoeffs,
                            path_sh[0],
                            straight_m,
                            self.config.air_pressure_exponents_per_m,
                        );
                        self.path_gates[index].resolve(source_snapshot, path_eq, path_sh);
                        self.baked_path_available[index] = Some(path_sh.iter().any(|v| v.abs() > 1.0e-12)
                            && path_eq.iter().any(|v| *v > 1.0e-12));
                    } else {
                        self.path_gates[index].miss(source_snapshot);
                        self.baked_path_available[index] = Some(false);
                    }
                    // Continue observing vendor route availability while roof
                    // transport owns the direct epoch, but never render both.
                    if source_snapshot.over_roof.active { silence_path(source_snapshot); }
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
                    source_snapshot.reflection_sequence = source_snapshot.reflection_sequence.wrapping_add(1);
                }
                _ => return Err(SimulationError::KernelFailure),
            }
        }
        if flag == ffi::IPL_SIMULATIONFLAGS_DIRECT {
            self.last_direct_frame = Some(self.frame);
            // Every source gain has now been observed for one coherent frame.
            // Exchange the fixed detailed slots once, after the complete
            // ranking is known; never let per-source observation order decide
            // who receives expensive reflection detail.
            self.governor.rebalance_detailed_sources();
            next_snapshot.direct_sequence = next_snapshot.direct_sequence.wrapping_add(1);
        }
        self.snapshot = next_snapshot;
        self.publication.publish(next_snapshot);
        Ok(())
    }
}

fn same_position(left: SteamVector3, right: SteamVector3) -> bool {
    left.x.to_bits() == right.x.to_bits()
        && left.y.to_bits() == right.y.to_bits()
        && left.z.to_bits() == right.z.to_bits()
}

fn width_snapshot(
    descriptor: ExtentDescriptor,
    source: SteamPose,
    listener_position: SteamVector3,
) -> SteamWidthState {
    let (geometric_k, phi_eff_radians) = match descriptor {
        ExtentDescriptor::LineSegment { length_m } => {
            let geometry =
                line_geometry(source.position, source.forward, listener_position, length_m);
            (geometry.k, geometry.phi_eff_radians)
        }
        // Wave 11 v1 is deliberately LineSegment-only. Keep the descriptor
        // visible while proving every other kind remains on the point path.
        _ => (0.0, 0.0),
    };
    SteamWidthState {
        descriptor,
        geometric_k,
        phi_eff_radians,
        declared_latency_samples: DECLARED_LATENCY_SAMPLES,
        renderer_revision: WIDTH_RENDERER_REVISION,
    }
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

fn relative_speed_mps(
    source_velocity_mps: SteamVector3,
    listener_velocity_mps: SteamVector3,
) -> f32 {
    let x = f64::from(source_velocity_mps.x) - f64::from(listener_velocity_mps.x);
    let y = f64::from(source_velocity_mps.y) - f64::from(listener_velocity_mps.y);
    let z = f64::from(source_velocity_mps.z) - f64::from(listener_velocity_mps.z);
    let speed = (x * x + y * y + z * z).sqrt();
    if speed.is_finite() {
        speed.min(f64::from(f32::MAX)) as f32
    } else {
        0.0
    }
}

type PropagationObservationKey = (u64, u32, u32, u32);

fn propagation_observation_key(
    direct_sequence: u64,
    delay_target_samples: f32,
    radial_velocity_mps: f32,
    relative_speed_mps: f32,
) -> PropagationObservationKey {
    (
        direct_sequence,
        delay_target_samples.to_bits(),
        radial_velocity_mps.to_bits(),
        relative_speed_mps.to_bits(),
    )
}

fn propagation_observation_cache_miss(
    cached: Option<PropagationObservationKey>,
    observed: PropagationObservationKey,
) -> bool {
    cached != Some(observed)
}

fn smoothed_source_distance_m(
    source_position: SteamVector3,
    listener_position: SteamVector3,
) -> f32 {
    let x = source_position.x - listener_position.x;
    let y = source_position.y - listener_position.y;
    let z = source_position.z - listener_position.z;
    let distance = (x * x + y * y + z * z).sqrt();
    if distance.is_finite() { distance } else { 0.0 }
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

const ANALYTIC_MEGABLOCK_PITCH_M: f32 = 95.0;
const ANALYTIC_MEGABLOCK_BLOCK_MIN_M: f32 = 15.0;
const ANALYTIC_MEGABLOCK_FACADE_INSET_M: f32 = 3.75;
const ANALYTIC_MEGABLOCK_BLOCK_SIZE_M: f32 = 80.0;
const ANALYTIC_MEGABLOCK_BLOCKS: usize = 6;
const MIN_SPECULAR_EXCESS_SECONDS: f32 = 0.3;
const MAX_SPECULAR_EXCESS_SECONDS: f32 = 1.2;
const MASONRY_ABSORPTION: [f32; 3] = [0.03, 0.05, 0.07];
const MASONRY_SCATTERING: f32 = 0.1;

/// Single-entry per-source memo for the analytic megablock echo plan. Keyed
/// on the exact bit patterns of every plan-determining input except the
/// publication sequence, which lands verbatim in the plan header, so a hit
/// reproduces identical plan bytes.
#[derive(Clone, Copy)]
struct EchoPlanMemo {
    context: usize,
    source_bits: [u32; 3],
    listener_bits: [u32; 3],
    occlusion_bits: u32,
    plan: EchoSourcePlan,
}

/// Audible-v1 oracle for the deterministic 6x6 synth grid. Production tables
/// remain an offline-baker concern; this fixed grid enumerator runs only in the
/// control-side direct pass and publishes a complete plan for onset adoption.
fn analytic_megablock_echo_plan(
    context: ffi::IPLContext,
    profile: EchoProfile,
    source_steam: SteamVector3,
    listener_steam: SteamVector3,
    direct_occlusion: f32,
    sample_rate_hz: i32,
    generation: u64,
    exponents: [f32; 3],
) -> EchoSourcePlan {
    if !profile.is_enabled() {
        return EchoSourcePlan::default();
    }
    let source = steam_vector_to_api(source_steam);
    let listener = steam_vector_to_api(listener_steam);
    let direct_distance = api_distance(source, listener);
    if !direct_distance.is_finite() {
        return EchoSourcePlan::default();
    }

    let mut plan = EchoSourcePlan {
        generation,
        ..EchoSourcePlan::default()
    };
    for axis in 0..2 {
        for block in 0..ANALYTIC_MEGABLOCK_BLOCKS {
            let block_min =
                ANALYTIC_MEGABLOCK_BLOCK_MIN_M + block as f32 * ANALYTIC_MEGABLOCK_PITCH_M;
            let planes = [
                block_min + ANALYTIC_MEGABLOCK_FACADE_INSET_M,
                block_min + ANALYTIC_MEGABLOCK_BLOCK_SIZE_M - ANALYTIC_MEGABLOCK_FACADE_INSET_M,
            ];
            for (side, plane) in planes.into_iter().enumerate() {
                let source_axis = if axis == 0 {
                    source.east_m
                } else {
                    source.north_m
                };
                let listener_axis = if axis == 0 {
                    listener.east_m
                } else {
                    listener.north_m
                };
                // Both endpoints must occupy the same half-space. Opposite
                // sides would be transmission through the authored facade,
                // not an image-source return.
                if (source_axis - plane) * (listener_axis - plane) <= 0.0 {
                    continue;
                }
                let mut image = source;
                if axis == 0 {
                    image.east_m = 2.0 * plane - source.east_m;
                } else {
                    image.north_m = 2.0 * plane - source.north_m;
                }
                let total_distance = api_distance(image, listener);
                let excess_seconds =
                    (total_distance - direct_distance) / SPEED_OF_SOUND_METERS_PER_SECOND;
                if !(MIN_SPECULAR_EXCESS_SECONDS..=MAX_SPECULAR_EXCESS_SECONDS)
                    .contains(&excess_seconds)
                    || total_distance > crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS
                {
                    continue;
                }
                let denominator = if axis == 0 {
                    image.east_m - listener.east_m
                } else {
                    image.north_m - listener.north_m
                };
                if denominator.abs() <= 1.0e-6 {
                    continue;
                }
                let t = (plane - listener_axis) / denominator;
                if !(0.0..=1.0).contains(&t) {
                    continue;
                }
                let bounce = ApiEnuVector3::new(
                    listener.east_m + (image.east_m - listener.east_m) * t,
                    listener.north_m + (image.north_m - listener.north_m) * t,
                    listener.up_m + (image.up_m - listener.up_m) * t,
                );
                let scatter_pressure = (1.0 - MASONRY_SCATTERING).sqrt();
                let material = MASONRY_ABSORPTION
                    .map(|absorption| (1.0 - absorption).sqrt() * scatter_pressure);
                insert_specular_candidate(
                    &mut plan,
                    echo_tap_for_path(
                        context,
                        EchoPathKind::Specular,
                        1 + (axis * 32 + block * 2 + side) as u32,
                        total_distance,
                        total_distance,
                        api_enu_to_steam(bounce),
                        material,
                        sample_rate_hz,
                        exponents,
                    ),
                );
            }
        }
    }

    // Steam's direct occlusion convention is 1 = clear and 0 = blocked.
    if direct_occlusion < 0.5 {
        let corners = [
            ApiEnuVector3::new(source.east_m, listener.north_m, source.up_m),
            ApiEnuVector3::new(listener.east_m, source.north_m, source.up_m),
        ];
        let (edge_index, edge, total_distance) = corners
            .into_iter()
            .enumerate()
            .map(|(index, edge)| {
                let distance = api_distance(source, edge) + api_distance(edge, listener);
                (index, edge, distance)
            })
            .min_by(|left, right| {
                left.2
                    .total_cmp(&right.2)
                    .then_with(|| left.0.cmp(&right.0))
            })
            .expect("two analytic corner candidates");
        if total_distance.is_finite()
            && total_distance > 0.0
            && total_distance <= crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS
        {
            reserve_corner_tap(
                &mut plan,
                echo_tap_for_path(
                    context,
                    EchoPathKind::Diffraction,
                    0x8000_0000 | edge_index as u32,
                    total_distance,
                    total_distance,
                    api_enu_to_steam(edge),
                    [1.0; 3],
                    sample_rate_hz,
                    exponents,
                ),
            );
        }
    }
    plan
}

/// The one geometric-path to render-tap law, shared by the analytic oracle and
/// host-published plans. Spherical spreading and Steam air absorption apply
/// once over the total traveled path (never per leg); surface pressure applies
/// once per interaction product; a diffraction path also takes the ratified
/// corner losses. Render-time impulse shaping is keyed by the same physical
/// length. Only the delay uses `delay_distance`, which equals the physical
/// length unless the host re-references timing to the rendered primary.
#[allow(clippy::too_many_arguments)]
fn echo_tap_for_path(
    context: ffi::IPLContext,
    kind: EchoPathKind,
    stable_path_id: u32,
    total_distance: f32,
    delay_distance: f32,
    arrival_position: SteamVector3,
    interaction_pressure: [f32; 3],
    sample_rate_hz: i32,
    exponents: [f32; 3],
) -> EchoTapPlan {
    let (distance_gain, air) = path_attenuation(context, total_distance, exponents);
    let interaction = path_interaction(kind, interaction_pressure);
    let bands = std::array::from_fn(|index| air[index] * interaction[index]);
    let score = distance_gain * bands.into_iter().sum::<f32>() / 3.0;
    EchoTapPlan {
        valid: true,
        kind,
        stable_path_id,
        total_path_distance_m: total_distance,
        delay_samples: delay_distance / SPEED_OF_SOUND_METERS_PER_SECOND * sample_rate_hz as f32,
        arrival_position,
        distance_gain,
        band_gain: bands,
        score,
        inherits_primary: false,
        primary_relative_gain: [0.0; 3],
    }
}

/// Surface pressure product, plus the ratified corner voicing on a
/// diffraction path.
fn path_interaction(kind: EchoPathKind, interaction_pressure: [f32; 3]) -> [f32; 3] {
    match kind {
        EchoPathKind::Specular => interaction_pressure,
        EchoPathKind::Diffraction => {
            let losses_db = [CORNER_LOSS_DB_LOW, CORNER_LOSS_DB_MID, CORNER_LOSS_DB_HIGH];
            std::array::from_fn(|index| {
                interaction_pressure[index] * 10.0_f32.powf(losses_db[index] / 20.0)
            })
        }
    }
}

/// The routed primary's actual Steam transfer at one block: the path SH
/// omni gain, which carries Steam's distance term, and the path EQ.
#[derive(Clone, Copy, Debug, PartialEq)]
struct PrimaryTransfer {
    broadband: f32,
    bands: [f32; 3],
}

/// `None` when the baked path carries nothing yet (no path, a miss, or
/// pathing disabled for the source).
fn primary_path_transfer(path_eq: [f32; 3], path_sh0: f32) -> Option<PrimaryTransfer> {
    let broadband = path_sh0 / PATH_SH_Y00;
    let peak = path_eq.into_iter().fold(0.0_f32, f32::max);
    let valid = broadband.is_finite()
        && broadband > 1.0e-9
        && peak.is_finite()
        && peak > 0.0
        && path_eq.iter().all(|gain| *gain >= 0.0);
    // Keep every EQ band at or below unity; the product is unchanged.
    let scale = peak.max(1.0);
    valid.then(|| PrimaryTransfer {
        broadband: broadband * scale,
        bands: path_eq.map(|gain| gain / scale),
    })
}

/// A tap's direct-effect distance and band gains: the routed primary's
/// transfer times the tap's relative gain when both exist, else the
/// free-field law.
fn echo_tap_gains(tap: &EchoTapPlan, primary: Option<PrimaryTransfer>) -> (f32, [f32; 3]) {
    match primary {
        Some(primary) if tap.inherits_primary => (
            primary.broadband,
            std::array::from_fn(|band| primary.bands[band] * tap.primary_relative_gain[band]),
        ),
        _ => (tap.distance_gain, tap.band_gain),
    }
}

/// Converts host-planned path geometry into one plan: the strongest taps by
/// predicted pressure, any kind, with no reserved slot. The host already
/// excludes transport that baked pathing renders (e.g. a bare corner path).
///
/// Behind a routed primary each tap also carries its level over that primary,
/// `(L_p / L_e) · air(L_e − L_p) · interactions` per band, so the render
/// applies distance, air, and every corner voicing exactly once: the shared
/// route's through the primary's rendered transfer, the rest here.
fn external_echo_plan(
    context: ffi::IPLContext,
    primary: EchoPrimary,
    paths: &[crate::EchoPathGeometry],
    sample_rate_hz: i32,
    generation: u64,
    exponents: [f32; 3],
) -> Result<EchoSourcePlan, EchoPlanError> {
    let primary_length = match primary {
        EchoPrimary::LineOfSight => None,
        EchoPrimary::Routed { path_length_m } => {
            if !(path_length_m.is_finite() && path_length_m > 0.0) {
                return Err(EchoPlanError::InvalidPath);
            }
            Some(path_length_m)
        }
    };
    let mut plan = EchoSourcePlan {
        generation,
        ..EchoSourcePlan::default()
    };
    for path in paths {
        let length = path.physical_path_length_m;
        let delay_length = path.render_delay_path_m;
        let valid = length.is_finite()
            && length > 0.0
            && delay_length.is_finite()
            && delay_length > 0.0
            && delay_length <= crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS
            && path.arrival_position_enu.is_finite()
            && path
                .band_pressure_gain
                .iter()
                .all(|gain| (0.0..=1.0).contains(gain))
            && primary_length.is_none_or(|primary| length >= primary);
        if !valid {
            return Err(EchoPlanError::InvalidPath);
        }
        let mut tap = echo_tap_for_path(
            context,
            path.kind,
            path.stable_path_id,
            length,
            delay_length,
            api_enu_to_steam(path.arrival_position_enu),
            path.band_pressure_gain,
            sample_rate_hz,
            exponents,
        );
        if let Some(primary_length) = primary_length {
            let (_, excess_air) = path_attenuation(context, length - primary_length, exponents);
            let interaction = path_interaction(path.kind, path.band_pressure_gain);
            tap.inherits_primary = true;
            tap.primary_relative_gain = std::array::from_fn(|band| {
                primary_length / length * excess_air[band] * interaction[band]
            });
        }
        insert_ranked_candidate(&mut plan, tap, MAX_ECHO_TAPS_PER_SOURCE);
    }
    Ok(plan)
}

/// Puts the NLOS corner tap first, ahead of at most three specular taps, so
/// every governor rung's delivered prefix retains it.
fn reserve_corner_tap(plan: &mut EchoSourcePlan, corner: EchoTapPlan) {
    let specular_count = usize::from(plan.tap_count).min(3);
    for index in (1..=specular_count).rev() {
        plan.taps[index] = plan.taps[index - 1];
    }
    plan.taps[0] = corner;
    plan.tap_count = (specular_count + 1) as u8;
}

fn echo_tap_precedes(candidate: EchoTapPlan, current: EchoTapPlan) -> bool {
    candidate
        .score
        .total_cmp(&current.score)
        .reverse()
        .then_with(|| {
            candidate
                .total_path_distance_m
                .total_cmp(&current.total_path_distance_m)
        })
        .then_with(|| candidate.stable_path_id.cmp(&current.stable_path_id))
        .is_lt()
}

fn insert_specular_candidate(plan: &mut EchoSourcePlan, candidate: EchoTapPlan) {
    insert_ranked_candidate(plan, candidate, 3);
}

/// Keeps the `capacity` strongest taps in ranked order.
fn insert_ranked_candidate(plan: &mut EchoSourcePlan, candidate: EchoTapPlan, capacity: usize) {
    let mut insert_at = usize::from(plan.tap_count).min(capacity);
    for index in 0..usize::from(plan.tap_count).min(capacity) {
        if echo_tap_precedes(candidate, plan.taps[index]) {
            insert_at = index;
            break;
        }
    }
    if insert_at >= capacity {
        return;
    }
    let old_count = usize::from(plan.tap_count).min(capacity);
    for index in (insert_at + 1..=old_count.min(capacity - 1)).rev() {
        plan.taps[index] = plan.taps[index - 1];
    }
    plan.taps[insert_at] = candidate;
    plan.tap_count = (old_count + 1).min(capacity) as u8;
}

fn path_attenuation(context: ffi::IPLContext, distance_m: f32, exponents: [f32; 3]) -> (f32, [f32; 3]) {
    let source = ffi::IPLVector3 {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let listener = ffi::IPLVector3 {
        x: distance_m,
        y: 0.0,
        z: 0.0,
    };
    let mut distance_model = default_distance_model();
    let mut air_model = iso_air_absorption_model(exponents);
    (
        ffi::distance_attenuation_calculate(context, source, listener, &mut distance_model),
        ffi::air_absorption_calculate(context, source, listener, &mut air_model),
    )
}

fn api_distance(left: ApiEnuVector3, right: ApiEnuVector3) -> f32 {
    let east = left.east_m - right.east_m;
    let north = left.north_m - right.north_m;
    let up = left.up_m - right.up_m;
    (east * east + north * north + up * up).sqrt()
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

fn delivered_reflection_budget(
    requested: SourceReflectionBudget,
    quality_tier: QualityTier,
    quality: GovernorRenderSnapshot,
) -> SourceReflectionBudget {
    debug_assert!(requested.is_realtime());
    let featured = requested.is_featured();
    let (ray_divisor, bounce_reduction, duration_divisor, cadence_multiplier) =
        match (quality_tier, quality.reflections.level, featured) {
            (_, ReflectionQualityLevel::Full, _) => (1, 0, 1.0, 1),
            // Reduced is Mobile's construction-time ceiling. Its one Standard
            // source therefore retains the requested phone budget at this rung.
            (QualityTier::Mobile, ReflectionQualityLevel::Reduced, false) => (1, 0, 1.0, 1),
            // Protect the featured source while ordinary sources thin first.
            (QualityTier::Desktop, ReflectionQualityLevel::Reduced, true) => (1, 0, 1.0, 1),
            (_, ReflectionQualityLevel::Reduced, false) => (2, 1, 2.0, 2),
            (_, ReflectionQualityLevel::Reduced, true) => (2, 1, 2.0, 2),
            (QualityTier::Desktop, ReflectionQualityLevel::Intermediate, _) => (4, 1, 2.0, 4),
            (QualityTier::Mobile, ReflectionQualityLevel::Intermediate, _) => (2, 0, 1.0, 4),
            (_, ReflectionQualityLevel::Minimum, true) => (2, 1, 2.0, 2),
            (_, ReflectionQualityLevel::Minimum, false) => (4, i32::MAX, 4.0, 4),
        };
    SourceReflectionBudget::realtime(
        (requested.rays / ray_divisor).max(requested.rays.min(128)),
        requested.bounces.saturating_sub(bounce_reduction).max(0)
            .min(if quality.reflections.level == ReflectionQualityLevel::Intermediate { 3 } else { i32::MAX }),
        (requested.duration_s / duration_divisor).max(0.05),
        requested.order.min(quality.ambisonic_order).max(0),
        requested
            .cadence_divisor
            .saturating_mul(cadence_multiplier)
            .max(1),
    )
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
        // Reflection reconstruction applies air over excess delay only; the
        // shared law there also needs a per-source input EQ, so the reflection
        // pass keeps Steam's default model.
        airAbsorptionModel: if flag & ffi::IPL_SIMULATIONFLAGS_DIRECT != 0 {
            iso_air_absorption_model(config.air_pressure_exponents_per_m)
        } else {
            default_air_absorption_model()
        },
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
        debug_assert_eq!(self.channels, 1);
        self.write_interleaved(samples);
    }

    fn write_interleaved(&mut self, samples: &mut [f32]) {
        let mut raw = self.raw();
        ffi::audio_buffer_deinterleave(handle(self.context), samples, &mut raw);
    }

    fn read_interleaved(&mut self, output: &mut [f32]) {
        let mut raw = self.raw();
        ffi::audio_buffer_interleave(handle(self.context), &mut raw, output);
    }

    fn clear_with_interleaved_scratch(&mut self, scratch: &mut [f32]) {
        let samples = usize::try_from(self.samples).expect("validated positive sample count");
        let channels = usize::try_from(self.channels).expect("validated positive channel count");
        let required = samples
            .checked_mul(channels)
            .expect("validated audio buffer dimensions fit usize");
        let zeros = &mut scratch[..required];
        zeros.fill(0.0);
        self.write_interleaved(zeros);
    }

    fn payload_bytes(&self) -> u64 {
        (self.channels as u64)
            .saturating_mul(self.samples as u64)
            .saturating_mul(size_of::<f32>() as u64)
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
    reflection_ir_capacity: i32,
    input: OwnedAudioBuffer,
    direct_mono: OwnedAudioBuffer,
    direct_stereo: OwnedAudioBuffer,
    direct_silent_pair: SteadySilentPair,
    path_stereo: OwnedAudioBuffer,
    path_silent: SteadyPath,
    reflection_scratch: OwnedAudioBuffer,
    propagation_smoother: SourcePropagationSmoother,
    impulse_shaper: Option<ImpulseShaper>,
    pathing_send_enabled: bool,
    reflection_send_enabled: bool,
    propagation_delay: PropagationDelayLine,
    /// The pathing stage's head on `propagation_delay` behind a host route.
    route_head: RouteReadHead,
    roof_head: crate::over_roof::RoofReadHead,
    last_propagation_observation: Option<PropagationObservationKey>,
    rendered_since_reset: bool,
    program_history: bool,
    guard_reactivation_history: bool,
    reactivation_epoch_samples: usize,
    quality_gains: [f32; 3],
    reflection_channels: i32,
    reflection_adopted_sequence: u64,
    applied_reflections: SteamReflectionParams,
    reflection_activity: ReflectionActivity,
    width: Option<LineWidthRenderState>,
    stereo_image: Option<StereoImageRenderState>,
    echo: Option<EchoRenderState>,
}

struct EchoRenderState {
    profile: EchoProfile,
    scheduler: EchoLoopScheduler,
    delay: EchoDelayRing,
    tap_direct_effects: [usize; MAX_ECHO_TAPS_PER_SOURCE],
    tap_binaural_effects: [usize; MAX_ECHO_TAPS_PER_SOURCE],
    tap_silent_pairs: Box<[SteadySilentPair; MAX_ECHO_TAPS_PER_SOURCE]>,
    tap_shapers: [Option<ImpulseShaper>; MAX_ECHO_TAPS_PER_SOURCE],
    input: OwnedAudioBuffer,
    filtered: OwnedAudioBuffer,
    stereo: OwnedAudioBuffer,
    tap_work: Vec<f32>,
    active_plan: EchoSourcePlan,
    active_delivered_taps: u8,
    has_triggered: bool,
    /// Plan to freeze if this block holds an onset or explicit trigger;
    /// staged by `render_block`, meaningless otherwise.
    onset_plan: EchoSourcePlan,
    /// Set once the host has ever triggered this source. The loop scheduler
    /// then never self-fires: each emission is an explicit trigger.
    trigger_mode: bool,
    pending_trigger: bool,
    /// The routed primary's transfer read at the last explicit trigger.
    primary_transfer: Option<PrimaryTransfer>,
    /// Frames since the last explicit trigger's block start; saturated when
    /// none. A tap stays silent until its own delay has elapsed, so a new plan
    /// never re-reads the previous shot still in the ring.
    samples_since_trigger: u32,
}

impl EchoRenderState {
    fn freeze(&mut self, delivered_taps: u8) {
        self.active_plan = self.onset_plan;
        self.active_delivered_taps = delivered_taps.min(self.onset_plan.tap_count);
        self.has_triggered = self.onset_plan.tap_count != 0;
        for shaper in self.tap_shapers.iter_mut().flatten() {
            shaper.reset();
        }
        for cache in self.tap_silent_pairs.iter_mut() {
            cache.reset();
        }
    }

    fn reset(&mut self) {
        self.scheduler.reset();
        self.delay.reset();
        self.tap_work.fill(0.0);
        self.active_plan = EchoSourcePlan::default();
        self.active_delivered_taps = 0;
        self.has_triggered = false;
        self.onset_plan = EchoSourcePlan::default();
        self.pending_trigger = false;
        self.primary_transfer = None;
        self.samples_since_trigger = u32::MAX;
        for shaper in self.tap_shapers.iter_mut().flatten() {
            shaper.reset();
        }
        for cache in self.tap_silent_pairs.iter_mut() {
            cache.reset();
        }
    }
}

/// Callback half of the host echo seam: shared trigger generations, the host's
/// published plans, and the last generation observed per source.
struct EchoTriggerRender {
    generations: Arc<EchoTriggerGenerations>,
    /// One publication per source keeps each callback read to one plan.
    external_plans: Vec<fightbox_runtime::SnapshotReader<ExternalEchoPlan>>,
    /// Host primary routes, read every block: they time the pathing head.
    routes: Vec<fightbox_runtime::SnapshotReader<Option<crate::PrimaryRoute>>>,
    observed: [u64; MAX_ACTIVE_SOURCES],
}

/// Control half of the host echo seam. Converts published path geometry with
/// the shared tap law off the callback and publishes it to that source's slot;
/// the callback reads a slot only in a block that freezes its plan.
pub(crate) struct EchoPlanWriter {
    air_exponents: [f32; 3],
    world: Arc<WorldGeneration>,
    sample_rate_hz: i32,
    profiles: Vec<EchoProfile>,
    writers: Vec<fightbox_runtime::SnapshotWriter<ExternalEchoPlan>>,
    route_writers: Vec<fightbox_runtime::SnapshotWriter<Option<crate::PrimaryRoute>>>,
    sequence: u64,
}

impl EchoPlanWriter {
    pub(crate) fn set_air_exponents(&mut self, exponents: [f32; 3]) {
        self.air_exponents = exponents;
    }

    pub(crate) fn publish(
        &mut self,
        source_index: usize,
        primary: EchoPrimary,
        paths: &[crate::EchoPathGeometry],
    ) -> Result<PlannedEchoTaps, EchoPlanError> {
        if source_index >= self.world.source_count {
            return Err(EchoPlanError::SourceOutOfRange);
        }
        if !self.profiles[source_index].is_enabled() {
            return Err(EchoPlanError::SourceEchoDisabled);
        }
        let plan = external_echo_plan(
            self.world.context(),
            primary,
            paths,
            self.sample_rate_hz,
            self.sequence.wrapping_add(1),
            self.air_exponents,
        )?;
        self.sequence = plan.generation;
        self.writers[source_index].publish(ExternalEchoPlan {
            present: true,
            plan,
        });
        let mut planned = PlannedEchoTaps {
            count: plan.tap_count,
            ..PlannedEchoTaps::default()
        };
        for (summary, tap) in planned.taps.iter_mut().zip(&plan.taps) {
            *summary = PlannedEchoTap {
                kind: tap.kind,
                stable_path_id: tap.stable_path_id,
                physical_path_length_m: tap.total_path_distance_m,
                delay_samples: tap.delay_samples,
                distance_gain: tap.distance_gain,
                band_gain: tap.band_gain,
                primary_relative_gain: tap.inherits_primary.then_some(tap.primary_relative_gain),
            };
        }
        Ok(planned)
    }

    pub(crate) fn publish_route(
        &mut self,
        source_index: usize,
        route: Option<crate::PrimaryRoute>,
    ) -> Result<(), EchoPlanError> {
        if source_index >= self.world.source_count {
            return Err(EchoPlanError::SourceOutOfRange);
        }
        if !self.profiles[source_index].is_enabled() {
            return Err(EchoPlanError::SourceEchoDisabled);
        }
        if route.is_some_and(|route| {
            !(route.length_m > 0.0
                && route.length_m <= crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS)
        }) {
            return Err(EchoPlanError::InvalidPath);
        }
        self.route_writers[source_index].publish(route);
        Ok(())
    }

    /// Returns the source to the backend's own analytic plan.
    pub(crate) fn clear(&mut self, source_index: usize) {
        if let Some(writer) = self.writers.get_mut(source_index) {
            writer.publish(ExternalEchoPlan::default());
        }
    }
}

struct LineWidthRenderState {
    length_m: f32,
    renderer: LineWidthRenderer,
    plus_binaural_effect: usize,
    minus_binaural_effect: usize,
    silent_binaural: Box<[SteadyBinaural; 3]>,
    presentation: OwnedAudioBuffer,
    direct: OwnedAudioBuffer,
}

struct StereoImageRenderState {
    width_m: f32,
    direct_effect: usize,
    left_binaural_effect: usize,
    right_binaural_effect: usize,
    input: OwnedAudioBuffer,
    direct: OwnedAudioBuffer,
    delay: StereoProgramPropagationDelay,
    roof_head: crate::over_roof::RoofReadHead,
    impulse_shapers: [Option<ImpulseShaper>; 2],
}

impl Drop for StereoImageRenderState {
    fn drop(&mut self) {
        if self.direct_effect != 0 {
            let mut direct = handle(self.direct_effect);
            ffi::direct_effect_release(&mut direct);
        }
        for effect in [self.left_binaural_effect, self.right_binaural_effect] {
            if effect != 0 {
                let mut binaural = handle(effect);
                ffi::binaural_effect_release(&mut binaural);
            }
        }
    }
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
    reflection_share_radii: [f32; MAX_ACTIVE_SOURCES],
    reflection_share_capacities: [i32; MAX_ACTIVE_SOURCES],
    reflection_share_targets: [usize; MAX_ACTIVE_SOURCES],
    reflection_previous_share_targets: [usize; MAX_ACTIVE_SOURCES],
    reflection_share_gains: [f32; MAX_ACTIVE_SOURCES],
    reflection_shared_work: Vec<f32>,
    mono_work: Vec<f32>,
    program_mono_work: Option<Vec<f32>>,
    /// The pathing stage's input while a route head is heard.
    route_work: Vec<f32>,
    roof_work: Vec<f32>,
    stereo_work: Vec<f32>,
    live_direct_path_left: Vec<f32>,
    live_direct_path_right: Vec<f32>,
    width_work: Vec<f32>,
    width_feed_work: Vec<f32>,
    spatial_export: Option<FullSpatialExportTap>,
    publication: fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
    stage_output_gain_writer: Option<fightbox_runtime::SnapshotWriter<StageOutputGains>>,
    stage_output_gains: fightbox_runtime::SnapshotReader<StageOutputGains>,
    echo_output_gain_writer: Option<fightbox_runtime::SnapshotWriter<f32>>,
    echo_output_gain: fightbox_runtime::SnapshotReader<f32>,
    governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    applied_governor_quality: GovernorRenderSnapshot,
    echo_profiles: [EchoProfile; MAX_ACTIVE_SOURCES],
    has_echo_sources: bool,
    /// Present only for graphs with an enabled echo profile.
    echo_trigger: Option<Box<EchoTriggerRender>>,
    echo_trigger_control: Option<(EchoTrigger, EchoPlanWriter)>,
    scene_reset_sequence: Arc<std::sync::atomic::AtomicU64>,
    observed_scene_reset: u64,
    reflection_output_gain: f32,
    reflection_block_order: i32,
    retire_silent_reflections: bool,
    idle_reflection_source: usize,
    reflection_ir_hold: usize,
    reflection_adoption: ReflectionAdoption,
    reflection_adoption_source: Option<usize>,
    tail_retiring: bool,
    reflection_tail_remaining: bool,
    reflection_decode_tail_remaining: bool,
    echo_tail_remaining: bool,
    tail_retirement_frames: u64,
    reflection_tail_deadline_frames: u64,
    echo_tail_deadline_frames: u64,
    retirement_stage_output_gains: StageOutputGains,
    retirement_echo_output_gain: f32,
    retirement_listener_position: SteamVector3,
    propagation_block_retention: f32,
    live_energy_writer: fightbox_runtime::SnapshotWriter<crate::LiveStageEnergySnapshot>,
    live_energy_reader: Option<fightbox_runtime::SnapshotReader<crate::LiveStageEnergySnapshot>>,
    live_energy_sequence: u64,
    #[cfg(test)]
    governor_snapshot_reads: u64,
    // Must drop after every SDK effect and audio buffer. Keeping the world
    // last also keeps its context alive when the simulation half dropped first.
    world: Arc<WorldGeneration>,
}

impl MultiSourceRenderGraph {
    pub(crate) fn scene_reset_control(&self) -> crate::SceneResetControl {
        crate::SceneResetControl { sequence: Arc::clone(&self.scene_reset_sequence) }
    }

    fn reset_scene_history(&mut self) {
        if let Some(tap) = &mut self.spatial_export { tap.reset(); }
        for state in &mut self.sources {
            state.propagation_smoother.reset();
            state.propagation_delay.reset_history();
            state.route_head.invalidate();
            state.roof_head.invalidate();
            state.last_propagation_observation = None;
            state.guard_reactivation_history = false;
            state.reactivation_epoch_samples = 0;
            state.program_history = false;
            if let Some(shaper) = &mut state.impulse_shaper { shaper.reset(); }
            ffi::direct_effect_reset(handle(state.direct_effect));
            ffi::binaural_effect_reset(handle(state.binaural_effect));
            state.direct_silent_pair.reset();
            ffi::path_effect_reset(handle(state.path_effect));
            state.path_silent.reset();
            // A drained convolution already has zero dry history. Retain its
            // current IR instead of clearing a large, already silent bank.
            if !self.retire_silent_reflections || state.reflection_activity.has_history() {
                ffi::reflection_effect_reset(handle(state.reflection_effect));
                // Keep an async acknowledgement across history resets: replaying
                // its token could consume the next IR before metadata arrives.
                if !self.world.reflection_worker_enabled.load(Ordering::Acquire) {
                    state.reflection_channels = 0;
                    state.reflection_adopted_sequence = 0;
                    state.applied_reflections = SteamReflectionParams::default();
                }
            }
            state.reflection_activity.reset();
            if let Some(width) = &mut state.width {
                width.renderer.reset();
                for cache in width.silent_binaural.iter_mut() { cache.reset(); }
                ffi::binaural_effect_reset(handle(width.plus_binaural_effect));
                ffi::binaural_effect_reset(handle(width.minus_binaural_effect));
            }
            if let Some(stereo) = &mut state.stereo_image {
                stereo.delay.reset_history();
                stereo.roof_head.invalidate();
                for shaper in stereo.impulse_shapers.iter_mut().flatten() { shaper.reset(); }
                for effect in [stereo.left_binaural_effect, stereo.right_binaural_effect] {
                    if effect != 0 { ffi::binaural_effect_reset(handle(effect)); }
                }
                if stereo.direct_effect != 0 { ffi::direct_effect_reset(handle(stereo.direct_effect)); }
            }
            if let Some(echo) = &mut state.echo {
                echo.reset();
                for effect in echo.tap_direct_effects { ffi::direct_effect_reset(handle(effect)); }
                for effect in echo.tap_binaural_effects { ffi::binaural_effect_reset(handle(effect)); }
            }
        }
        ffi::reflection_mixer_reset(handle(self.reflection_mixer));
        ffi::ambisonics_decode_effect_reset(handle(self.ambisonics_decode));
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

    pub(crate) fn take_stage_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<StageOutputGains>> {
        self.stage_output_gain_writer.take()
    }

    pub(crate) fn take_echo_output_gain_writer(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotWriter<f32>> {
        self.echo_output_gain_writer.take()
    }

    /// `None` for a graph without an enabled echo profile, or once taken.
    pub(crate) fn take_echo_trigger_control(&mut self) -> Option<(EchoTrigger, EchoPlanWriter)> {
        self.echo_trigger_control.take()
    }

    pub(crate) fn take_live_stage_energy_reader(
        &mut self,
    ) -> Option<fightbox_runtime::SnapshotReader<crate::LiveStageEnergySnapshot>> {
        self.live_energy_reader.take()
    }

    pub(crate) fn begin_tail_retirement(&mut self) {
        self.tail_retiring = true;
        self.tail_retirement_frames = 0;
        self.retirement_stage_output_gains = self.stage_output_gains.read();
        self.retirement_echo_output_gain = self.echo_output_gain.read();
        let snapshot = self.publication.read();
        self.retirement_listener_position = snapshot.listener_position;
        self.reflection_tail_remaining = self
            .sources
            .iter()
            .any(|source| source.reflection_send_enabled);
        self.reflection_tail_deadline_frames = (f64::from(self.config.reflection_duration_s)
            * f64::from(self.audio.sample_rate_hz))
        .ceil()
        .max(self.audio.frame_size as f64) as u64
            + u64::from(ffi::ambisonics_decode_effect_get_tail_size(handle(
                self.ambisonics_decode,
            )))
            + self.audio.frame_size as u64;

        let maximum_echo_delay_frames = self
            .sources
            .iter()
            .filter_map(|source| source.echo.as_ref())
            .filter(|echo| echo.has_triggered && echo.active_delivered_taps != 0)
            .flat_map(|echo| {
                echo.active_plan.taps[..usize::from(echo.active_delivered_taps)]
                    .iter()
                    .filter(|tap| tap.valid)
                    .map(|tap| tap.delay_samples.ceil().max(0.0) as u64)
            })
            .max();
        self.echo_tail_remaining = maximum_echo_delay_frames.is_some();
        self.echo_tail_deadline_frames = maximum_echo_delay_frames
            .unwrap_or(0)
            .saturating_add(self.audio.frame_size as u64);
    }

    #[must_use]
    pub(crate) fn tail_retirement_state(&self) -> SpatialTailRetirementState {
        if self.reflection_tail_remaining
            || self.reflection_decode_tail_remaining
            || self.echo_tail_remaining
        {
            SpatialTailRetirementState::TailRemaining
        } else {
            SpatialTailRetirementState::TailComplete
        }
    }

    fn advance_retiring_tails(
        &mut self,
        listener: SteamPose,
        output_left: &mut [f32],
        output_right: &mut [f32],
    ) -> bool {
        let reflection_frame_ready = self.reflection_tail_remaining;
        if reflection_frame_ready {
            let mut any_remaining = false;
            for source in &mut self.sources {
                if !source.reflection_send_enabled {
                    continue;
                }
                let mut scratch = source.reflection_scratch.raw();
                any_remaining |= ffi::reflection_effect_get_tail_to_mixer(
                    handle(source.reflection_effect),
                    &mut scratch,
                    handle(self.reflection_mixer),
                );
            }
            self.reflection_tail_remaining = any_remaining;
        }

        if self.echo_tail_remaining {
            for source in &mut self.sources {
                let Some(echo) = &mut source.echo else {
                    continue;
                };
                if !echo.has_triggered || echo.active_delivered_taps == 0 {
                    continue;
                }
                render_retiring_echo_sidecar(
                    echo,
                    listener,
                    self.retirement_listener_position,
                    self.hrtf,
                    self.retirement_stage_output_gains.reflections
                        * self.applied_governor_quality.reflection_output_gain
                        * self.retirement_echo_output_gain,
                    output_left,
                    output_right,
                    &mut self.mono_work,
                    &mut self.stereo_work,
                    self.retire_silent_reflections && self.audio.frame_size == 128
                        && source.stereo_image.is_none(),
                );
            }
        }

        self.tail_retirement_frames = self
            .tail_retirement_frames
            .saturating_add(self.audio.frame_size as u64);
        if self.tail_retirement_frames >= self.reflection_tail_deadline_frames {
            self.reflection_tail_remaining = false;
            self.reflection_decode_tail_remaining = false;
        }
        if self.tail_retirement_frames >= self.echo_tail_deadline_frames {
            self.echo_tail_remaining = false;
        }
        reflection_frame_ready
    }

    pub(crate) fn render_retiring_tail(
        &mut self,
        listener_orientation: ListenerOrientation,
        output_left: &mut [f32],
        output_right: &mut [f32],
    ) -> Result<SpatialTailRetirementState, BackendRenderError> {
        let frames = self.audio.frame_size as usize;
        if output_left.len() != frames || output_right.len() != frames {
            return Err(BackendRenderError::InvalidBlockLength);
        }
        let listener =
            listener_pose(listener_orientation).ok_or(BackendRenderError::InactiveGraph)?;
        let reflection_frame_ready =
            self.advance_retiring_tails(listener, output_left, output_right);
        let mut live_energy = StageEnergyAccumulator::default();
        if reflection_frame_ready {
            self.render_reflection_mix(
                listener,
                output_left,
                output_right,
                self.retirement_stage_output_gains.reflections,
                self.applied_governor_quality,
                &mut live_energy,
            );
        } else if self.reflection_decode_tail_remaining {
            self.render_reflection_decode_tail(
                output_left,
                output_right,
                self.retirement_stage_output_gains.reflections,
                self.applied_governor_quality,
                &mut live_energy,
            );
        }
        Ok(self.tail_retirement_state())
    }

    #[cfg(test)]
    pub(crate) fn render_block(
        &mut self,
        block: PropagationRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        if block.sources.len() > MAX_ACTIVE_SOURCES {
            return Err(BackendRenderError::InvalidSourceIndex);
        }
        let mut sources = [SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&[], &[]],
        }; MAX_ACTIVE_SOURCES];
        for (program, source) in sources.iter_mut().zip(block.sources) {
            *program = SpatialBackendSourceBlock {
                source_index: source.source_index,
                program_plane_count: 1,
                program_planes: [source.input_mono, &[]],
            };
        }
        self.render_program_block(fightbox_runtime::ProgramRenderBlock {
            listener_orientation: block.listener_orientation,
            sources: &sources[..block.sources.len()],
            output_left: block.output_left,
            output_right: block.output_right,
        })
    }

    pub(crate) fn render_program_block(
        &mut self,
        block: fightbox_runtime::ProgramRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        let mut _profile_total = crate::render_profile::timer(0);
        crate::render_profile::count(6);
        let frames = self.audio.frame_size as usize;
        if block.output_left.len() != frames || block.output_right.len() != frames {
            return Err(BackendRenderError::InvalidBlockLength);
        }
        for source in block.sources {
            if source.source_index >= self.world.source_count {
                return Err(BackendRenderError::InvalidSourceIndex);
            }
            if !(1..=2).contains(&source.program_plane_count)
                || (source.program_plane_count == 2
                    && self.sources[source.source_index].stereo_image.is_none())
            {
                return Err(BackendRenderError::InactiveGraph);
            }
            if source.program_planes[..source.program_plane_count]
                .iter()
                .any(|plane| plane.len() != frames)
            {
                return Err(BackendRenderError::InvalidBlockLength);
            }
            if !source.program_planes[..source.program_plane_count]
                .iter()
                .all(|plane| plane.iter().copied().all(f32::is_finite))
            {
                return Err(BackendRenderError::InactiveGraph);
            }
        }
        let listener =
            listener_pose(block.listener_orientation).ok_or(BackendRenderError::InactiveGraph)?;
        let scene_reset = self.scene_reset_sequence.load(std::sync::atomic::Ordering::Acquire);
        if scene_reset != self.observed_scene_reset {
            self.reset_scene_history();
            self.observed_scene_reset = scene_reset;
        }
        let snapshot = self.publication.read();
        if snapshot.world_generation != self.world.generation {
            return Err(BackendRenderError::InactiveGraph);
        }
        let stage_output_gains = if self.tail_retiring {
            self.retirement_stage_output_gains
        } else {
            self.stage_output_gains.read()
        };
        let echo_output_gain = if self.tail_retiring {
            self.retirement_echo_output_gain
        } else {
            self.echo_output_gain.read()
        };
        let governor_quality = if self.tail_retiring {
            self.applied_governor_quality
        } else {
            self.governor_quality.read()
        };
        #[cfg(test)]
        {
            self.governor_snapshot_reads = self.governor_snapshot_reads.saturating_add(1);
        }
        self.applied_governor_quality = governor_quality;
        if let Some(profile) = _profile_total.as_mut() {
            profile.set_quality(governor_quality.reflections.level,
                self.audio.frame_size as u64 * 1_000_000_000 / self.audio.sample_rate_hz as u64,
                governor_quality.ladder_position);
        }
        let delivered_echo_taps = if self.has_echo_sources && !self.tail_retiring {
            let mut rendered = [false; MAX_ACTIVE_SOURCES];
            for source in block.sources {
                rendered[source.source_index] = true;
            }
            let active =
                std::array::from_fn(|index| rendered[index] && snapshot.sources[index].active);
            let mut onsets = [false; MAX_ACTIVE_SOURCES];
            for (index, source) in self.sources.iter_mut().enumerate() {
                let Some(echo) = &mut source.echo else {
                    continue;
                };
                if let Some(trigger) = &mut self.echo_trigger {
                    let generation = trigger.generations.load(index);
                    if generation != trigger.observed[index] {
                        trigger.observed[index] = generation;
                        echo.trigger_mode = true;
                        echo.pending_trigger = true;
                    }
                }
                onsets[index] = echo.pending_trigger
                    || (!echo.trigger_mode && {
                        let mut scheduler = echo.scheduler;
                        (0..frames).any(|_| scheduler.advance_sample(echo.profile))
                    });
            }
            // A host plan is read only for a source that freezes one.
            for (index, source) in self.sources.iter_mut().enumerate() {
                if let Some(echo) = &mut source.echo
                    && onsets[index]
                {
                    let external = match &mut self.echo_trigger {
                        Some(trigger) => trigger.external_plans[index].read(),
                        None => ExternalEchoPlan::default(),
                    };
                    echo.onset_plan = if external.present {
                        external.plan
                    } else {
                        snapshot.sources[index].echo
                    };
                }
            }
            let plans = std::array::from_fn(|index| {
                let Some(echo) = self
                    .sources
                    .get(index)
                    .and_then(|source| source.echo.as_ref())
                else {
                    return EchoSourcePlan::default();
                };
                if onsets[index] {
                    echo.onset_plan
                } else if echo.has_triggered {
                    echo.active_plan
                } else {
                    EchoSourcePlan::default()
                }
            });
            delivered_tap_counts(
                &self.echo_profiles,
                &plans,
                &active,
                &governor_quality.sources,
                self.world.source_count,
                governor_quality.reflections.level,
            )
        } else {
            [0; MAX_ACTIVE_SOURCES]
        };
        if !self.tail_retiring {
            for (index, state) in self.sources.iter_mut().enumerate() {
                if snapshot.sources[index].active {
                    continue;
                }
                if let Some(tap) = &mut self.spatial_export { tap.reset_source(index); }
                state.propagation_smoother.reset();
                if let Some(shaper) = &mut state.impulse_shaper {
                    shaper.reset();
                }
                if state.rendered_since_reset {
                    state.guard_reactivation_history = true;
                }
                state.propagation_delay.invalidate();
                state.route_head.invalidate();
                state.roof_head.invalidate();
                if let Some(width) = &mut state.width {
                    width.renderer.reset();
                }
                if let Some(stereo) = &mut state.stereo_image {
                    stereo.delay.invalidate();
                    stereo.roof_head.invalidate();
                    ffi::direct_effect_reset(handle(stereo.direct_effect));
                    ffi::binaural_effect_reset(handle(stereo.left_binaural_effect));
                    ffi::binaural_effect_reset(handle(stereo.right_binaural_effect));
                    for shaper in stereo.impulse_shapers.iter_mut().flatten() {
                        shaper.reset();
                    }
                }
                state.last_propagation_observation = None;
                state.reactivation_epoch_samples = 0;
                if let Some(echo) = &mut state.echo {
                    echo.reset();
                }
            }
        }

        let mut live_energy = StageEnergyAccumulator {
            audible_source_count: block
                .sources
                .iter()
                .filter(|source| {
                    source.program_planes[..source.program_plane_count]
                        .iter()
                        .any(|plane| plane.iter().any(|sample| *sample != 0.0))
                })
                .count()
                .min(u8::MAX as usize) as u8,
            ..StageEnergyAccumulator::default()
        };
        self.idle_reflection_source = (self.idle_reflection_source + 1) % self.sources.len();
        let asynchronous_reflections = self.world.reflection_worker_enabled.load(Ordering::Acquire);
        let reflection_ir_hold = if asynchronous_reflections {
            self.world.reflection_worker_hold_ir.load(Ordering::Relaxed)
        } else { self.reflection_ir_hold };
        self.reflection_adoption_source = if reflection_ir_hold != 0 {
            let mut pending = [false; MAX_ACTIVE_SOURCES];
            for source in block.sources {
                let index = source.source_index;
                let state = &self.sources[index];
                let propagation = snapshot.sources[index];
                let cold = state.reflection_channels == 0;
                let awake = cold
                    || state.reflection_activity.has_history()
                    || state.reflection_activity.wake_frames != 0
                    || source.program_planes[..source.program_plane_count]
                        .iter()
                        .any(|plane| plane.iter().any(|sample| *sample != 0.0))
                    || (self.idle_reflection_source == index
                        && state.propagation_delay.current_delay_samples()
                            < self.audio.frame_size as f32 * 2.0);
                pending[index] = propagation.active
                    && propagation.reflections.ir != 0
                    && (asynchronous_reflections || awake)
                    && ((cold && !asynchronous_reflections)
                        || state.reflection_adopted_sequence != propagation.reflection_sequence);
            }
            self.reflection_adoption.select(&pending[..self.sources.len()])
        } else {
            None
        };
        self.reflection_shared_work.fill(0.0);
        self.reflection_share_targets = if asynchronous_reflections {
            shared_reflection_targets(
                &std::array::from_fn(|i| snapshot.sources[i].source_position),
                &std::array::from_fn(|i| snapshot.sources[i].active),
                &governor_quality.sources, &self.reflection_share_radii,
                &self.reflection_share_capacities, self.sources.len(),
            )
        } else { std::array::from_fn(|i| i) };
        let mut shared_primaries = [false; MAX_ACTIVE_SOURCES];
        for i in 0..self.sources.len() {
            for target in [self.reflection_share_targets[i], self.reflection_previous_share_targets[i]] {
                if target != i { shared_primaries[target] = true; }
            }
        }
        self.reflection_block_order = governor_quality.ambisonic_order;
        self.live_direct_path_left.fill(0.0);
        self.live_direct_path_right.fill(0.0);
        for deferred in [false, true] {
        for source_block in block.sources {
            if shared_primaries[source_block.source_index] != deferred { continue; }
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
                snapshot.direct_sequence,
                block.output_left,
                block.output_right,
                stage_output_gains,
                governor_quality,
                delivered_echo_taps[source_block.source_index],
                echo_output_gain,
            );
        }
        }
        self.reflection_previous_share_targets = self.reflection_share_targets;
        let retiring_reflection_frame = self.tail_retiring
            && self.advance_retiring_tails(listener, block.output_left, block.output_right);
        live_energy.direct_path_energy =
            stereo_planes_energy(&self.live_direct_path_left, &self.live_direct_path_right);
        if !self.tail_retiring || retiring_reflection_frame {
            self.render_reflection_mix(
                listener,
                block.output_left,
                block.output_right,
                stage_output_gains.reflections,
                governor_quality,
                &mut live_energy,
            );
        } else if self.reflection_decode_tail_remaining {
            self.render_reflection_decode_tail(
                block.output_left,
                block.output_right,
                stage_output_gains.reflections,
                governor_quality,
                &mut live_energy,
            );
        }
        self.live_energy_sequence = self.live_energy_sequence.wrapping_add(1);
        self.live_energy_writer
            .publish(crate::LiveStageEnergySnapshot {
                sequence: self.live_energy_sequence,
                simulation_sequence: snapshot.sequence,
                world_generation: self.world.generation,
                audible_source_count: live_energy.audible_source_count,
                direct_path_energy: live_energy.direct_path_energy,
                reflection_energy: live_energy.reflection_energy,
            });
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
    /// Published source and listener velocities reconstruct same-time geometry
    /// between position snapshots. The delay line solves
    /// `D(t) = distance(t - D) / 343`, so its derivative produces the exact
    /// reception-time ratio `1 / (1 + v_radial / 343)` without a second pitch
    /// stage. Raw positions remain the sole teleport signal, but observations
    /// update only the present end of the geometry history rather than replacing
    /// the active read target. Finite zero relative speed takes the exact legacy
    /// static path. Motion enters the fast algorithm at 8 m/s and leaves at
    /// 7 m/s; the hysteresis band prevents mode flutter. Fast approaches use a
    /// rate-aware bandlimited readout through the documented 2x ratio bound;
    /// recession retains the legacy half-sample safety ceiling.
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
    /// per-path length, so without a host route the source-distance delay is
    /// used as a lower bound: around-corner energy arrives slightly early
    /// rather than before the source was audible at all. A host-published
    /// primary route instead gives the pathing stage its own head on this
    /// line at `L / c` (see `RouteReadHead`); direct and reflections keep the
    /// straight line.
    fn render_source(
        &mut self,
        source_block: &SpatialBackendSourceBlock<'_>,
        propagation: SteamSourcePropagation,
        listener: SteamPose,
        listener_position: SteamVector3,
        listener_linear_velocity_mps: SteamVector3,
        direct_sequence: u64,
        output_left: &mut [f32],
        output_right: &mut [f32],
        stage_output_gains: StageOutputGains,
        governor_quality: GovernorRenderSnapshot,
        delivered_echo_taps: u8,
        echo_output_gain: f32,
    ) {
        crate::render_profile::count(7);
        let profile_prepare = crate::render_profile::timer(1);
        let route = self
            .echo_trigger
            .as_mut()
            .and_then(|trigger| trigger.routes[source_block.source_index].read());
        let state = &mut self.sources[source_block.source_index];
        let stereo_program = source_block.program_plane_count == 2;
        let input_mono = if stereo_program {
            let folded = self.program_mono_work.as_mut().expect("stereo program scratch");
            for (frame, sample) in folded.iter_mut().enumerate() {
                *sample = 0.5 * source_block.program_planes[0][frame]
                    + 0.5 * source_block.program_planes[1][frame];
            }
            folded.as_slice()
        } else {
            source_block.program_planes[0]
        };
        state.program_history |= input_mono.iter().any(|sample| *sample != 0.0);
        let source_quality = governor_quality.sources[source_block.source_index];
        let listener_centric_reflection = governor_quality.reverb
            != ReverbStrategy::ListenerCentric
            || usize::from(governor_quality.listener_centric_source) == source_block.source_index;
        let mut targets = source_quality_targets(source_quality, listener_centric_reflection);
        if !self.world.has_baked_pathing || !state.pathing_send_enabled {
            targets[1] = 0.0;
        }
        if !state.reflection_send_enabled
            || self.reflection_share_targets[source_block.source_index] != source_block.source_index {
            targets[2] = 0.0;
        }
        let relative_speed_mps = relative_speed_mps(
            propagation.linear_velocity_mps,
            listener_linear_velocity_mps,
        );
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
                relative_speed_mps,
                self.propagation_block_retention,
            )
            .endpoint();
        // The rendered primary behind a routed echo plan is this source's
        // baked path; the raw target is its steady transfer, not a fade-in.
        let primary_transfer = state
            .pathing_send_enabled
            .then(|| primary_path_transfer(propagation.path_eq, propagation.path_sh[0]))
            .flatten();
        drop(profile_prepare);
        let profile_echo = crate::render_profile::timer(4);
        if !self.tail_retiring
            && let Some(echo) = &mut state.echo
        {
            let transfer_fallback = render_echo_sidecar(
                echo,
                input_mono,
                delivered_echo_taps,
                primary_transfer,
                listener,
                smoothed.listener_position,
                self.hrtf,
                stage_output_gains.reflections
                    * governor_quality.reflection_output_gain
                    * echo_output_gain,
                output_left,
                output_right,
                &mut self.mono_work,
                &mut self.stereo_work,
                self.retire_silent_reflections && self.audio.frame_size == 128
                    && state.stereo_image.is_none(),
                self.spatial_export.as_mut(),
            );
            if transfer_fallback && let Some(trigger) = &self.echo_trigger {
                trigger
                    .generations
                    .note_transfer_fallback(source_block.source_index);
            }
        }
        drop(profile_echo);
        let profile_prepare = crate::render_profile::timer(1);
        // Time of flight is anchored by the simulated endpoints rather than by
        // the 80 ms acoustic smoother above. The smoother exists to keep gain
        // and occlusion from zippering; running the raw delay through it too
        // would erase the distinction between motion and a teleport, which is
        // exactly what the delay line must be able to tell apart. Published
        // velocity reconstructs the between-snapshot geometry; the delay line
        // reads that history at emission time, so publication changes cannot
        // replace the active retarded-time target.
        let delay_target_samples = uncapped_propagation_delay_samples(
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
        let observation = propagation_observation_key(
            direct_sequence,
            delay_target_samples,
            radial_velocity_mps,
            relative_speed_mps,
        );
        let mut width_teleported = false;
        if propagation_observation_cache_miss(state.last_propagation_observation, observation) {
            width_teleported =
                state
                    .last_propagation_observation
                    .is_some_and(|(_, previous_delay_bits, _, _)| {
                        (delay_target_samples - f32::from_bits(previous_delay_bits)).abs()
                            > TELEPORT_DELAY_STEP_SECONDS * self.audio.sample_rate_hz as f32
                    });
            if relative_speed_mps == 0.0 {
                // A finite zero relative velocity is the exact pre-WP1 static
                // path, including its single one-pole correction after the
                // graph's seeded listener pose changes. This branch protects
                // the immutable legacy PCM fingerprint.
                state
                    .propagation_delay
                    .observe_block_target_with_zero_motion(delay_target_samples);
            } else {
                // A tangency can have zero radial velocity while the full
                // relative velocity is nonzero, so only full zero bypasses the
                // retarded-time path.
                state.propagation_delay.observe_block_target_with_motion(
                    delay_target_samples,
                    radial_velocity_mps,
                    relative_speed_mps,
                );
            }
            state.last_propagation_observation = Some(observation);
            if let Some(stereo) = &mut state.stereo_image {
                if relative_speed_mps == 0.0 {
                    stereo.delay.observe_block_target_with_zero_motion(delay_target_samples);
                } else {
                    stereo.delay.observe_block_target_with_motion(
                        delay_target_samples,
                        radial_velocity_mps,
                        relative_speed_mps,
                    );
                }
            }
        }
        if let Some(shaper) = &mut state.impulse_shaper {
            let distance_m =
                smoothed_source_distance_m(smoothed.source_position, smoothed.listener_position);
            let parameters = shaper.parameters_at_distance(distance_m);
            for (frame, input) in input_mono.iter().copied().enumerate() {
                let shaped = shaper.process_sample(input, parameters);
                let delayed = state.propagation_delay.process_sample(shaped);
                if state.guard_reactivation_history {
                    state.reactivation_epoch_samples =
                        state.reactivation_epoch_samples.saturating_add(1);
                    if state.reactivation_epoch_samples
                        <= state
                            .propagation_delay
                            .required_reactivation_history_samples()
                    {
                        self.mono_work[frame] = 0.0;
                    } else {
                        state.guard_reactivation_history = false;
                        self.mono_work[frame] = delayed;
                    }
                } else {
                    self.mono_work[frame] = delayed;
                }
                self.roof_work[frame] = state.roof_head.read_mono(
                    &state.propagation_delay, propagation.over_roof,
                    self.mono_work[frame], self.audio.sample_rate_hz,
                );
            }
        } else {
            // Structural Wave 12 bypass: existing and explicitly `None`
            // sources execute the pre-change dry-to-delay loop bit-for-bit.
            for (frame, input) in input_mono.iter().copied().enumerate() {
                let delayed = state.propagation_delay.process_sample(input);
                if state.guard_reactivation_history {
                    state.reactivation_epoch_samples =
                        state.reactivation_epoch_samples.saturating_add(1);
                    if state.reactivation_epoch_samples
                        <= state
                            .propagation_delay
                            .required_reactivation_history_samples()
                    {
                        self.mono_work[frame] = 0.0;
                    } else {
                        state.guard_reactivation_history = false;
                        self.mono_work[frame] = delayed;
                    }
                } else {
                    self.mono_work[frame] = delayed;
                }
                self.roof_work[frame] = state.roof_head.read_mono(
                    &state.propagation_delay, propagation.over_roof,
                    self.mono_work[frame], self.audio.sample_rate_hz,
                );
            }
        }
        if stereo_program {
            let stereo = state.stereo_image.as_mut().expect("validated stereo extent");
            let distance_m =
                smoothed_source_distance_m(smoothed.source_position, smoothed.listener_position);
            for frame in 0..input_mono.len() {
                let mut input = [
                    source_block.program_planes[0][frame],
                    source_block.program_planes[1][frame],
                ];
                for (sample, shaper) in input.iter_mut().zip(&mut stereo.impulse_shapers) {
                    if let Some(shaper) = shaper {
                        *sample = shaper.process_sample(
                            *sample,
                            shaper.parameters_at_distance(distance_m),
                        );
                    }
                }
                let delayed = stereo.delay.process_frame(input, 2)
                    .expect("validated stereo program plane count");
                let extra = stereo.roof_head.next_extra_samples(propagation.over_roof, self.audio.sample_rate_hz);
                let delayed = if extra == 0.0 { delayed } else {
                    stereo.delay.read_behind_newest(stereo.delay.current_delay_samples()+extra, stereo.roof_head.history_samples())
                };
                self.width_work[frame * 2] = delayed[0];
                self.width_work[frame * 2 + 1] = delayed[1];
            }
        }
        state.rendered_since_reset = true;
        let frames = input_mono.len();
        // Untouched distant voices have no DSP tail. Wake on the raw onset,
        // before its delayed samples reach the native effects. Played voices
        // keep advancing their filters and tails through ordinary silence.
        let pristine_silent = self.retire_silent_reflections
            && !stereo_program
            && !state.program_history
            && state.propagation_delay.current_delay_samples() >= frames as f32 * 2.0;
        state.route_head.observe(
            route.map(|route| RouteDelayTarget::from_route(route, self.audio.sample_rate_hz)),
            state.propagation_delay.current_delay_samples(),
            &self.mono_work[..frames],
        );
        let route_timed = state.route_head.is_heard();
        if route_timed {
            for (frame, (routed, line_output)) in self
                .route_work
                .iter_mut()
                .zip(&self.mono_work)
                .take(frames)
                .enumerate()
            {
                *routed = state.route_head.process(
                    &state.propagation_delay,
                    *line_output,
                    frames - 1 - frame,
                );
            }
        }
        // Direct reads the single selected roof/transmission head. Reflections
        // retain mono_work's original straight-line propagation clock.
        state.input.write_mono(&mut self.roof_work);

        let mut input = state.input.raw();
        drop(profile_prepare);
        let profile_direct = crate::render_profile::timer(2);
        if quality_ramps[0].is_audible() && !pristine_silent {
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
            if stereo_program {
                let stereo = state.stereo_image.as_mut().expect("validated stereo extent");
                let right_enu = normalized_api(cross_api(
                    steam_vector_to_api(propagation.source_forward),
                    steam_vector_to_api(propagation.source_up),
                )).unwrap_or(ApiEnuVector3::new(1.0, 0.0, 0.0));
                let geometry = line_geometry(
                    smoothed.arrival_position,
                    api_enu_to_steam(right_enu),
                    smoothed.listener_position,
                    stereo.width_m,
                );
                stereo.input.write_interleaved(&mut self.width_work[..frames * 2]);
                let mut stereo_input = stereo.input.raw();
                let mut stereo_direct = stereo.direct.raw();
                ffi::direct_effect_apply(
                    handle(stereo.direct_effect),
                    &mut direct_params,
                    &mut stereo_input,
                    &mut stereo_direct,
                );
                stereo.direct.read_interleaved(&mut self.width_work[..frames * 2]);
                let center_distance = smoothed_source_distance_m(
                    smoothed.arrival_position,
                    smoothed.listener_position,
                ).max(1.0);
                let feeds = [
                    (stereo.left_binaural_effect, geometry.minus_endpoint),
                    (stereo.right_binaural_effect, geometry.plus_endpoint),
                ];
                for (channel, (binaural_effect, position)) in feeds.into_iter().enumerate() {
                    let endpoint_distance = smoothed_source_distance_m(
                        position,
                        smoothed.listener_position,
                    ).max(1.0);
                    let drive = 0.5 * center_distance / endpoint_distance;
                    for (frame, sample) in self.width_feed_work.iter_mut().enumerate() {
                        *sample = self.width_work[frame * 2 + channel] * drive;
                    }
                    if let Some(tap) = &mut self.spatial_export {
                        tap.feed(source_block.source_index,
                            if channel == 0 { SpatialPresentationComponent::WidthNegative }
                                else { SpatialPresentationComponent::WidthPositive },
                            position, smoothed.listener_position, propagation,
                            &self.width_feed_work, stage_output_gains.direct, quality_ramps[0], 0);
                    }
                    state.direct_mono.write_mono(&mut self.width_feed_work);
                    let mut direct_mono = state.direct_mono.raw();
                    let mut direct_stereo = state.direct_stereo.raw();
                    let mut binaural_params = ffi::IPLBinauralEffectParams {
                        direction: relative_direction_steam(
                            position,
                            smoothed.listener_position,
                            listener,
                        ),
                        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
                        spatialBlend: 1.0,
                        hrtf: handle(self.hrtf),
                        peakDelays: core::ptr::null_mut(),
                    };
                    ffi::binaural_effect_apply(
                        handle(binaural_effect),
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
                    accumulate_stereo_ramped(
                        &self.stereo_work,
                        &mut self.live_direct_path_left,
                        &mut self.live_direct_path_right,
                        stage_output_gains.direct,
                        quality_ramps[0],
                    );
                }
            } else if let Some(width) = &mut state.width {
                let geometry = line_geometry(
                    smoothed.arrival_position,
                    propagation.source_forward,
                    smoothed.listener_position,
                    width.length_m,
                );
                width.renderer.render_presentation(
                    &self.roof_work,
                    geometry.k,
                    width_teleported,
                    &mut self.width_work,
                );
                width.presentation.write_interleaved(&mut self.width_work);
                let mut width_input = width.presentation.raw();
                let mut width_direct = width.direct.raw();
                ffi::direct_effect_apply(
                    handle(state.direct_effect),
                    &mut direct_params,
                    &mut width_input,
                    &mut width_direct,
                );
                width.direct.read_interleaved(&mut self.width_work);

                let feeds = [
                    (state.binaural_effect, geometry.center),
                    (width.plus_binaural_effect, geometry.plus_endpoint),
                    (width.minus_binaural_effect, geometry.minus_endpoint),
                ];
                for (channel, (binaural_effect, position)) in feeds.into_iter().enumerate() {
                    for (frame, sample) in self.width_feed_work.iter_mut().enumerate() {
                        *sample = self.width_work[frame * 3 + channel];
                    }
                    if let Some(tap) = &mut self.spatial_export {
                        tap.feed(source_block.source_index,
                            [SpatialPresentationComponent::DirectCenter,
                                SpatialPresentationComponent::WidthPositive,
                                SpatialPresentationComponent::WidthNegative][channel],
                            position, smoothed.listener_position, propagation,
                            &self.width_feed_work, stage_output_gains.direct, quality_ramps[0],
                            DECLARED_LATENCY_SAMPLES as u32);
                    }
                    state.direct_mono.write_mono(&mut self.width_feed_work);
                    let mut direct_mono = state.direct_mono.raw();
                    let mut binaural_params = ffi::IPLBinauralEffectParams {
                        direction: relative_direction_steam(
                            position,
                            smoothed.listener_position,
                            listener,
                        ),
                        interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
                        spatialBlend: 1.0,
                        hrtf: handle(self.hrtf),
                        peakDelays: core::ptr::null_mut(),
                    };
                    width.silent_binaural[channel].render(
                        self.retire_silent_reflections && frames == 128
                            && input_mono.iter().all(|sample| *sample == 0.0),
                        &self.width_feed_work,
                        binaural_effect,
                        &mut binaural_params,
                        &mut direct_mono,
                        &mut state.direct_stereo,
                        &mut self.stereo_work,
                    );
                    accumulate_stereo_ramped(
                        &self.stereo_work,
                        output_left,
                        output_right,
                        stage_output_gains.direct,
                        quality_ramps[0],
                    );
                    accumulate_stereo_ramped(
                        &self.stereo_work,
                        &mut self.live_direct_path_left,
                        &mut self.live_direct_path_right,
                        stage_output_gains.direct,
                        quality_ramps[0],
                    );
                }
            } else {
                // Structural legacy bypass. Point, MultiPoint, StereoImage, and
                // the default/extent-absent descriptor execute the unchanged
                // mono DirectEffect -> point BinauralEffect path.
                let mut binaural_params = ffi::IPLBinauralEffectParams {
                    direction: relative_direction_steam(
                        smoothed.arrival_position,
                        smoothed.listener_position,
                        listener,
                    ),
                    interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
                    spatialBlend: 1.0,
                    hrtf: handle(self.hrtf),
                    peakDelays: core::ptr::null_mut(),
                };
                state.direct_silent_pair.render(
                    self.retire_silent_reflections && frames == 128 && state.stereo_image.is_none(),
                    &self.roof_work,
                    state.direct_effect,
                    state.binaural_effect,
                    &mut direct_params,
                    &mut binaural_params,
                    &mut input,
                    &mut state.direct_mono,
                    &mut state.direct_stereo,
                    &mut self.stereo_work,
                );
                if let Some(tap) = &mut self.spatial_export {
                    tap.direct_buffer(source_block.source_index, smoothed.arrival_position,
                        smoothed.listener_position, propagation, &mut state.direct_mono,
                        stage_output_gains.direct, quality_ramps[0]);
                }
                accumulate_stereo_ramped(
                    &self.stereo_work,
                    output_left,
                    output_right,
                    stage_output_gains.direct,
                    quality_ramps[0],
                );
                accumulate_stereo_ramped(
                    &self.stereo_work,
                    &mut self.live_direct_path_left,
                    &mut self.live_direct_path_right,
                    stage_output_gains.direct,
                    quality_ramps[0],
                );
            }
        }

        drop(profile_direct);
        state.input.write_mono(&mut self.mono_work);
        input = state.input.raw();
        let profile_path = crate::render_profile::timer(3);
        // PathEffect likewise retains its EQ/SH parameter frame and
        // interpolates toward these one-pole endpoints within the block.
        if quality_ramps[1].is_audible() && !pristine_silent {
            if route_timed {
                state.input.write_mono(&mut self.route_work);
                input = state.input.raw();
            }
            let mut path_coefficients = smoothed.path_sh;
            let mut path_params = ffi::IPLPathEffectParams {
                eqCoeffs: realizable_eq(smoothed.path_eq.map(f64::from)),
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
            state.path_silent.render(
                self.retire_silent_reflections && frames == 128 && state.stereo_image.is_none(),
                if route_timed { &self.route_work } else { &self.mono_work },
                &path_coefficients,
                state.path_effect,
                &mut path_params,
                &mut input,
                &mut state.path_stereo,
                &mut self.stereo_work,
            );
            if let Some(tap) = &mut self.spatial_export {
                tap.path(source_block.source_index, &mut path_params, &mut input,
                    stage_output_gains.pathing, quality_ramps[1]);
            }
            accumulate_stereo_ramped(
                &self.stereo_work,
                output_left,
                output_right,
                stage_output_gains.pathing,
                quality_ramps[1],
            );
            accumulate_stereo_ramped(
                &self.stereo_work,
                &mut self.live_direct_path_left,
                &mut self.live_direct_path_right,
                stage_output_gains.pathing,
                quality_ramps[1],
            );
        }

        drop(profile_path);
        let _profile_reflection = crate::render_profile::timer(5);
        let shared_target = self.reflection_share_targets[source_block.source_index];
        let previous_target = self.reflection_previous_share_targets[source_block.source_index];
        let sharing = shared_target != source_block.source_index;
        let share_ramp = GainRamp::new(self.reflection_share_gains[source_block.source_index],
            if sharing { 1.0 } else { 0.0 }, self.audio.frame_size as usize);
        self.reflection_share_gains[source_block.source_index] = if sharing { 1.0 } else { 0.0 };
        let destination = if sharing { shared_target } else { previous_target };
        if destination != source_block.source_index {
            let start = destination * self.audio.frame_size as usize;
            for (frame, sample) in self.mono_work.iter().copied().enumerate() {
                self.reflection_shared_work[start + frame] += sample * share_ramp.at(frame);
            }
        }
        let shared_start = source_block.source_index * self.audio.frame_size as usize;
        let shared_input = self.reflection_shared_work.get(shared_start..shared_start + self.audio.frame_size as usize);
        let shared_awake = shared_input.is_some_and(|samples| samples.iter().any(|x| *x != 0.0));
        let reflection = propagation.reflections;
        let asynchronous_reflections = self.world.reflection_worker_enabled.load(Ordering::Acquire);
        let reflection_ir_hold = if asynchronous_reflections {
            self.world.reflection_worker_hold_ir.load(Ordering::Relaxed)
        } else { self.reflection_ir_hold };
        if !self.tail_retiring
            && (quality_ramps[2].is_audible()
                || state.reflection_activity.has_history()
                || state.reflection_activity.sequence != propagation.reflection_sequence
                || (asynchronous_reflections
                    && state.reflection_adopted_sequence != propagation.reflection_sequence))
            && (reflection.ir != 0
                || reflection_effect_uses_reverb(self.config.reflection_effect.effect_type))
        {
            for (frame, sample) in self.mono_work.iter_mut().enumerate() {
                *sample *= quality_ramps[2].at(frame);
                if let Some(shared) = shared_input { *sample += shared[frame]; }
            }
            // A dry onset wakes IR adoption before the distance-delayed send
            // arrives. Active inputs and tails continue while swaps are spread.
            state.reflection_activity.observe_dry(
                shared_awake || input_mono.iter().any(|sample| *sample != 0.0),
                maximum_propagation_delay_samples(self.audio.sample_rate_hz) as u64 + self.audio.frame_size as u64 * 2,
            );
            let active = state.reflection_activity.advance(
                self.mono_work.iter().any(|sample| *sample != 0.0),
                self.audio.frame_size as u64,
                propagation.reflection_sequence,
                // A dry onset primes a distant send before its first delayed
                // sample arrives. Nearby sends keep staggered silent handoffs.
                self.idle_reflection_source == source_block.source_index
                    && state.propagation_delay.current_delay_samples()
                        < self.audio.frame_size as f32 * 2.0,
            );
            // Async native IRs may arrive before their Rust metadata. Hold the
            // mailbox until a new sequence is published and selected here.
            let adopt = reflection_ir_hold == 0
                || self.reflection_adoption_source == Some(source_block.source_index);
            let cold_adoption = reflection_ir_hold != 0
                && state.reflection_channels == 0 && adopt;
            let held = reflection_ir_hold != 0 && !adopt;
            let applied = if held && state.reflection_channels != 0 {
                state.applied_reflections
            } else {
                reflection
            };
            let mut reflection_params = reflection_effect_params(applied, self.config);
            reflection_params.irSize = reflection_params.irSize.min(state.reflection_ir_capacity);
            if held {
                // 4.8.1 consumes input normally when a valid mailbox has no
                // publication; each effect retains its own current FFT IR.
                reflection_params.ir = handle(reflection_ir_hold);
            }
            // Steam crossfades both IR spectra over this block. Keep the old
            // channels through adoption; the new IR's removed planes are zero.
            reflection_params.numChannels = state.reflection_channels.max(applied.num_channels);
            self.reflection_block_order = self.reflection_block_order.max(
                reflection_order_for_channels(reflection_params.numChannels),
            );
            if active || cold_adoption || (asynchronous_reflections && adopt) || !self.retire_silent_reflections {
                state.input.write_mono(&mut self.mono_work);
                input = state.input.raw();
                let mut scratch = state.reflection_scratch.raw();
                crate::render_profile::reflection_apply_event(
                    adopt && state.reflection_adopted_sequence != propagation.reflection_sequence,
                    held,
                );
                let profile_convolution = crate::render_profile::reflection_timer(0);
                ffi::reflection_effect_apply_to_mixer(
                    handle(state.reflection_effect),
                    &mut reflection_params,
                    &mut input,
                    &mut scratch,
                    handle(self.reflection_mixer),
                );
                drop(profile_convolution);
                if adopt {
                    state.reflection_channels = reflection.num_channels;
                    state.reflection_adopted_sequence = propagation.reflection_sequence;
                    state.applied_reflections = reflection;
                    self.world.reflection_acknowledged[source_block.source_index]
                        .store(propagation.reflection_sequence, Ordering::Release);
                }
            }
        }
    }

    fn render_reflection_mix(
        &mut self,
        listener: SteamPose,
        output_left: &mut [f32],
        output_right: &mut [f32],
        gain: f32,
        governor_quality: GovernorRenderSnapshot,
        live_energy: &mut StageEnergyAccumulator,
    ) {
        let _profile_reflection = crate::render_profile::timer(5);
        let mut mixer_params = ffi::IPLReflectionEffectParams {
            type_: reflection_effect_ffi_type(self.config.reflection_effect.effect_type)
                .expect("validated reflection effect"),
            ir: core::ptr::null_mut(),
            reverbTimes: [0.0; 3],
            eq: [1.0; 3],
            delay: 0,
            numChannels: ambisonics_channel_count(self.reflection_block_order)
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
        let profile_mixer = crate::render_profile::reflection_timer(1);
        ffi::reflection_mixer_apply(
            handle(self.reflection_mixer),
            &mut mixer_params,
            &mut reflection_mix,
        );
        drop(profile_mixer);
        let reflection_gain_ramp = GainRamp::new(
            self.reflection_output_gain,
            governor_quality.reflection_output_gain,
            self.audio.frame_size as usize,
        );
        if let Some(tap) = &mut self.spatial_export {
            tap.reflection(&mut self.reflection_mix, self.reflection_block_order, gain, reflection_gain_ramp);
        }
        let mut decode_params = ffi::IPLAmbisonicsDecodeEffectParams {
            order: self.reflection_block_order,
            hrtf: handle(self.hrtf),
            orientation: coordinate_space(listener).expect("validated listener orientation"),
            binaural: ffi::IPL_TRUE,
        };
        let mut reflection_stereo = self.reflection_stereo.raw();
        let profile_decode = crate::render_profile::reflection_timer(2);
        self.reflection_decode_tail_remaining = ffi::ambisonics_decode_effect_apply(
            handle(self.ambisonics_decode),
            &mut decode_params,
            &mut reflection_mix,
            &mut reflection_stereo,
        );
        drop(profile_decode);
        self.reflection_stereo
            .read_interleaved(&mut self.stereo_work);
        self.reflection_output_gain = governor_quality.reflection_output_gain;
        accumulate_stereo_ramped(
            &self.stereo_work,
            output_left,
            output_right,
            gain,
            reflection_gain_ramp,
        );
        live_energy.reflection_energy +=
            stereo_energy_ramped(&self.stereo_work, gain, reflection_gain_ramp);
    }

    fn render_reflection_decode_tail(
        &mut self,
        output_left: &mut [f32],
        output_right: &mut [f32],
        gain: f32,
        governor_quality: GovernorRenderSnapshot,
        live_energy: &mut StageEnergyAccumulator,
    ) {
        let mut reflection_stereo = self.reflection_stereo.raw();
        self.reflection_decode_tail_remaining = ffi::ambisonics_decode_effect_get_tail(
            handle(self.ambisonics_decode),
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
        live_energy.reflection_energy +=
            stereo_energy_ramped(&self.stereo_work, gain, reflection_gain_ramp);
    }
}

// Convolution has finite history. Normal zero applies clear every dry FFT
// partition and overlap before sleep; GetTail cannot safely resume later.
#[derive(Clone, Copy, Debug)]
struct ReflectionActivity {
    drain_frames: u64,
    quiet_frames: u64,
    sequence: u64,
    wake_frames: u64,
}

impl ReflectionActivity {
    fn new(drain_frames: u64) -> Self {
        Self { drain_frames, quiet_frames: drain_frames, sequence: 0, wake_frames: 0 }
    }

    fn reset(&mut self) {
        self.quiet_frames = self.drain_frames;
        self.sequence = 0;
        self.wake_frames = 0;
    }

    fn has_history(self) -> bool {
        self.quiet_frames < self.drain_frames
    }

    fn observe_dry(&mut self, input_present: bool, transport_frames: u64) {
        if input_present {
            self.wake_frames = self.wake_frames.max(transport_frames);
        }
    }

    fn advance(&mut self, input_present: bool, frames: u64, sequence: u64, prime_due: bool) -> bool {
        let had_history = self.has_history();
        self.quiet_frames = if input_present { 0 } else {
            self.quiet_frames.saturating_add(frames).min(self.drain_frames)
        };
        let new_ir = self.sequence != sequence && (prime_due || self.wake_frames != 0);
        self.wake_frames = self.wake_frames.saturating_sub(frames);
        let apply = input_present || had_history || new_ir;
        if apply { self.sequence = sequence; }
        apply
    }
}

fn reflection_order_for_channels(channels: i32) -> i32 {
    let mut order = 0;
    while (order + 2) * (order + 2) <= channels {
        order += 1;
    }
    order
}

#[derive(Clone, Copy, Debug, Default)]
struct StageEnergyAccumulator {
    audible_source_count: u8,
    direct_path_energy: f64,
    reflection_energy: f64,
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

fn stereo_energy_ramped(interleaved: &[f32], stage_gain: f32, ramp: GainRamp) -> f64 {
    if stage_gain == 0.0 || !ramp.is_audible() {
        return 0.0;
    }
    interleaved
        .chunks_exact(2)
        .enumerate()
        .map(|(frame_index, frame)| {
            let gain = stage_gain * ramp.at(frame_index);
            let left = f64::from(frame[0] * gain);
            let right = f64::from(frame[1] * gain);
            left * left + right * right
        })
        .sum()
}

fn stereo_planes_energy(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .chain(right)
        .map(|sample| {
            let sample = f64::from(*sample);
            sample * sample
        })
        .sum()
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
            if let Some(width) = &mut source.width {
                let mut plus = handle(width.plus_binaural_effect);
                ffi::binaural_effect_release(&mut plus);
                let mut minus = handle(width.minus_binaural_effect);
                ffi::binaural_effect_release(&mut minus);
            }
            if let Some(echo) = &mut source.echo {
                for tap_index in 0..MAX_ECHO_TAPS_PER_SOURCE {
                    let mut direct = handle(echo.tap_direct_effects[tap_index]);
                    ffi::direct_effect_release(&mut direct);
                    let mut binaural = handle(echo.tap_binaural_effects[tap_index]);
                    ffi::binaural_effect_release(&mut binaural);
                }
            }
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum NeutralPresentationShape {
    Point,
    LineSegment { length_m: f32 },
    StereoImage { width_m: f32 },
}

impl NeutralPresentationShape {
    fn from_descriptor(
        descriptor: crate::MultiSourceDescriptor,
        program_plane_count: usize,
    ) -> Result<Self, BackendError> {
        match (descriptor.extent, program_plane_count) {
            (ExtentDescriptor::Point | ExtentDescriptor::MultiPoint { .. }, 1) => Ok(Self::Point),
            (ExtentDescriptor::LineSegment { length_m }, 1) => Ok(Self::LineSegment { length_m }),
            (ExtentDescriptor::StereoImage { width_m }, 2) => Ok(Self::StereoImage { width_m }),
            (ExtentDescriptor::StereoImage { .. }, 1) => Err(BackendError::InvalidInput(
                "neutral Wave 0 does not implement mono-expanded StereoImage presentation",
            )),
            (ExtentDescriptor::Point | ExtentDescriptor::MultiPoint { .. }, 2) => {
                Err(BackendError::InvalidInput(
                    "neutral Wave 0 rejects stereo Point and MultiPoint presentation",
                ))
            }
            (ExtentDescriptor::LineSegment { .. }, 2) => Err(BackendError::InvalidInput(
                "neutral Wave 0 defers stereo LineSegment presentation",
            )),
            (_, _) => Err(BackendError::InvalidInput(
                "neutral source program channel count must be one or two",
            )),
        }
    }

    const fn direct_channel_count(self) -> i32 {
        match self {
            Self::Point => 1,
            Self::LineSegment { .. } => 3,
            Self::StereoImage { .. } => 2,
        }
    }

    const fn admits_indirect(self) -> bool {
        // AuthoredStereo needs an asset-authoritative center derivative before
        // it can drive one shared path/reflection field. Wave 0 deliberately
        // suppresses that send instead of inventing an L+R fold that could
        // comb or cancel valid authored material.
        !matches!(self, Self::StereoImage { .. })
    }
}

struct NeutralLineRenderState {
    renderer: LineWidthRenderer,
    presentation: OwnedAudioBuffer,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NeutralReleaseCounts {
    direct: usize,
    path: usize,
    reflection: usize,
    mixer: usize,
}

#[cfg(test)]
std::thread_local! {
    static NEUTRAL_RELEASE_COUNTS: std::cell::Cell<NeutralReleaseCounts> =
        const { std::cell::Cell::new(NeutralReleaseCounts {
            direct: 0,
            path: 0,
            reflection: 0,
            mixer: 0,
        }) };
}

#[cfg(test)]
fn reset_neutral_release_counts() {
    NEUTRAL_RELEASE_COUNTS.with(|counts| counts.set(NeutralReleaseCounts::default()));
}

#[cfg(test)]
fn neutral_release_counts() -> NeutralReleaseCounts {
    NEUTRAL_RELEASE_COUNTS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn record_neutral_release(update: impl FnOnce(&mut NeutralReleaseCounts)) {
    NEUTRAL_RELEASE_COUNTS.with(|counts| {
        let mut current = counts.get();
        update(&mut current);
        counts.set(current);
    });
}

struct NeutralDirectEffect(usize);

impl Drop for NeutralDirectEffect {
    fn drop(&mut self) {
        let mut effect = handle(self.0);
        ffi::direct_effect_release(&mut effect);
        #[cfg(test)]
        record_neutral_release(|counts| counts.direct += 1);
    }
}

struct NeutralPathEffect(usize);

impl Drop for NeutralPathEffect {
    fn drop(&mut self) {
        let mut effect = handle(self.0);
        ffi::path_effect_release(&mut effect);
        #[cfg(test)]
        record_neutral_release(|counts| counts.path += 1);
    }
}

struct NeutralReflectionEffect(usize);

impl Drop for NeutralReflectionEffect {
    fn drop(&mut self) {
        let mut effect = handle(self.0);
        ffi::reflection_effect_release(&mut effect);
        #[cfg(test)]
        record_neutral_release(|counts| counts.reflection += 1);
    }
}

struct NeutralReflectionMixer(usize);

impl Drop for NeutralReflectionMixer {
    fn drop(&mut self) {
        let mut mixer = handle(self.0);
        ffi::reflection_mixer_release(&mut mixer);
        #[cfg(test)]
        record_neutral_release(|counts| counts.mixer += 1);
    }
}

struct NeutralMonoProgramDelay {
    delay: PropagationDelayLine,
    guard_reactivation_history: bool,
    reactivation_epoch_samples: u64,
}

impl NeutralMonoProgramDelay {
    fn new(maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        Self {
            delay: PropagationDelayLine::new(maximum_delay_samples, sample_rate_hz),
            guard_reactivation_history: false,
            reactivation_epoch_samples: 0,
        }
    }

    fn invalidate(&mut self) {
        self.delay.invalidate();
        self.guard_reactivation_history = true;
        self.reactivation_epoch_samples = 0;
    }

    fn process_sample(&mut self, input: f32) -> f32 {
        let delayed = self.delay.process_sample(input);
        if !self.guard_reactivation_history {
            return delayed;
        }
        self.reactivation_epoch_samples = self.reactivation_epoch_samples.saturating_add(1);
        if self.reactivation_epoch_samples
            <= self.delay.required_reactivation_history_samples() as u64
        {
            0.0
        } else {
            self.guard_reactivation_history = false;
            delayed
        }
    }
}

enum NeutralProgramDelay {
    Mono(NeutralMonoProgramDelay),
    Stereo(StereoProgramPropagationDelay),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct NeutralProgramDelayMemory {
    audio_history_payload_bytes: u64,
    geometry_history_payload_bytes: u64,
    additional_channel_payload_bytes: u64,
}

impl NeutralProgramDelay {
    fn new(channel_count: usize, maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        match channel_count {
            1 => Self::Mono(NeutralMonoProgramDelay::new(
                maximum_delay_samples,
                sample_rate_hz,
            )),
            2 => Self::Stereo(StereoProgramPropagationDelay::new(
                maximum_delay_samples,
                sample_rate_hz,
            )),
            _ => unreachable!("neutral source shape is validated before construction"),
        }
    }

    // Kept as the explicit position-only route for callers that do not own a
    // finite velocity observation. Production direct snapshots use one of the
    // two velocity-qualified methods below.
    #[allow(dead_code)]
    fn observe_block_target(&mut self, target_samples: f32) {
        match self {
            Self::Mono(delay) => delay.delay.observe_block_target(target_samples),
            Self::Stereo(delay) => delay.observe_block_target(target_samples),
        }
    }

    fn observe_block_target_with_zero_motion(&mut self, target_samples: f32) {
        match self {
            Self::Mono(delay) => delay
                .delay
                .observe_block_target_with_zero_motion(target_samples),
            Self::Stereo(delay) => delay.observe_block_target_with_zero_motion(target_samples),
        }
    }

    fn observe_block_target_with_motion(
        &mut self,
        target_samples: f32,
        radial_velocity_mps: f32,
        relative_speed_mps: f32,
    ) {
        match self {
            Self::Mono(delay) => delay.delay.observe_block_target_with_motion(
                target_samples,
                radial_velocity_mps,
                relative_speed_mps,
            ),
            Self::Stereo(delay) => delay.observe_block_target_with_motion(
                target_samples,
                radial_velocity_mps,
                relative_speed_mps,
            ),
        }
    }

    fn invalidate(&mut self) {
        match self {
            Self::Mono(delay) => delay.invalidate(),
            Self::Stereo(delay) => delay.invalidate(),
        }
    }

    fn reset_to(&mut self, target_samples: f32) {
        match self {
            Self::Mono(delay) => delay.delay.reset_to(target_samples),
            Self::Stereo(delay) => delay.reset_to(target_samples),
        }
    }

    fn process_frame(
        &mut self,
        input: [f32; 2],
        channel_count: usize,
    ) -> Result<[f32; 2], SpatialBackendRenderError> {
        match (self, channel_count) {
            (Self::Mono(delay), 1) => Ok([delay.process_sample(input[0]), 0.0]),
            (Self::Stereo(delay), 2) => delay
                .process_frame(input, channel_count)
                .map_err(|_| SpatialBackendRenderError::InvalidProgramPlaneCount),
            _ => Err(SpatialBackendRenderError::InvalidProgramPlaneCount),
        }
    }

    #[cfg(test)]
    fn instrumentation(
        &self,
    ) -> Option<crate::propagation_delay::StereoProgramDelayInstrumentation> {
        match self {
            Self::Mono(_) => None,
            Self::Stereo(delay) => Some(delay.instrumentation()),
        }
    }

    fn read_roof_frame(&self, extra: f32, history: usize) -> [f32; 2] {
        match self {
            Self::Mono(delay) => [delay.delay.read_behind_newest(delay.delay.current_delay_samples()+extra, history), 0.0],
            Self::Stereo(delay) => delay.read_behind_newest(delay.current_delay_samples()+extra, history),
        }
    }

    const fn channel_count(&self) -> usize {
        match self {
            Self::Mono(_) => 1,
            Self::Stereo(_) => 2,
        }
    }

    fn memory(&self) -> NeutralProgramDelayMemory {
        match self {
            Self::Mono(delay) => {
                let memory = delay.delay.memory();
                debug_assert_eq!(
                    memory.total_heap_payload_bytes,
                    memory
                        .audio_history_payload_bytes
                        .saturating_add(memory.geometry_history_payload_bytes)
                );
                NeutralProgramDelayMemory {
                    audio_history_payload_bytes: memory.audio_history_payload_bytes as u64,
                    geometry_history_payload_bytes: memory.geometry_history_payload_bytes as u64,
                    additional_channel_payload_bytes: 0,
                }
            }
            Self::Stereo(delay) => {
                let memory = delay.memory();
                debug_assert_eq!(
                    memory.total_heap_payload_bytes,
                    memory
                        .audio_history_payload_bytes
                        .saturating_add(memory.geometry_history_payload_bytes)
                );
                NeutralProgramDelayMemory {
                    audio_history_payload_bytes: memory.audio_history_payload_bytes as u64,
                    geometry_history_payload_bytes: memory.geometry_history_payload_bytes as u64,
                    additional_channel_payload_bytes: memory.additional_channel_payload_bytes
                        as u64,
                }
            }
        }
    }
}

struct NeutralSourceRenderState {
    direct_effect: NeutralDirectEffect,
    path_effect: Option<NeutralPathEffect>,
    reflection_effect: Option<NeutralReflectionEffect>,
    reflection_ir_capacity: i32,
    program_input: OwnedAudioBuffer,
    indirect_input: Option<OwnedAudioBuffer>,
    direct_output: OwnedAudioBuffer,
    path_field: Option<OwnedAudioBuffer>,
    reflection_scratch: Option<OwnedAudioBuffer>,
    propagation_smoother: SourcePropagationSmoother,
    roof_head: crate::over_roof::RoofReadHead,
    impulse_shapers: [Option<ImpulseShaper>; 2],
    pathing_send_enabled: bool,
    reflection_send_enabled: bool,
    render_active: bool,
    last_propagation_observation: Option<PropagationObservationKey>,
    quality_gains: [f32; 3],
    presentation: NeutralPresentationShape,
    line: Option<NeutralLineRenderState>,
}

/// Parallel pre-HRTF render graph for the Wave 0 neutral route.
///
/// The preserved [`MultiSourceRenderGraph`] remains the only owner of the
/// legacy HRTF/decode topology. This graph creates no HRTF, binaural effect, or
/// Ambisonic decoder. It emits direct presentation feeds and one unrotated
/// world-space environmental field into caller-owned fixed banks.
pub(crate) struct NeutralMultiSourceRenderGraph {
    config: S3SimulationConfig,
    audio: AudioConfig,
    environmental_order: i32,
    environmental_channels: usize,
    path_order: i32,
    path_channels: usize,
    reflection_channels: usize,
    sources: Vec<NeutralSourceRenderState>,
    metadata_city_offsets: Vec<ApiEnuVector3>,
    metadata_city_frame_enabled: Vec<bool>,
    program_plane_counts: [usize; MAX_ACTIVE_SOURCES],
    program_delays: Vec<NeutralProgramDelay>,
    delayed_program: Vec<[Vec<f32>; 2]>,
    roof_program_work: [Vec<f32>; 2],
    reflection_mixer: Option<NeutralReflectionMixer>,
    reflection_mix: Option<OwnedAudioBuffer>,
    program_interleaved_work: Vec<f32>,
    effect_interleaved_work: Vec<f32>,
    line_work: Vec<f32>,
    steam_environment_bank: Vec<f32>,
    publication: fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
    correlation_current: SteamPropagationSnapshot,
    correlation_previous: Option<SteamPropagationSnapshot>,
    correlation_history_hits: u64,
    correlation_misses: u64,
    governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    applied_governor_quality: GovernorRenderSnapshot,
    reflection_output_gain: f32,
    tail_retiring: bool,
    tail_retirement_state: SpatialTailRetirementState,
    tail_retirement_frames: u64,
    tail_retirement_deadline_frames: u64,
    propagation_block_retention: f32,
    prepared_for_realtime: bool,
    #[cfg(test)]
    prepared_reflection_effect_count: usize,
    memory: crate::SpatialRenderMemoryTelemetry,
    // Drop after every SDK effect and audio buffer so the shared context and
    // simulator generation outlive every neutral callback-side handle.
    world: Arc<WorldGeneration>,
}

/// Both unspatialized path rendering and the reflection mixer emit their
/// causal current-block output without backend lookahead. Physical path/IR
/// arrival time remains in the content and is not processing latency.
const NEUTRAL_ENVIRONMENTAL_LATENCY_FRAMES: u32 = 0;
// 80 ms propagation smoothing plus the callback-visible propagation history
// is the fixed preparation runway for a newly constructed real graph.
const NEUTRAL_PRE_CROSSFADE_WARMUP_BLOCKS: u8 = 64;

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NeutralCorrelationDiagnostics {
    current_direct_sequence: u64,
    previous_direct_sequence: Option<u64>,
    history_hits: u64,
    misses: u64,
}

impl NeutralMultiSourceRenderGraph {
    pub(crate) fn persistent_memory(&self) -> crate::SpatialRenderMemoryTelemetry {
        self.memory
    }

    #[cfg(test)]
    fn correlation_diagnostics(&self) -> NeutralCorrelationDiagnostics {
        NeutralCorrelationDiagnostics {
            current_direct_sequence: self.correlation_current.direct_sequence,
            previous_direct_sequence: self
                .correlation_previous
                .map(|snapshot| snapshot.direct_sequence),
            history_hits: self.correlation_history_hits,
            misses: self.correlation_misses,
        }
    }

    fn snapshot_for_direct_sequence(
        &mut self,
        requested_direct_sequence: u64,
    ) -> Result<SteamPropagationSnapshot, SpatialBackendRenderError> {
        let observed = self.publication.read();
        if observed.world_generation != self.world.generation {
            return Err(SpatialBackendRenderError::InactiveGraph);
        }

        if observed.direct_sequence == self.correlation_current.direct_sequence {
            // Path, reflection, and path-gate publications retain the current
            // direct token. Refresh their copied values without consuming the
            // one-entry direct history.
            self.correlation_current = observed;
        } else if observed.direct_sequence
            == self.correlation_current.direct_sequence.wrapping_add(1)
        {
            // SnapshotReader is a single-consumer monotonic publication. A new
            // adjacent direct token advances the exact two-entry window.
            self.correlation_previous = Some(self.correlation_current);
            self.correlation_current = observed;
        } else {
            // A skipped token means the reader cannot prove which generation
            // immediately preceded `observed`. Do not relabel an older cached
            // snapshot as adjacent; clear the recovery window and fail closed
            // for every token other than the newly observed one.
            self.correlation_previous = None;
            self.correlation_current = observed;
        }

        if self.correlation_current.direct_sequence == requested_direct_sequence {
            return Ok(self.correlation_current);
        }
        if let Some(previous) = self
            .correlation_previous
            .filter(|snapshot| snapshot.direct_sequence == requested_direct_sequence)
        {
            if previous.world_generation != self.world.generation {
                return Err(SpatialBackendRenderError::InactiveGraph);
            }
            self.correlation_history_hits = self.correlation_history_hits.saturating_add(1);
            return Ok(previous);
        }

        self.correlation_misses = self.correlation_misses.saturating_add(1);
        Err(SpatialBackendRenderError::PropagationSequenceMismatch)
    }

    /// Resolves Steam Audio's convolution and mixer lazy state away from the
    /// audio callback, then restores the graph to a bit-clean first-render
    /// state. The paired simulation must have published current direct,
    /// pathing, and reflection passes before this control-thread call.
    pub(crate) fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        // A failed repeat preparation must never leave an earlier prepared
        // state renderable through the low-level Rust seam.
        self.prepared_for_realtime = false;
        #[cfg(test)]
        {
            self.prepared_reflection_effect_count = 0;
        }

        // Binding validation applies even to the structural no-reflections
        // topology. The bypass below suppresses vendor priming only; it must
        // not make a graph from the wrong world generation renderable.
        let snapshot = self.publication.read();
        if snapshot.world_generation != self.world.generation {
            return Err(SpatialBackendRenderError::InactiveGraph);
        }

        let (Some(reflection_mixer), Some(_)) =
            (self.reflection_mixer.as_ref(), self.reflection_mix.as_ref())
        else {
            // Preserve the structural no-reflections bypass: no temporary
            // allocation, SDK call, or persistent accounting change.
            self.prepared_for_realtime = true;
            return Ok(());
        };
        let reflection_mixer = handle(reflection_mixer.0);

        let governor_quality = self.governor_quality.read();
        let expected_reflection_channels =
            ambisonics_channel_count(governor_quality.ambisonic_order)
                .map_err(|_| SpatialBackendRenderError::InactiveGraph)?;
        let maximum_ir_size = reflection_ir_size(
            governor_quality.reflections.ir_duration_s,
            self.audio.sample_rate_hz,
        )
        .map_err(|_| SpatialBackendRenderError::InactiveGraph)?;
        let mut mixer_params = self.reflection_mixer_params(governor_quality)?;
        let uses_ir = reflection_effect_uses_ir(self.config.reflection_effect.effect_type);
        for (index, state) in self.sources.iter().enumerate() {
            if state.reflection_effect.is_none() {
                continue;
            }
            let reflection = snapshot.sources[index].reflections;
            if (uses_ir
                && (reflection.ir == 0
                    || reflection.num_channels != expected_reflection_channels
                    || reflection.ir_size <= 0
                    || reflection.ir_size > maximum_ir_size))
                || !reflection
                    .reverb_times
                    .into_iter()
                    .chain(reflection.eq)
                    .all(f32::is_finite)
            {
                return Err(SpatialBackendRenderError::InactiveGraph);
            }
        }

        let frames = self.audio.frame_size as usize;
        self.program_interleaved_work[..frames].fill(0.0);
        for (index, state) in self.sources.iter_mut().enumerate() {
            let Some(reflection_effect) = state.reflection_effect.as_ref() else {
                continue;
            };
            let indirect_input = state
                .indirect_input
                .as_mut()
                .expect("reflection source owns mono send buffer");
            indirect_input.write_mono(&mut self.program_interleaved_work[..frames]);
            let mut raw_indirect_input = indirect_input.raw();
            let reflection_scratch = state
                .reflection_scratch
                .as_mut()
                .expect("reflection source owns reflection scratch");
            reflection_scratch.clear_with_interleaved_scratch(&mut self.effect_interleaved_work);
            let mut raw_reflection_scratch = reflection_scratch.raw();
            let mut reflection_params =
                reflection_effect_params(snapshot.sources[index].reflections, self.config);
            reflection_params.irSize = reflection_params.irSize.min(state.reflection_ir_capacity);
            ffi::reflection_effect_apply_to_mixer(
                handle(reflection_effect.0),
                &mut reflection_params,
                &mut raw_indirect_input,
                &mut raw_reflection_scratch,
                reflection_mixer,
            );
            #[cfg(test)]
            {
                self.prepared_reflection_effect_count += 1;
            }
        }

        let reflection_mix = self
            .reflection_mix
            .as_mut()
            .expect("reflection mixer owns its output buffer");
        reflection_mix.clear_with_interleaved_scratch(&mut self.effect_interleaved_work);
        let mut raw_reflection_mix = reflection_mix.raw();
        ffi::reflection_mixer_apply(reflection_mixer, &mut mixer_params, &mut raw_reflection_mix);

        // Reset only the vendor history touched above. Reset is intentionally
        // after one apply so Steam retains its resolved convolution workspace.
        for state in &mut self.sources {
            if let Some(reflection_effect) = &state.reflection_effect {
                ffi::reflection_effect_reset(handle(reflection_effect.0));
            }
            if let Some(indirect_input) = &mut state.indirect_input {
                indirect_input.clear_with_interleaved_scratch(&mut self.effect_interleaved_work);
            }
            if let Some(reflection_scratch) = &mut state.reflection_scratch {
                reflection_scratch
                    .clear_with_interleaved_scratch(&mut self.effect_interleaved_work);
            }
        }
        ffi::reflection_mixer_reset(reflection_mixer);
        self.reflection_mix
            .as_mut()
            .expect("reflection mixer owns its output buffer")
            .clear_with_interleaved_scratch(&mut self.effect_interleaved_work);
        self.program_interleaved_work.fill(0.0);
        self.effect_interleaved_work.fill(0.0);
        self.line_work.fill(0.0);
        self.steam_environment_bank.fill(0.0);
        self.prepared_for_realtime = true;
        Ok(())
    }

    #[cfg(test)]
    fn delay_instrumentation(
        &self,
        source_index: usize,
    ) -> Option<crate::propagation_delay::StereoProgramDelayInstrumentation> {
        self.program_delays
            .get(source_index)
            .and_then(NeutralProgramDelay::instrumentation)
    }

    #[cfg(test)]
    fn delayed_program_for_source(&self, source_index: usize) -> Option<[&[f32]; 2]> {
        self.delayed_program
            .get(source_index)
            .map(|planes| [planes[0].as_slice(), planes[1].as_slice()])
    }

    fn validate_block(
        &self,
        block: &SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        let frames = self.audio.frame_size as usize;
        if block.presentation_bank.len() != MAX_SPATIAL_PRESENTATION_FEEDS * frames
            || block.environmental_bank.len() != MAX_SPATIAL_ENVIRONMENT_PLANES * frames
        {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }
        if block.sources.len() > self.world.source_count {
            return Err(SpatialBackendRenderError::InvalidSourceIndex);
        }

        let mut seen = [false; MAX_ACTIVE_SOURCES];
        for source in block.sources {
            if source.source_index >= self.world.source_count {
                return Err(SpatialBackendRenderError::InvalidSourceIndex);
            }
            if seen[source.source_index] {
                return Err(SpatialBackendRenderError::InvalidSourceIndex);
            }
            seen[source.source_index] = true;
            if source.program_plane_count != self.program_plane_counts[source.source_index]
                || !(1..=2).contains(&source.program_plane_count)
            {
                return Err(SpatialBackendRenderError::InvalidProgramPlaneCount);
            }
            if source.program_planes[..source.program_plane_count]
                .iter()
                .any(|plane| plane.len() != frames || !plane.iter().copied().all(f32::is_finite))
                || source.program_planes[source.program_plane_count..]
                    .iter()
                    .any(|plane| !plane.is_empty())
            {
                return Err(SpatialBackendRenderError::InvalidBlockLength);
            }
        }
        Ok(())
    }

    fn reset_inactive_sources(&mut self, supplied: &[bool; MAX_ACTIVE_SOURCES]) {
        for (index, state) in self.sources.iter_mut().enumerate() {
            if supplied[index] || !state.render_active {
                continue;
            }
            state.render_active = false;
            state.propagation_smoother.reset();
            state.roof_head.invalidate();
            for shaper in state.impulse_shapers.iter_mut().flatten() {
                shaper.reset();
            }
            self.program_delays[index].invalidate();
            for plane in &mut self.delayed_program[index] {
                plane.fill(0.0);
            }
            if let Some(line) = &mut state.line {
                line.renderer.reset();
            }
            ffi::direct_effect_reset(handle(state.direct_effect.0));
            if let Some(path_effect) = &state.path_effect {
                ffi::path_effect_reset(handle(path_effect.0));
            }
            if let Some(reflection_effect) = &state.reflection_effect {
                ffi::reflection_effect_reset(handle(reflection_effect.0));
            }
            state.last_propagation_observation = None;
        }
    }

    fn prepare_delayed_program(
        &mut self,
        source_index: usize,
        program_planes: Option<[&[f32]; 2]>,
        propagation: SteamSourcePropagation,
        listener_position: SteamVector3,
        listener_linear_velocity_mps: SteamVector3,
        direct_sequence: u64,
    ) -> Result<(crate::motion_smoothing::SmoothedPropagationTerms, bool), SpatialBackendRenderError>
    {
        let program_plane_count = self.program_plane_counts[source_index];
        let state = &mut self.sources[source_index];
        let relative_speed_mps = relative_speed_mps(
            propagation.linear_velocity_mps,
            listener_linear_velocity_mps,
        );
        let smoothed = state
            .propagation_smoother
            .advance(
                propagation,
                listener_position,
                relative_speed_mps,
                self.propagation_block_retention,
            )
            .endpoint();

        let delay_target_samples = uncapped_propagation_delay_samples(
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
        let observation = propagation_observation_key(
            direct_sequence,
            delay_target_samples,
            radial_velocity_mps,
            relative_speed_mps,
        );
        let mut teleported = false;
        if propagation_observation_cache_miss(state.last_propagation_observation, observation) {
            teleported =
                state
                    .last_propagation_observation
                    .is_some_and(|(_, previous_delay_bits, _, _)| {
                        (delay_target_samples - f32::from_bits(previous_delay_bits)).abs()
                            > TELEPORT_DELAY_STEP_SECONDS * self.audio.sample_rate_hz as f32
                    });
            if relative_speed_mps == 0.0 {
                self.program_delays[source_index]
                    .observe_block_target_with_zero_motion(delay_target_samples);
            } else {
                self.program_delays[source_index].observe_block_target_with_motion(
                    delay_target_samples,
                    radial_velocity_mps,
                    relative_speed_mps,
                );
            }
            state.last_propagation_observation = Some(observation);
        }

        let distance_m =
            smoothed_source_distance_m(smoothed.source_position, smoothed.listener_position);
        let impulse_parameters: [_; 2] = std::array::from_fn(|channel| {
            state.impulse_shapers[channel]
                .as_ref()
                .map(|shaper| shaper.parameters_at_distance(distance_m))
        });
        let frames = self.audio.frame_size as usize;
        for frame in 0..frames {
            let mut shaped = [0.0; 2];
            for channel in 0..program_plane_count {
                let input = program_planes.map_or(0.0, |planes| planes[channel][frame]);
                shaped[channel] = match (
                    &mut state.impulse_shapers[channel],
                    impulse_parameters[channel],
                ) {
                    (Some(shaper), Some(parameters)) => shaper.process_sample(input, parameters),
                    _ => input,
                };
            }
            let delayed =
                self.program_delays[source_index].process_frame(shaped, program_plane_count)?;
            let extra = state.roof_head.next_extra_samples(propagation.over_roof, self.audio.sample_rate_hz);
            let roof_frame = if extra == 0.0 { delayed } else {
                self.program_delays[source_index].read_roof_frame(extra, state.roof_head.history_samples())
            };
            self.roof_program_work[0][frame] = roof_frame[0];
            self.roof_program_work[1][frame] = roof_frame[1];
            self.delayed_program[source_index][0][frame] = delayed[0];
            if program_plane_count == 2 {
                self.delayed_program[source_index][1][frame] = delayed[1];
            }
        }
        Ok((smoothed, teleported))
    }

    fn mark_feed(
        metadata: &mut fightbox_runtime::backend::SpatialOutputMetadata,
        source_index: usize,
        component: SpatialPresentationComponent,
        position: SteamVector3,
        listener_position: SteamVector3,
        metadata_city_offset: ApiEnuVector3,
        canonicalize_city_mm: bool,
        propagation: SteamSourcePropagation,
        latency_frames: u32,
    ) -> Result<usize, SpatialBackendRenderError> {
        let slot = component
            .presentation_slot()
            .ok_or(SpatialBackendRenderError::InvalidOutputMetadata)?;
        let plane = source_index * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE + slot;
        let local_position_enu = steam_vector_to_api(position);
        let local_listener_enu = steam_vector_to_api(listener_position);
        let city_coordinate = |local: f32, offset: f32| {
            if canonicalize_city_mm {
                let city_mm = ((f64::from(local) + f64::from(offset)) * 1_000.0).round();
                (city_mm / 1_000.0) as f32
            } else {
                local + offset
            }
        };
        let position_enu = ApiEnuVector3::new(
            city_coordinate(local_position_enu.east_m, metadata_city_offset.east_m),
            city_coordinate(local_position_enu.north_m, metadata_city_offset.north_m),
            city_coordinate(local_position_enu.up_m, metadata_city_offset.up_m),
        );
        // Translation is metadata-only. Resolve direction from the exact
        // cell-local difference so large city offsets cannot quantize or
        // rotate the acoustic placement vector.
        let direction_enu = normalized_api(ApiEnuVector3::new(
            local_position_enu.east_m - local_listener_enu.east_m,
            local_position_enu.north_m - local_listener_enu.north_m,
            local_position_enu.up_m - local_listener_enu.up_m,
        ))
        .unwrap_or(ApiEnuVector3::new(0.0, 1.0, 0.0));
        metadata.presentation_feeds[plane] = SpatialPresentationFeedMetadata {
            valid: true,
            source_index,
            component,
            placement: SpatialFeedPlacement::Direction,
            pose_enu: Pose {
                position: position_enu,
                forward: steam_vector_to_api(propagation.source_forward),
                up: steam_vector_to_api(propagation.source_up),
            },
            direction_enu,
            latency_frames,
        };
        Ok(plane)
    }

    fn apply_direct_effect(
        state: &mut NeutralSourceRenderState,
        direct_params: &mut ffi::IPLDirectEffectParams,
    ) {
        let mut input = match state.presentation {
            NeutralPresentationShape::LineSegment { .. } => state
                .line
                .as_ref()
                .expect("line presentation state is constructed")
                .presentation
                .raw(),
            NeutralPresentationShape::Point | NeutralPresentationShape::StereoImage { .. } => {
                state.program_input.raw()
            }
        };
        let mut output = state.direct_output.raw();
        ffi::direct_effect_apply(
            handle(state.direct_effect.0),
            direct_params,
            &mut input,
            &mut output,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn render_source(
        &mut self,
        source_index: usize,
        program_planes: Option<[&[f32]; 2]>,
        propagation: SteamSourcePropagation,
        listener_position: SteamVector3,
        listener_linear_velocity_mps: SteamVector3,
        direct_sequence: u64,
        governor_quality: GovernorRenderSnapshot,
        presentation_bank: &mut [f32],
        metadata: &mut fightbox_runtime::backend::SpatialOutputMetadata,
    ) -> Result<usize, SpatialBackendRenderError> {
        let listener_centric_reflection = governor_quality.reverb
            != ReverbStrategy::ListenerCentric
            || usize::from(governor_quality.listener_centric_source) == source_index;
        let state = &self.sources[source_index];
        let mut targets = source_quality_targets(
            governor_quality.sources[source_index],
            listener_centric_reflection,
        );
        if !self.world.has_baked_pathing
            || !state.presentation.admits_indirect()
            || !state.pathing_send_enabled
        {
            targets[1] = 0.0;
        }
        if !state.reflection_send_enabled || !state.presentation.admits_indirect() {
            targets[2] = 0.0;
        }
        let quality_ramps: [GainRamp; 3] = std::array::from_fn(|index| {
            GainRamp::new(
                state.quality_gains[index],
                targets[index],
                self.audio.frame_size as usize,
            )
        });
        self.sources[source_index].quality_gains = targets;

        let (smoothed, teleported) = self.prepare_delayed_program(
            source_index,
            program_planes,
            propagation,
            listener_position,
            listener_linear_velocity_mps,
            direct_sequence,
        )?;
        let frames = self.audio.frame_size as usize;
        let metadata_city_offset = self.metadata_city_offsets[source_index];
        let city_frame_metadata = self.metadata_city_frame_enabled[source_index];
        let metadata_source_position = if propagation.over_roof.active
            || smoothed.arrival_position != smoothed.source_position {
            smoothed.arrival_position
        } else if city_frame_metadata {
            propagation.source_position
        } else {
            smoothed.source_position
        };
        let metadata_listener_position = if city_frame_metadata {
            listener_position
        } else {
            smoothed.listener_position
        };
        let state = &mut self.sources[source_index];
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

        let active_feed_count = match state.presentation {
            NeutralPresentationShape::Point => {
                state
                    .program_input
                    .write_mono(&mut self.roof_program_work[0]);
                let plane = Self::mark_feed(
                    metadata,
                    source_index,
                    SpatialPresentationComponent::DirectCenter,
                    metadata_source_position,
                    metadata_listener_position,
                    metadata_city_offset,
                    city_frame_metadata,
                    propagation,
                    0,
                )?;
                if quality_ramps[0].is_audible() {
                    Self::apply_direct_effect(state, &mut direct_params);
                    state
                        .direct_output
                        .read_interleaved(&mut self.effect_interleaved_work[..frames]);
                    accumulate_interleaved_channel_ramped(
                        &self.effect_interleaved_work[..frames],
                        1,
                        0,
                        spatial_plane_mut(presentation_bank, plane, frames),
                        quality_ramps[0],
                    );
                }
                1
            }
            NeutralPresentationShape::LineSegment { length_m } => {
                let geometry = line_geometry(
                    smoothed.arrival_position,
                    propagation.source_forward,
                    smoothed.listener_position,
                    length_m,
                );
                let metadata_geometry = if city_frame_metadata {
                    line_geometry(
                        metadata_source_position,
                        propagation.source_forward,
                        metadata_listener_position,
                        length_m,
                    )
                } else {
                    geometry
                };
                let line = state
                    .line
                    .as_mut()
                    .expect("line presentation state is constructed");
                line.renderer.render_presentation(
                    &self.roof_program_work[0],
                    geometry.k,
                    teleported,
                    &mut self.line_work,
                );
                line.presentation.write_interleaved(&mut self.line_work);
                if quality_ramps[0].is_audible() {
                    Self::apply_direct_effect(state, &mut direct_params);
                    state
                        .direct_output
                        .read_interleaved(&mut self.effect_interleaved_work[..frames * 3]);
                }
                let feeds = [
                    (
                        SpatialPresentationComponent::DirectCenter,
                        metadata_geometry.center,
                        0,
                    ),
                    (
                        SpatialPresentationComponent::WidthPositive,
                        metadata_geometry.plus_endpoint,
                        1,
                    ),
                    (
                        SpatialPresentationComponent::WidthNegative,
                        metadata_geometry.minus_endpoint,
                        2,
                    ),
                ];
                for (component, position, channel) in feeds {
                    let plane = Self::mark_feed(
                        metadata,
                        source_index,
                        component,
                        position,
                        metadata_listener_position,
                        metadata_city_offset,
                        city_frame_metadata,
                        propagation,
                        DECLARED_LATENCY_SAMPLES,
                    )?;
                    if quality_ramps[0].is_audible() {
                        accumulate_interleaved_channel_ramped(
                            &self.effect_interleaved_work[..frames * 3],
                            3,
                            channel,
                            spatial_plane_mut(presentation_bank, plane, frames),
                            quality_ramps[0],
                        );
                    }
                }
                3
            }
            NeutralPresentationShape::StereoImage { width_m } => {
                for frame in 0..frames {
                    self.program_interleaved_work[frame * 2] =
                        self.roof_program_work[0][frame];
                    self.program_interleaved_work[frame * 2 + 1] =
                        self.roof_program_work[1][frame];
                }
                state
                    .program_input
                    .write_interleaved(&mut self.program_interleaved_work);
                if quality_ramps[0].is_audible() {
                    Self::apply_direct_effect(state, &mut direct_params);
                    state
                        .direct_output
                        .read_interleaved(&mut self.effect_interleaved_work[..frames * 2]);
                }

                let forward_enu = steam_vector_to_api(propagation.source_forward);
                let up_enu = steam_vector_to_api(propagation.source_up);
                let right_enu = normalized_api(cross_api(forward_enu, up_enu))
                    .ok_or(SpatialBackendRenderError::InvalidOutputMetadata)?;
                let geometry = line_geometry(
                    smoothed.arrival_position,
                    api_enu_to_steam(right_enu),
                    smoothed.listener_position,
                    width_m,
                );
                let metadata_geometry = if city_frame_metadata {
                    line_geometry(
                        metadata_source_position,
                        api_enu_to_steam(right_enu),
                        metadata_listener_position,
                        width_m,
                    )
                } else {
                    geometry
                };
                // Transparent Wave 0 structural mapping only: authored left
                // occupies the negative endpoint and authored right the
                // positive endpoint. No PCA, M/S matrix, crossfeed, collapse
                // law, or synthesized center exists in this graph.
                let feeds = [
                    (
                        SpatialPresentationComponent::WidthNegative,
                        metadata_geometry.minus_endpoint,
                        0,
                    ),
                    (
                        SpatialPresentationComponent::WidthPositive,
                        metadata_geometry.plus_endpoint,
                        1,
                    ),
                ];
                for (component, position, channel) in feeds {
                    let plane = Self::mark_feed(
                        metadata,
                        source_index,
                        component,
                        position,
                        metadata_listener_position,
                        metadata_city_offset,
                        city_frame_metadata,
                        propagation,
                        0,
                    )?;
                    if quality_ramps[0].is_audible() {
                        accumulate_interleaved_channel_ramped(
                            &self.effect_interleaved_work[..frames * 2],
                            2,
                            channel,
                            spatial_plane_mut(presentation_bank, plane, frames),
                            quality_ramps[0],
                        );
                    }
                }
                2
            }
        };

        if state.presentation.admits_indirect() {
            state
                .indirect_input
                .as_mut()
                .expect("indirect-admitting source owns mono send buffer")
                .write_mono(&mut self.delayed_program[source_index][0]);
            let mut indirect_input = state
                .indirect_input
                .as_mut()
                .expect("indirect-admitting source owns mono send buffer")
                .raw();
            if quality_ramps[1].is_audible() {
                let mut path_coefficients = smoothed.path_sh;
                let mut path_params = ffi::IPLPathEffectParams {
                    eqCoeffs: realizable_eq(smoothed.path_eq.map(f64::from)),
                    shCoeffs: path_coefficients.as_mut_ptr(),
                    order: self.path_order,
                    binaural: ffi::IPL_FALSE,
                    hrtf: core::ptr::null_mut(),
                    listener: ffi::IPLCoordinateSpace3::default(),
                    normalizeEQ: ffi::IPL_FALSE,
                };
                let path_effect = state
                    .path_effect
                    .as_ref()
                    .expect("indirect-admitting source owns path effect");
                let path_field = state
                    .path_field
                    .as_mut()
                    .expect("indirect-admitting source owns path field");
                let mut raw_path_field = path_field.raw();
                ffi::path_effect_apply(
                    handle(path_effect.0),
                    &mut path_params,
                    &mut indirect_input,
                    &mut raw_path_field,
                );
                path_field.read_interleaved(
                    &mut self.effect_interleaved_work[..frames * self.path_channels],
                );
                accumulate_interleaved_environment_ramped(
                    &self.effect_interleaved_work[..frames * self.path_channels],
                    self.path_channels,
                    &mut self.steam_environment_bank,
                    frames,
                    quality_ramps[1],
                    self.path_channels,
                );
            }

            let reflection = propagation.reflections;
            if !self.tail_retiring
                && quality_ramps[2].is_audible()
                && (reflection.ir != 0
                    || reflection_effect_uses_reverb(self.config.reflection_effect.effect_type))
            {
                for (frame, sample) in self.program_interleaved_work[..frames]
                    .iter_mut()
                    .enumerate()
                {
                    *sample =
                        self.delayed_program[source_index][0][frame] * quality_ramps[2].at(frame);
                }
                state
                    .indirect_input
                    .as_mut()
                    .expect("reflection source owns mono send buffer")
                    .write_mono(&mut self.program_interleaved_work[..frames]);
                indirect_input = state
                    .indirect_input
                    .as_mut()
                    .expect("reflection source owns mono send buffer")
                    .raw();
                let mut reflection_params = reflection_effect_params(reflection, self.config);
                reflection_params.irSize = reflection_params.irSize.min(state.reflection_ir_capacity);
                let reflection_effect = state
                    .reflection_effect
                    .as_ref()
                    .expect("enabled reflection send owns reflection effect");
                let mut scratch = state
                    .reflection_scratch
                    .as_mut()
                    .expect("enabled reflection send owns reflection scratch")
                    .raw();
                ffi::reflection_effect_apply_to_mixer(
                    handle(reflection_effect.0),
                    &mut reflection_params,
                    &mut indirect_input,
                    &mut scratch,
                    handle(
                        self.reflection_mixer
                            .as_ref()
                            .expect("reflection sources require shared mixer")
                            .0,
                    ),
                );
            }
        }

        Ok(active_feed_count)
    }

    /// Advances only reflection-effect histories that were admitted before the
    /// generation entered retirement. No source program or simulation state is
    /// consulted here.
    fn advance_retiring_reflection_sends(
        &mut self,
    ) -> Result<SpatialTailRetirementState, SpatialBackendRenderError> {
        if self.tail_retirement_state == SpatialTailRetirementState::TailComplete {
            return Ok(SpatialTailRetirementState::TailComplete);
        }
        let Some(mixer) = self.reflection_mixer.as_ref() else {
            self.tail_retirement_state = SpatialTailRetirementState::TailComplete;
            return Ok(self.tail_retirement_state);
        };

        let mut any_remaining = false;
        for state in &mut self.sources {
            let (Some(effect), Some(scratch)) = (
                state.reflection_effect.as_ref(),
                state.reflection_scratch.as_mut(),
            ) else {
                continue;
            };
            let mut raw_scratch = scratch.raw();
            any_remaining |= ffi::reflection_effect_get_tail_to_mixer(
                handle(effect.0),
                &mut raw_scratch,
                handle(mixer.0),
            );
        }
        self.tail_retirement_frames = self
            .tail_retirement_frames
            .saturating_add(self.audio.frame_size as u64);
        if !any_remaining || self.tail_retirement_frames >= self.tail_retirement_deadline_frames {
            self.tail_retirement_state = SpatialTailRetirementState::TailComplete;
        }
        Ok(self.tail_retirement_state)
    }

    fn render_reflection_mix(
        &mut self,
        governor_quality: GovernorRenderSnapshot,
    ) -> Result<(), SpatialBackendRenderError> {
        if self.reflection_mixer.is_none() || self.reflection_mix.is_none() {
            self.reflection_output_gain = governor_quality.reflection_output_gain;
            return Ok(());
        }
        let mut mixer_params = self.reflection_mixer_params(governor_quality)?;
        let reflection_mixer = self
            .reflection_mixer
            .as_ref()
            .expect("reflection presence checked above");
        let reflection_mix = self
            .reflection_mix
            .as_mut()
            .expect("reflection presence checked above");
        let mut raw_reflection_mix = reflection_mix.raw();
        ffi::reflection_mixer_apply(
            handle(reflection_mixer.0),
            &mut mixer_params,
            &mut raw_reflection_mix,
        );
        let frames = self.audio.frame_size as usize;
        reflection_mix.read_interleaved(
            &mut self.effect_interleaved_work[..frames * self.reflection_channels],
        );
        let reflection_gain_ramp = GainRamp::new(
            self.reflection_output_gain,
            governor_quality.reflection_output_gain,
            frames,
        );
        self.reflection_output_gain = governor_quality.reflection_output_gain;
        let active_reflection_channels = ambisonics_channel_count(governor_quality.ambisonic_order)
            .map_err(|_| SpatialBackendRenderError::InactiveGraph)?
            as usize;
        accumulate_interleaved_environment_ramped(
            &self.effect_interleaved_work[..frames * self.reflection_channels],
            self.reflection_channels,
            &mut self.steam_environment_bank,
            frames,
            reflection_gain_ramp,
            active_reflection_channels.min(self.environmental_channels),
        );
        Ok(())
    }

    fn reflection_mixer_params(
        &self,
        governor_quality: GovernorRenderSnapshot,
    ) -> Result<ffi::IPLReflectionEffectParams, SpatialBackendRenderError> {
        Ok(ffi::IPLReflectionEffectParams {
            type_: reflection_effect_ffi_type(self.config.reflection_effect.effect_type)
                .map_err(|_| SpatialBackendRenderError::InactiveGraph)?,
            ir: core::ptr::null_mut(),
            reverbTimes: [0.0; 3],
            eq: [1.0; 3],
            delay: 0,
            numChannels: ambisonics_channel_count(governor_quality.ambisonic_order)
                .map_err(|_| SpatialBackendRenderError::InactiveGraph)?,
            irSize: reflection_ir_size(
                governor_quality.reflections.ir_duration_s,
                self.audio.sample_rate_hz,
            )
            .map_err(|_| SpatialBackendRenderError::InactiveGraph)?,
            tanDevice: core::ptr::null_mut(),
            tanSlot: 0,
        })
    }

    fn write_environment_bank(
        &self,
        environmental_bank: &mut [f32],
    ) -> Result<(), SpatialBackendRenderError> {
        let frames = self.audio.frame_size as usize;
        // The audited Steam->neutral transform is the identity over the active
        // ACN prefix, so the per-frame gather/transform/scatter collapses into
        // plane-wise strided copies that touch each sample exactly once. Every
        // active sample is still validated finite before any byte is written,
        // preserving the original all-or-nothing error behavior.
        for channel in 0..self.environmental_channels {
            let start = channel * frames;
            if self.steam_environment_bank[start..start + frames]
                .iter()
                .any(|sample| !sample.is_finite())
            {
                return Err(SpatialBackendRenderError::InvalidOutputMetadata);
            }
        }
        for channel in 0..self.environmental_channels {
            let start = channel * frames;
            environmental_bank[start..start + frames]
                .copy_from_slice(&self.steam_environment_bank[start..start + frames]);
        }
        Ok(())
    }
}

impl SpatialBackendRenderGraph for NeutralMultiSourceRenderGraph {
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        NeutralMultiSourceRenderGraph::prepare_for_realtime(self)
    }

    fn pre_crossfade_warmup_blocks(&self) -> u8 {
        NEUTRAL_PRE_CROSSFADE_WARMUP_BLOCKS
    }

    fn begin_tail_retirement(&mut self) {
        self.tail_retiring = true;
        self.tail_retirement_frames = 0;
        self.tail_retirement_state = if self
            .sources
            .iter()
            .any(|source| source.reflection_effect.is_some())
        {
            SpatialTailRetirementState::TailRemaining
        } else {
            SpatialTailRetirementState::TailComplete
        };
    }

    fn tail_retirement_state(&self) -> SpatialTailRetirementState {
        self.tail_retirement_state
    }

    fn render_retiring_environmental_tail(
        &mut self,
        environmental_bank: &mut [f32],
    ) -> Result<SpatialTailRetirementState, SpatialBackendRenderError> {
        let expected_samples =
            MAX_SPATIAL_ENVIRONMENT_PLANES.saturating_mul(self.audio.frame_size as usize);
        if environmental_bank.len() != expected_samples {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }
        environmental_bank.fill(0.0);
        self.steam_environment_bank.fill(0.0);
        if self.tail_retirement_state == SpatialTailRetirementState::TailComplete {
            return Ok(self.tail_retirement_state);
        }

        let state = self.advance_retiring_reflection_sends()?;
        self.render_reflection_mix(self.applied_governor_quality)?;
        self.write_environment_bank(environmental_bank)?;
        Ok(state)
    }

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        if !self.prepared_for_realtime {
            return Err(SpatialBackendRenderError::InactiveGraph);
        }
        self.validate_block(&block)?;
        // During the bounded direct/path fade, both retained simulations are
        // advanced with one logical control token, so both graphs resolve the
        // same requested sequence. After the fade the swap wrapper calls only
        // `render_retiring_environmental_tail`, which consumes no new pose.
        let snapshot = self.snapshot_for_direct_sequence(block.propagation_sequence)?;
        let governor_quality = self.governor_quality.read();
        self.applied_governor_quality = governor_quality;

        block.presentation_bank.fill(0.0);
        block.environmental_bank.fill(0.0);
        self.steam_environment_bank.fill(0.0);
        let discontinuity_sequence = block.metadata.discontinuity_sequence;
        *block.metadata = fightbox_runtime::backend::SpatialOutputMetadata {
            sample_rate_hz: self.audio.sample_rate_hz as u32,
            block_size_frames: self.audio.frame_size as u32,
            block_start_frame: block.block_start_frame,
            validity: SpatialOutputValidity::Invalid,
            generation: self.world.generation,
            discontinuity_sequence,
            active_environmental_order: spatial_ambisonic_order(self.environmental_order)?,
            active_environmental_plane_count: self.environmental_channels,
            environmental_latency_frames: NEUTRAL_ENVIRONMENTAL_LATENCY_FRAMES,
            environmental_channel_order: SpatialAmbisonicChannelOrder::Acn,
            environmental_normalization: SpatialAmbisonicNormalization::N3d,
            environmental_basis: SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
            world_space_unrotated: true,
            source_drive_applied: true,
            source_safety_gain_applied: true,
            monitor_gain_applied: false,
            final_hrtf_applied: false,
            output_limiter_applied: false,
            ..fightbox_runtime::backend::SpatialOutputMetadata::default()
        };

        let mut supplied = [None; MAX_ACTIVE_SOURCES];
        for source in block.sources {
            supplied[source.source_index] = Some(*source);
        }
        let supplied_mask = supplied.map(|source| source.is_some());
        if !self.tail_retiring {
            self.reset_inactive_sources(&supplied_mask);
        }
        let mut active_feed_count = 0;
        for source_index in 0..self.world.source_count {
            let Some(source) = supplied[source_index] else {
                continue;
            };
            let propagation = snapshot.sources[source_index];
            self.sources[source_index].render_active = true;
            active_feed_count += self.render_source(
                source_index,
                Some(source.program_planes),
                propagation,
                snapshot.listener_position,
                snapshot.listener_linear_velocity_mps,
                snapshot.direct_sequence,
                governor_quality,
                block.presentation_bank,
                block.metadata,
            )?;
        }
        if self.tail_retiring {
            self.advance_retiring_reflection_sends()?;
        }
        self.render_reflection_mix(governor_quality)?;
        self.write_environment_bank(block.environmental_bank)?;
        block.metadata.active_presentation_feed_count = active_feed_count;
        block.metadata.validity = SpatialOutputValidity::Valid;
        Ok(())
    }
}

fn spatial_ambisonic_order(order: i32) -> Result<SpatialAmbisonicOrder, SpatialBackendRenderError> {
    match order {
        0 => Ok(SpatialAmbisonicOrder::Zero),
        1 => Ok(SpatialAmbisonicOrder::One),
        2 => Ok(SpatialAmbisonicOrder::Two),
        _ => Err(SpatialBackendRenderError::InvalidOutputMetadata),
    }
}

fn spatial_plane_mut(bank: &mut [f32], plane: usize, frames: usize) -> &mut [f32] {
    let start = plane * frames;
    &mut bank[start..start + frames]
}

fn accumulate_interleaved_channel_ramped(
    interleaved: &[f32],
    channel_count: usize,
    channel: usize,
    output: &mut [f32],
    ramp: GainRamp,
) {
    for (frame, output) in output.iter_mut().enumerate() {
        *output += interleaved[frame * channel_count + channel] * ramp.at(frame);
    }
}

fn accumulate_interleaved_environment_ramped(
    interleaved: &[f32],
    channel_count: usize,
    output_planar: &mut [f32],
    frames: usize,
    ramp: GainRamp,
    active_channels: usize,
) {
    let active_channels = active_channels.min(channel_count);
    for channel in 0..active_channels {
        let output = &mut output_planar[channel * frames..(channel + 1) * frames];
        for (frame, output) in output.iter_mut().enumerate() {
            *output += interleaved[frame * channel_count + channel] * ramp.at(frame);
        }
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
    let (simulation, reader, governor_quality) = build_simulation_generation(
        mesh,
        baked,
        audio,
        config,
        descriptors,
        generation,
        quality_tier,
        None,
    )?;
    let (stage_output_gain_writer, stage_output_gains) =
        SnapshotPublication::new(StageOutputGains::UNITY);
    let initial_echo_output_gain = if descriptors
        .iter()
        .any(|descriptor| descriptor.echo_profile.is_enabled())
    {
        1.0
    } else {
        0.0
    };
    let (echo_output_gain_writer, echo_output_gain) =
        SnapshotPublication::new(initial_echo_output_gain);
    let render = create_render_graph(
        Arc::clone(&simulation.world),
        audio,
        config,
        reader,
        stage_output_gain_writer,
        stage_output_gains,
        echo_output_gain_writer,
        echo_output_gain,
        governor_quality,
        descriptors,
    )?;
    Ok((simulation, render))
}

pub(crate) fn build_neutral_multi_source_generation(
    mesh: &SceneMesh,
    baked: Option<&BakedProbeBatch>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    program_channel_counts: &[usize],
    environmental_order: usize,
    generation: u64,
    quality_tier: QualityTier,
) -> Result<(MultiSourceSimulation, NeutralMultiSourceRenderGraph), BackendError> {
    validate_neutral_source_contract(
        config,
        descriptors,
        program_channel_counts,
        environmental_order,
    )?;
    let mut indirect_policy = NeutralSimulationIndirectPolicy {
        pathing: [false; MAX_ACTIVE_SOURCES],
        reflections: [false; MAX_ACTIVE_SOURCES],
    };
    for (index, (descriptor, program_plane_count)) in descriptors
        .iter()
        .copied()
        .zip(program_channel_counts.iter().copied())
        .enumerate()
    {
        let admits_indirect =
            NeutralPresentationShape::from_descriptor(descriptor, program_plane_count)?
                .admits_indirect();
        indirect_policy.pathing[index] = admits_indirect && descriptor.admits_pathing_send();
        indirect_policy.reflections[index] = admits_indirect && descriptor.admits_reflection_send();
    }
    let (mut simulation, reader, governor_quality) = build_simulation_generation(
        mesh,
        baked,
        audio,
        config,
        descriptors,
        generation,
        quality_tier,
        Some(indirect_policy),
    )?;
    let render = create_neutral_render_graph(
        Arc::clone(&simulation.world),
        audio,
        config,
        reader,
        governor_quality,
        descriptors,
        program_channel_counts,
        environmental_order as i32,
    )?;
    let neutral_memory = neutral_session_memory_telemetry(
        &simulation.world,
        audio,
        config,
        render.persistent_memory(),
    )?;
    simulation.governor.replace_session_memory(neutral_memory);
    Ok((simulation, render))
}

fn validate_neutral_source_contract(
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    program_channel_counts: &[usize],
    environmental_order: usize,
) -> Result<(), BackendError> {
    if config.reflection_effect.effect_type != ReflectionEffectType::Convolution {
        return Err(BackendError::InvalidInput(
            "neutral Wave 0 reflection mixing supports convolution only",
        ));
    }
    if descriptors.len() != program_channel_counts.len() {
        return Err(BackendError::InvalidInput(
            "neutral program channel counts must match the descriptor count",
        ));
    }
    if environmental_order > MAX_NEUTRAL_ENVIRONMENT_LAYOUT_ORDER_USIZE {
        return Err(BackendError::InvalidInput(
            "neutral environmental order must be between zero and two",
        ));
    }
    if !(0..=2).contains(&config.reflection_order) {
        return Err(BackendError::InvalidInput(
            "neutral reflection order must be between zero and two",
        ));
    }
    for (descriptor, program_plane_count) in descriptors
        .iter()
        .copied()
        .zip(program_channel_counts.iter().copied())
    {
        NeutralPresentationShape::from_descriptor(descriptor, program_plane_count)?;
    }
    Ok(())
}

const MAX_NEUTRAL_ENVIRONMENT_LAYOUT_ORDER_USIZE: usize =
    crate::neutral_environment::MAX_NEUTRAL_ENVIRONMENT_ORDER as usize;

/// Builds only the retained simulator used by anomaly proxy queries.
///
/// No HRTF, direct/path effect, reflection effect, mixer, decode effect, or
/// render scratch is constructed on this path.
pub(crate) fn build_anomaly_query_simulation(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptor: crate::MultiSourceDescriptor,
) -> Result<MultiSourceSimulation, BackendError> {
    let (simulation, _unused_snapshot_reader, _unused_governor_reader) =
        build_simulation_generation(
            mesh,
            Some(baked),
            audio,
            config,
            core::slice::from_ref(&descriptor),
            1,
            QualityTier::Desktop,
            None,
        )?;
    Ok(simulation)
}

fn build_simulation_generation(
    mesh: &SceneMesh,
    baked: Option<&BakedProbeBatch>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    generation: u64,
    quality_tier: QualityTier,
    neutral_indirect_policy: Option<NeutralSimulationIndirectPolicy>,
) -> Result<
    (
        MultiSourceSimulation,
        fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
        fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    ),
    BackendError,
> {
    validate_multi_source_config(mesh, baked, audio, config, descriptors, quality_tier)?;
    let reflection_budget_plan = build_reflection_budget_plan(config, descriptors, quality_tier)?;
    let mut memory = session_memory_telemetry(audio, config, descriptors, baked)?;
    let world = Arc::new(create_world(
        mesh,
        baked,
        audio,
        config,
        descriptors.len(),
        generation,
        quality_tier,
        neutral_indirect_policy,
    )?);
    let roof_bytes = world.roof_profile.payload_bytes();
    memory.retained_bake_bytes += roof_bytes;
    memory.tracked_at_create_bytes += roof_bytes;
    memory.tracked_current_bytes += roof_bytes;
    memory.tracked_peak_bytes += roof_bytes;
    let mut initial = SteamPropagationSnapshot::default();
    initial.world_generation = generation;
    let mut source_poses = [SteamPose {
        position: SteamVector3::default(),
        forward: SteamVector3::new(0.0, 0.0, -1.0),
        up: SteamVector3::new(0.0, 1.0, 0.0),
    }; MAX_ACTIVE_SOURCES];
    let mut source_directivities = [Directivity::OMNIDIRECTIONAL; MAX_ACTIVE_SOURCES];
    let mut source_occlusion_modes = [config.direct_occlusion; MAX_ACTIVE_SOURCES];
    let mut source_extents = [ExtentDescriptor::Point; MAX_ACTIVE_SOURCES];
    let mut source_echo_profiles = [EchoProfile::OFF; MAX_ACTIVE_SOURCES];
    let mut source_reflection_sends = [false; MAX_ACTIVE_SOURCES];
    let mut source_pathing_sends = [false; MAX_ACTIVE_SOURCES];
    let mut source_reflection_update_divisors = [1; MAX_ACTIVE_SOURCES];
    let mut source_reflection_share_radii = [0.0; MAX_ACTIVE_SOURCES];
    let mut source_reflection_capacities = [0; MAX_ACTIVE_SOURCES];
    let mut active = [false; MAX_ACTIVE_SOURCES];
    let listener = SteamPose::from_api(default_api_pose(ApiEnuVector3::default()))
        .expect("canonical listener pose is valid");
    for (index, descriptor) in descriptors.iter().enumerate() {
        source_poses[index] = SteamPose::from_api(descriptor.initial_pose()).ok_or(
            BackendError::InvalidInput("multi-source descriptor position must be finite"),
        )?;
        source_directivities[index] = descriptor.directivity;
        source_occlusion_modes[index] =
            crate::direct_occlusion_for_extent(config, descriptor.extent);
        source_extents[index] = descriptor.extent;
        source_echo_profiles[index] = descriptor.echo_profile;
        source_reflection_sends[index] = descriptor.admits_reflection_send();
        source_pathing_sends[index] = descriptor.admits_pathing_send();
        source_reflection_update_divisors[index] = descriptor.reflection_update_divisor();
        source_reflection_share_radii[index] = descriptor.reflection_share_radius_m().unwrap_or(0.0);
        source_reflection_capacities[index] = source_reflection_ir_size(*descriptor, config, audio)?;
        active[index] = descriptor.initially_active;
        initial.sources[index].active = descriptor.initially_active;
        initial.sources[index].source_position = source_poses[index].position;
        initial.sources[index].source_forward = source_poses[index].forward;
        initial.sources[index].source_up = source_poses[index].up;
        initial.sources[index].width =
            width_snapshot(descriptor.extent, source_poses[index], listener.position);
        initial.sources[index].configured_pathing_order = config.pathing_order as u8;
    }
    let (writer, reader) = SnapshotPublication::new(initial);
    let (mut governor, governor_quality) =
        QualityGovernor::new(audio, config, descriptors, quality_tier, memory);
    for (index, descriptor) in descriptors.iter().enumerate() {
        governor.set_source_priority(index, descriptor.priority_class);
    }
    let (scene_air_writer, scene_air) = SnapshotPublication::new(config.air_pressure_exponents_per_m);
    let simulation = MultiSourceSimulation {
        scene_air_writer: Some(scene_air_writer),
        scene_air,
        world,
        audio,
        config,
        source_directivities,
        source_occlusion_modes,
        source_extents,
        source_echo_profiles,
        source_reflection_sends,
        source_pathing_sends,
        baked_path_available: [None; MAX_ACTIVE_SOURCES],
        source_reflection_update_divisors,
        source_reflection_share_radii,
        source_reflection_capacities,
        reflection_budget_plan,
        reflection_forced_due: [true; MAX_ACTIVE_SOURCES],
        reflection_worker: None,
        reflection_worker_busy: false,
        reflection_min_interval_ns: 0,
        reflection_revisions: [0; MAX_ACTIVE_SOURCES],
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
        pass_cadences: [SimulationPassCadence::default(); 3],
        last_direct_frame: None,
        path_gates: [PathGateState::default(); MAX_ACTIVE_SOURCES],
        echo_plan_cache: [None; MAX_ACTIVE_SOURCES],
        roof_caches: [crate::over_roof::RoofCache::default(); MAX_ACTIVE_SOURCES],
        neutral_indirect_policy,
        work_counters: SimulationWorkCounters::default(),
        started: Instant::now(),
    };
    Ok((simulation, reader, governor_quality))
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
    if config.air_pressure_exponents_per_m.iter().any(|value| !value.is_finite() || *value < 0.0) {
        return Err(BackendError::InvalidInput("air exponents must be finite and non-negative"));
    }
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
        SteamPose::from_api(descriptor.initial_pose()).is_none()
            || !descriptor.declared_level_db().is_finite()
    }) {
        return Err(BackendError::InvalidInput(
            "multi-source descriptor poses and reference levels must be finite and non-degenerate",
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
    for descriptor in descriptors {
        if descriptor.reflection_share_radius_m().is_some_and(|r| !r.is_finite() || r <= 0.0)
            || (descriptor.reflection_share_radius_m().is_some() && descriptor.reflection_ir_limit_seconds().is_none()) {
            return Err(BackendError::InvalidInput("reflection sharing requires a positive finite radius and an IR cap"));
        }
        if descriptor.reflection_update_divisor() == 0 {
            return Err(BackendError::InvalidInput("source reflection update divisor must be positive"));
        }
        source_reflection_ir_size(*descriptor, config, audio)?;
    }
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

fn shared_reflection_targets(
    positions: &[SteamVector3; MAX_ACTIVE_SOURCES], active: &[bool; MAX_ACTIVE_SOURCES],
    quality: &[SourceQualityLevel; MAX_ACTIVE_SOURCES], radii: &[f32; MAX_ACTIVE_SOURCES],
    capacities: &[i32; MAX_ACTIVE_SOURCES], count: usize,
) -> [usize; MAX_ACTIVE_SOURCES] {
    let mut targets = std::array::from_fn(|i| i);
    let mut pair = [0; 2];
    let mut admitted = 0;
    for i in 0..count {
        if radii[i] > 0.0 {
            if admitted == 2 { return targets; }
            pair[admitted] = i; admitted += 1;
        }
    }
    if admitted != 2 { return targets; }
    let [a, b] = pair;
    let radius = radii[a].min(radii[b]);
    let delta = SteamVector3::new(positions[a].x - positions[b].x, positions[a].y - positions[b].y, positions[a].z - positions[b].z);
    if active[a] && active[b] && quality[a] == SourceQualityLevel::Full
        && quality[b] == SourceQualityLevel::Full && capacities[a] == capacities[b]
        && delta.x * delta.x + delta.y * delta.y + delta.z * delta.z <= radius * radius {
        targets[a] = b;
    }
    targets
}

fn source_reflection_ir_size(
    descriptor: crate::MultiSourceDescriptor,
    config: S3SimulationConfig,
    audio: AudioConfig,
) -> Result<i32, BackendError> {
    let duration = match descriptor.reflection_ir_limit_seconds() {
        Some(seconds) if seconds.is_finite() && seconds > 0.0 =>
            seconds.min(config.reflection_duration_s),
        Some(_) => return Err(BackendError::InvalidInput(
            "source reflection IR limit must be finite and positive",
        )),
        None => config.reflection_duration_s,
    };
    reflection_ir_size(duration, audio.sample_rate_hz)
}

fn build_reflection_budget_plan(
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    quality_tier: QualityTier,
) -> Result<Option<ReflectionBudgetPlan>, BackendError> {
    let explicit_count = descriptors
        .iter()
        .filter(|descriptor| descriptor.reflection_budget.is_some())
        .count();
    if explicit_count == 0 {
        if !descriptors.iter().any(|source| source.reflection_simulation_ir_limit_seconds.is_some()) {
            return Ok(None);
        }
        let mut plan = ReflectionBudgetPlan {
            groups: [ReflectionBudgetGroup::default(); MAX_REFLECTION_GROUPS],
            group_count: 0, source_groups: [NO_REFLECTION_GROUP; MAX_ACTIVE_SOURCES], quality_tier,
        };
        for (index, source) in descriptors.iter().copied().enumerate() {
            if !source.reflection_send_enabled { continue; }
            let duration = match source.reflection_simulation_ir_limit_seconds {
                Some(seconds) if seconds.is_finite() && seconds > 0.0 => seconds.min(config.reflection_duration_s),
                Some(_) => return Err(BackendError::InvalidInput("source reflection IR limit must be finite and positive")),
                None => config.reflection_duration_s,
            };
            let (rays, bounces) = (config.reflection_rays, config.reflection_bounces);
            let budget = SourceReflectionBudget::realtime(rays.min(config.reflection_rays),
                bounces.min(config.reflection_bounces), duration, config.reflection_order, 1);
            let group = plan.groups[..usize::from(plan.group_count)].iter()
                .position(|group| group.requested == budget);
            let group_index = match group {
                Some(index) => index,
                None => {
                    let index = usize::from(plan.group_count);
                    if index == MAX_REFLECTION_GROUPS {
                        return Err(BackendError::InvalidInput("source reflection limits support at most two budget shapes"));
                    }
                    plan.groups[index] = ReflectionBudgetGroup {
                        requested: budget, inherit_shared_quality: true, tick: 0,
                    };
                    plan.group_count += 1;
                    index
                }
            };
            plan.source_groups[index] = group_index as u8;
        }
        return Ok(Some(plan));
    }
    if explicit_count != descriptors.len() {
        return Err(BackendError::InvalidInput(
            "reflection budgets must be explicit for every source or absent for every source",
        ));
    }

    let mut plan = ReflectionBudgetPlan {
        groups: [ReflectionBudgetGroup::default(); MAX_REFLECTION_GROUPS],
        group_count: 0,
        source_groups: [NO_REFLECTION_GROUP; MAX_ACTIVE_SOURCES],
        quality_tier,
    };
    let mut featured_sources = 0_usize;
    let mut realtime_sources = 0_usize;

    for (source_index, descriptor) in descriptors.iter().copied().enumerate() {
        let budget = descriptor
            .reflection_budget
            .expect("all reflection budgets were checked explicit");
        match budget.delivery {
            ReflectionDelivery::Off => {
                if budget != SourceReflectionBudget::OFF {
                    return Err(BackendError::InvalidInput(
                        "an Off reflection budget must use the canonical zero shape",
                    ));
                }
                continue;
            }
            ReflectionDelivery::Realtime => {
                if budget.rays <= 0
                    || budget.bounces < 0
                    || !budget.duration_s.is_finite()
                    || budget.duration_s <= 0.0
                    || !(0..=3).contains(&budget.order)
                    || budget.cadence_divisor == 0
                {
                    return Err(BackendError::InvalidInput(
                        "realtime reflection budgets require positive rays, duration, and cadence, nonnegative bounces, and order zero through three",
                    ));
                }
                if !descriptor.reflection_send_enabled {
                    return Err(BackendError::InvalidInput(
                        "a realtime reflection budget cannot be combined with a disabled reflection send; use SourceReflectionBudget::OFF",
                    ));
                }
                if budget.rays > config.reflection_rays
                    || budget.bounces > config.reflection_bounces
                    || budget.duration_s > config.reflection_duration_s
                    || budget.order > config.reflection_order
                {
                    return Err(BackendError::InvalidInput(
                        "a source reflection budget exceeds the session reflection capacity",
                    ));
                }
            }
        }

        realtime_sources += 1;
        if budget.is_featured() {
            featured_sources += 1;
        }
        let existing_group = plan.groups[..usize::from(plan.group_count)]
            .iter()
            .position(|group| group.requested == budget);
        let group_index = if let Some(group_index) = existing_group {
            group_index
        } else {
            let group_index = usize::from(plan.group_count);
            if group_index == MAX_REFLECTION_GROUPS {
                return Err(BackendError::InvalidInput(
                    "an explicit reflection plan supports at most two distinct realtime budget shapes",
                ));
            }
            plan.groups[group_index].requested = budget;
            plan.group_count += 1;
            group_index
        };
        plan.source_groups[source_index] = group_index as u8;
    }

    if featured_sources > 1 {
        return Err(BackendError::InvalidInput(
            "an explicit reflection plan supports at most one featured source",
        ));
    }
    if quality_tier == QualityTier::Mobile && (realtime_sources > 1 || featured_sources > 0) {
        return Err(BackendError::InvalidInput(
            "mobile v1 supports one Standard-or-smaller realtime reflection source",
        ));
    }
    Ok(Some(plan))
}

fn session_memory_telemetry(
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    baked: Option<&BakedProbeBatch>,
) -> Result<SessionMemoryTelemetry, BackendError> {
    let frames = u64::try_from(audio.frame_size)
        .map_err(|_| BackendError::InvalidInput("audio frame size must be positive"))?;
    let sources = descriptors.len() as u64;
    let wide_sources = descriptors
        .iter()
        .filter(|descriptor| matches!(descriptor.extent, ExtentDescriptor::LineSegment { .. }))
        .count() as u64;
    let stereo_sources = descriptors
        .iter()
        .filter(|descriptor| matches!(descriptor.extent, ExtentDescriptor::StereoImage { .. }))
        .count() as u64;
    let echo_sources = descriptors
        .iter()
        .filter(|descriptor| descriptor.echo_profile.is_enabled())
        .count() as u64;
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
    let snapshot_ring_payload_bytes = 4_u64
        .saturating_mul(
            size_of::<SteamPropagationSnapshot>()
                .saturating_add(size_of::<GovernorRenderSnapshot>())
                .saturating_add(size_of::<StageOutputGains>())
                .saturating_add(size_of::<f32>()) as u64,
        )
        .saturating_add(if echo_sources > 0 {
            // Host echo plans and primary routes per source slot: three
            // shared slots and the reader's retained copy.
            4 * (size_of::<[ExternalEchoPlan; MAX_ACTIVE_SOURCES]>()
                + size_of::<[Option<crate::PrimaryRoute>; MAX_ACTIVE_SOURCES]>())
                as u64
        } else {
            0
        });
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
    // Ambisonic reflection scratch. A line adds three-channel presentation and
    // direct buffers; stereo images add two-channel input and direct buffers.
    // Shared: Ambisonic reflection mix + stereo decode target.
    let audio_buffer_payload_bytes = sources
        .saturating_mul(6_u64.saturating_add(channels))
        .saturating_add(wide_sources.saturating_mul(6))
        .saturating_add(stereo_sources.saturating_mul(4))
        .saturating_add(echo_sources.saturating_mul(4))
        .saturating_add(channels.saturating_add(2))
        .saturating_mul(frames)
        .saturating_mul(float_bytes);
    let width_scratch_channels = if wide_sources > 0 {
        4
    } else if stereo_sources > 0 {
        3
    } else {
        0
    };
    // Mono, stereo, route and roof-head work, then width and echo-tap scratch.
    let render_scratch_bytes = (5_u64
        + width_scratch_channels
        + u64::from(stereo_sources > 0)
        + echo_sources.saturating_mul(MAX_ECHO_TAPS_PER_SOURCE as u64))
    .saturating_mul(frames)
    .saturating_mul(float_bytes)
    .saturating_add(
        sources.saturating_add(echo_sources.saturating_mul(MAX_ECHO_TAPS_PER_SOURCE as u64))
            .saturating_mul(size_of::<SteadySilentPair>() as u64),
    )
    .saturating_add(wide_sources.saturating_mul(3).saturating_mul(size_of::<SteadyBinaural>() as u64));
    let render_scratch_bytes = render_scratch_bytes
        .saturating_add(sources.saturating_mul(size_of::<SteadyPath>() as u64))
        .saturating_add(if descriptors.iter().filter(|d| d.reflection_share_radius_m().is_some()).count() == 2 { sources * frames * float_bytes } else { 0 });
    let maximum_delay_samples = maximum_propagation_delay_samples(audio.sample_rate_hz);
    let propagation_ring_samples = delay_history_len(maximum_delay_samples) as u64;
    let echo_ring_samples = maximum_delay_samples.saturating_add(4) as u64;
    // Every propagation line owns audio plus same-time geometry history. Echo
    // sidecars own only their independent dry-audio history ring.
    let propagation_delay_line_bytes = sources
        .saturating_mul(2)
        .saturating_add(stereo_sources.saturating_mul(3))
        .saturating_mul(propagation_ring_samples)
        .saturating_add(echo_sources.saturating_mul(echo_ring_samples))
        .saturating_mul(float_bytes)
        .saturating_add(bandlimited_kernel_payload_bytes());
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

fn neutral_session_memory_telemetry(
    world: &WorldGeneration,
    audio: AudioConfig,
    config: S3SimulationConfig,
    render: crate::SpatialRenderMemoryTelemetry,
) -> Result<SessionMemoryTelemetry, BackendError> {
    // SnapshotPublication owns three shared slots and each reader retains one
    // last-complete value. The neutral propagation graph additionally retains
    // current and immediately previous exact-direct-token snapshots so an
    // ordinary one-publication skew does not become audible silence.
    let snapshot_ring_payload_bytes =
        SnapshotPublication::shared_payload_bytes::<SteamPropagationSnapshot>()
            .saturating_add(SnapshotPublication::shared_payload_bytes::<[f32; 3]>())
            .saturating_add(size_of::<[f32; 3]>() as u64)
            .saturating_add(size_of::<SteamPropagationSnapshot>() as u64)
            .saturating_add(size_of::<SteamPropagationSnapshot>() as u64)
            .saturating_add(size_of::<Option<SteamPropagationSnapshot>>() as u64)
            .saturating_add(SnapshotPublication::shared_payload_bytes::<
                GovernorRenderSnapshot,
            >())
            .saturating_add(size_of::<GovernorRenderSnapshot>() as u64);
    let reflection_source_count = world.source_simulation_flags[..world.source_count]
        .iter()
        .filter(|flags| **flags & ffi::IPL_SIMULATIONFLAGS_REFLECTIONS != 0)
        .count() as u64;
    let reflection_ir_payload_capacity_bytes =
        if reflection_effect_uses_ir(config.reflection_effect.effect_type) {
            let channels = u64::try_from(ambisonics_channel_count(config.reflection_order)?)
                .map_err(|_| BackendError::InvalidInput("Ambisonic channel count is invalid"))?;
            let samples = u64::try_from(reflection_ir_size(
                config.reflection_duration_s,
                audio.sample_rate_hz,
            )?)
            .map_err(|_| BackendError::InvalidInput("reflection IR size is invalid"))?;
            reflection_source_count
                .saturating_mul(channels)
                .saturating_mul(samples)
                .saturating_mul(size_of::<f32>() as u64)
        } else {
            0
        };
    // The retained category covers every Rust-owned buffer kept alive by the
    // generation: the caller-owned serialized bake (capacity convention) plus
    // the probe influence index derived from it (cell offsets, sphere
    // indices, decoded spheres). Folding the grid here keeps the governor's
    // category-sum reconciliation intact without widening the public shape.
    let retained_bake_bytes = (world.serialized_bytes.capacity() as u64)
        .saturating_add(world.probe_grid_payload_capacity_bytes())
        .saturating_add(world.roof_profile.payload_bytes());
    let propagation_delay_line_bytes = render
        .program_delay_audio_history_payload_bytes
        .saturating_add(render.program_delay_geometry_history_payload_bytes)
        .saturating_add(bandlimited_kernel_payload_bytes());
    let render_scratch_bytes = render
        .rust_scratch_payload_bytes
        .saturating_add(render.outer_vec_payload_bytes);
    let tracked = snapshot_ring_payload_bytes
        .saturating_add(reflection_ir_payload_capacity_bytes)
        .saturating_add(render.steam_audio_buffer_payload_bytes)
        .saturating_add(render_scratch_bytes)
        .saturating_add(propagation_delay_line_bytes)
        .saturating_add(retained_bake_bytes);

    Ok(SessionMemoryTelemetry {
        tracked_at_create_bytes: tracked,
        tracked_current_bytes: tracked,
        tracked_peak_bytes: tracked,
        snapshot_ring_payload_bytes,
        reflection_ir_payload_capacity_bytes,
        audio_buffer_payload_bytes: render.steam_audio_buffer_payload_bytes,
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
    neutral_indirect_policy: Option<NeutralSimulationIndirectPolicy>,
) -> Result<WorldGeneration, BackendError> {
    let probe_influences = baked
        .map(|baked| {
            SerializedProbeInfluences::parse(&baked.bytes, baked.metadata.probe_count)
                .map_err(BackendError::InvalidProbeBatch)
        })
        .transpose()?;
    let serialized_bytes = baked.map_or_else(Vec::new, |baked| baked.bytes.clone());
    let probe_grid = probe_influences
        .as_ref()
        .and_then(|influences| ProbeInfluenceGrid::build(*influences, &serialized_bytes));
    let probe_influences = probe_influences;
    let mut context = core::ptr::null_mut();
    let mut context_settings = ffi::IPLContextSettings::pinned_defaults();
    sdk_status(
        "iplContextCreate",
        ffi::context_create(&mut context_settings, &mut context),
    )?;
    let mut world = WorldGeneration {
        generation,
        has_baked_pathing: baked.is_some(),
        roof_profile: crate::over_roof::RoofProfile::from_mesh(mesh),
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
        source_simulation_flags: [0; MAX_ACTIVE_SOURCES],
        source_count: 0,
        reflection_worker_enabled: AtomicBool::new(false),
        reflection_acknowledged: std::array::from_fn(|_| AtomicU64::new(0)),
        reflection_worker_hold_source: AtomicUsize::new(0),
        reflection_worker_hold_ir: AtomicUsize::new(0),
        probe_influences,
        probe_grid,
        serialized_bytes,
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
            let flags = neutral_indirect_policy.map_or_else(all_simulation_flags, |policy| {
                ffi::IPL_SIMULATIONFLAGS_DIRECT
                    | if policy.pathing[index] {
                        ffi::IPL_SIMULATIONFLAGS_PATHING
                    } else {
                        0
                    }
                    | if policy.reflections[index] {
                        ffi::IPL_SIMULATIONFLAGS_REFLECTIONS
                    } else {
                        0
                    }
            });
            let mut settings = ffi::IPLSourceSettings { flags };
            let mut source = core::ptr::null_mut();
            sdk_status(
                "iplSourceCreate",
                ffi::source_create(simulator, &mut settings, &mut source),
            )?;
            ffi::source_add(source, simulator);
            world.sources[index] = source as usize;
            world.source_simulation_flags[index] = flags;
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
    echo_output_gain_writer: fightbox_runtime::SnapshotWriter<f32>,
    echo_output_gain: fightbox_runtime::SnapshotReader<f32>,
    mut governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    descriptors: &[crate::MultiSourceDescriptor],
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
        let initial_delay_samples = uncapped_propagation_delay_samples(
            initial_snapshot.sources[index].source_position,
            initial_snapshot.listener_position,
            audio.sample_rate_hz,
        );
        let mut state = create_source_render_state(
            context,
            &mut audio_settings,
            hrtf,
            config,
            source_reflection_ir_size(descriptors[index], config, audio)?,
            channels,
            maximum_delay_samples,
            initial_delay_samples,
            descriptors[index].extent,
            descriptors[index].impulse_class,
            descriptors[index].admits_pathing_send(),
            descriptors[index].admits_reflection_send(),
            descriptors[index].echo_profile,
        )?;
        if !initial_snapshot.sources[index].active {
            // A pre-declared inactive event can trigger before the callback has
            // ever rendered an inactive block. Do not retain the constructor's
            // default-listener delay in that case: first activation must adopt
            // its teleported trigger-time distance whole, exactly as a later
            // inactive render would arrange through `invalidate`.
            state.propagation_delay.invalidate();
            if let Some(stereo) = &mut state.stereo_image {
                stereo.delay.invalidate();
                stereo.roof_head.invalidate();
            }
        }
        source_states.push(state);
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
        if !world.has_baked_pathing || !state.pathing_send_enabled {
            state.quality_gains[1] = 0.0;
        }
        if !state.reflection_send_enabled {
            state.quality_gains[2] = 0.0;
        }
    }
    let has_line_width = descriptors
        .iter()
        .any(|descriptor| matches!(descriptor.extent, ExtentDescriptor::LineSegment { .. }));
    let has_stereo_image = descriptors
        .iter()
        .any(|descriptor| matches!(descriptor.extent, ExtentDescriptor::StereoImage { .. }));
    let echo_profiles = std::array::from_fn(|index| {
        descriptors
            .get(index)
            .map_or(EchoProfile::OFF, |descriptor| descriptor.echo_profile)
    });
    let has_echo_sources = descriptors
        .iter()
        .any(|descriptor| descriptor.echo_profile.is_enabled());
    let (echo_trigger, echo_trigger_control) = if has_echo_sources {
        let generations = Arc::new(EchoTriggerGenerations::new());
        let (writers, external_plans) = (0..MAX_ACTIVE_SOURCES)
            .map(|_| SnapshotPublication::new(ExternalEchoPlan::default()))
            .unzip();
        let (route_writers, routes) = (0..MAX_ACTIVE_SOURCES)
            .map(|_| SnapshotPublication::new(None))
            .unzip();
        (
            Some(Box::new(EchoTriggerRender {
                generations: Arc::clone(&generations),
                external_plans,
                routes,
                observed: [0; MAX_ACTIVE_SOURCES],
            })),
            Some((
                EchoTrigger::new(generations),
                EchoPlanWriter {
                    air_exponents: config.air_pressure_exponents_per_m,
                    world: Arc::clone(&world),
                    sample_rate_hz: audio.sample_rate_hz,
                    profiles: echo_profiles.to_vec(),
                    writers,
                    route_writers,
                    sequence: 0,
                },
            )),
        )
    } else {
        (None, None)
    };
    let (live_energy_writer, live_energy_reader) =
        SnapshotPublication::new(crate::LiveStageEnergySnapshot::default());
    // An immutable OFF send (the combat crack slot) has a valid, never-written
    // source mailbox. Reuse it to defer swaps without suppressing input/tails.
    let reflection_ir_hold = if descriptors.iter().filter(|source| source.admits_reflection_send()).count() > 1
        && config.reflection_effect.effect_type == ReflectionEffectType::Convolution
    {
        descriptors.iter().enumerate().find(|(index, source)| !source.admits_reflection_send()
            && world.source_simulation_flags[*index] & ffi::IPL_SIMULATIONFLAGS_REFLECTIONS != 0)
            .map_or(0, |(index, _)| {
                let mut outputs = ffi::IPLSimulationOutputs::zeroed();
                ffi::source_get_outputs(world.source(index), ffi::IPL_SIMULATIONFLAGS_REFLECTIONS, &mut outputs);
                outputs.reflections.ir as usize
            })
    } else { 0 };
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
        reflection_share_radii: std::array::from_fn(|i| descriptors.get(i).and_then(|d| d.reflection_share_radius_m()).unwrap_or(0.0)),
        reflection_share_capacities: std::array::from_fn(|i| descriptors.get(i).map(|d| source_reflection_ir_size(*d, config, audio).expect("validated IR cap")).unwrap_or(0)),
        reflection_share_targets: std::array::from_fn(|i| i),
        reflection_previous_share_targets: std::array::from_fn(|i| i),
        reflection_share_gains: [0.0; MAX_ACTIVE_SOURCES],
        reflection_shared_work: vec![0.0; if descriptors.iter().filter(|d| d.reflection_share_radius_m().is_some()).count() == 2 { audio.frame_size as usize * descriptors.len() } else { 0 }],
        mono_work: vec![0.0; audio.frame_size as usize],
        program_mono_work: has_stereo_image.then(|| vec![0.0; audio.frame_size as usize]),
        route_work: vec![0.0; audio.frame_size as usize],
        roof_work: vec![0.0; audio.frame_size as usize],
        stereo_work: vec![0.0; audio.frame_size as usize * 2],
        live_direct_path_left: vec![0.0; audio.frame_size as usize],
        live_direct_path_right: vec![0.0; audio.frame_size as usize],
        width_work: vec![
            0.0;
            if has_line_width {
                audio.frame_size as usize * 3
            } else if has_stereo_image {
                audio.frame_size as usize * 2
            } else {
                0
            }
        ],
        width_feed_work: vec![
            0.0;
            if has_line_width || has_stereo_image {
                audio.frame_size as usize
            } else {
                0
            }
        ],
        spatial_export: None,
        publication,
        stage_output_gain_writer: Some(stage_output_gain_writer),
        stage_output_gains,
        echo_output_gain_writer: Some(echo_output_gain_writer),
        echo_output_gain,
        governor_quality,
        applied_governor_quality,
        echo_profiles,
        has_echo_sources,
        echo_trigger,
        echo_trigger_control,
        scene_reset_sequence: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        observed_scene_reset: 0,
        reflection_output_gain: applied_governor_quality.reflection_output_gain,
        reflection_block_order: applied_governor_quality.ambisonic_order,
        retire_silent_reflections: descriptors.iter().filter(|source| source.admits_reflection_send()).count() > 1
            && config.reflection_effect.effect_type == ReflectionEffectType::Convolution,
        idle_reflection_source: 0,
        reflection_ir_hold,
        reflection_adoption: ReflectionAdoption::new(),
        reflection_adoption_source: None,
        tail_retiring: false,
        reflection_tail_remaining: false,
        reflection_decode_tail_remaining: false,
        echo_tail_remaining: false,
        tail_retirement_frames: 0,
        reflection_tail_deadline_frames: 0,
        echo_tail_deadline_frames: 0,
        retirement_stage_output_gains: StageOutputGains::UNITY,
        retirement_echo_output_gain: 0.0,
        retirement_listener_position: SteamVector3::new(0.0, 0.0, 0.0),
        propagation_block_retention: (-(audio.frame_size as f32 / audio.sample_rate_hz as f32)
            / PROPAGATION_SLEW_TIME_SECONDS)
            .exp(),
        live_energy_writer,
        live_energy_reader: Some(live_energy_reader),
        live_energy_sequence: 0,
        #[cfg(test)]
        governor_snapshot_reads: 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn create_neutral_render_graph(
    world: Arc<WorldGeneration>,
    audio: AudioConfig,
    config: S3SimulationConfig,
    mut publication: fightbox_runtime::SnapshotReader<SteamPropagationSnapshot>,
    mut governor_quality: fightbox_runtime::SnapshotReader<GovernorRenderSnapshot>,
    descriptors: &[crate::MultiSourceDescriptor],
    program_channel_counts: &[usize],
    environmental_order: i32,
) -> Result<NeutralMultiSourceRenderGraph, BackendError> {
    let context = world.context();
    let mut audio_settings = raw_audio_settings(audio);
    let environmental_channels = active_channel_count(environmental_order).map_err(|_| {
        BackendError::InvalidInput("neutral environmental order must be 0, 1, or 2")
    })?;
    let path_order = config.pathing_order.min(environmental_order);
    let path_channels = path_coefficient_count(path_order).ok_or(BackendError::InvalidInput(
        "neutral pathing order must be between zero and two",
    ))?;
    let reflection_channels = usize::try_from(ambisonics_channel_count(config.reflection_order)?)
        .map_err(|_| {
        BackendError::InvalidInput("neutral reflection channel count is invalid")
    })?;
    let ir_size = reflection_ir_size(config.reflection_duration_s, audio.sample_rate_hz)?;
    let has_reflection_send = descriptors.iter().any(|descriptor| {
        descriptor.admits_reflection_send()
            && !matches!(descriptor.extent, ExtentDescriptor::StereoImage { .. })
    });
    let reflection_mixer = if has_reflection_send {
        let mut reflection_settings = ffi::IPLReflectionEffectSettings {
            type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
            irSize: ir_size,
            numChannels: reflection_channels as i32,
        };
        let mut reflection_mixer = core::ptr::null_mut();
        sdk_status(
            "iplReflectionMixerCreate(neutral)",
            ffi::reflection_mixer_create(
                context,
                &mut audio_settings,
                &mut reflection_settings,
                &mut reflection_mixer,
            ),
        )?;
        Some(NeutralReflectionMixer(reflection_mixer as usize))
    } else {
        None
    };

    let initial_snapshot = publication.read();
    let maximum_delay_samples = maximum_propagation_delay_samples(audio.sample_rate_hz);
    let mut program_delays = Vec::with_capacity(world.source_count);
    let mut fixed_program_plane_counts = [0; MAX_ACTIVE_SOURCES];
    let mut source_states = Vec::with_capacity(MAX_ACTIVE_SOURCES);
    for index in 0..world.source_count {
        let presentation = NeutralPresentationShape::from_descriptor(
            descriptors[index],
            program_channel_counts[index],
        )?;
        fixed_program_plane_counts[index] = program_channel_counts[index];
        let initial_delay_samples = uncapped_propagation_delay_samples(
            initial_snapshot.sources[index].source_position,
            initial_snapshot.listener_position,
            audio.sample_rate_hz,
        );
        let mut program_delay = NeutralProgramDelay::new(
            program_channel_counts[index],
            maximum_delay_samples,
            audio.sample_rate_hz,
        );
        program_delay.reset_to(initial_delay_samples);
        if !initial_snapshot.sources[index].active {
            program_delay.invalidate();
        }
        program_delays.push(program_delay);
        let mut source_state = create_neutral_source_render_state(
            context,
            &mut audio_settings,
            config,
            source_reflection_ir_size(descriptors[index], config, audio)?,
            reflection_channels as i32,
            path_order,
            path_channels as i32,
            presentation,
            descriptors[index].impulse_class,
            descriptors[index].admits_pathing_send(),
            descriptors[index].admits_reflection_send(),
        )?;
        source_state.render_active = initial_snapshot.sources[index].active;
        source_states.push(source_state);
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
        if !world.has_baked_pathing
            || !state.presentation.admits_indirect()
            || !state.pathing_send_enabled
        {
            state.quality_gains[1] = 0.0;
        }
        if !state.reflection_send_enabled || !state.presentation.admits_indirect() {
            state.quality_gains[2] = 0.0;
        }
    }

    let frames = audio.frame_size as usize;
    let delayed_program = program_channel_counts
        .iter()
        .copied()
        .map(|channel_count| {
            [
                vec![0.0; frames],
                if channel_count == 2 {
                    vec![0.0; frames]
                } else {
                    Vec::new()
                },
            ]
        })
        .collect::<Vec<_>>();
    let reflection_mix = if has_reflection_send {
        Some(OwnedAudioBuffer::allocate(
            context,
            reflection_channels as i32,
            audio.frame_size,
        )?)
    } else {
        None
    };
    let program_interleaved_work = vec![0.0; frames * 2];
    let roof_program_work = [vec![0.0; frames], vec![0.0; frames]];
    let effect_interleaved_work = vec![0.0; frames * MAX_NEUTRAL_ENVIRONMENT_CHANNELS];
    let line_work = vec![0.0; frames * 3];
    let steam_environment_bank = vec![0.0; frames * MAX_NEUTRAL_ENVIRONMENT_CHANNELS];
    let metadata_city_offsets = descriptors
        .iter()
        .copied()
        .map(crate::MultiSourceDescriptor::metadata_city_offset)
        .collect::<Vec<_>>();
    let metadata_city_frame_enabled = descriptors
        .iter()
        .copied()
        .map(crate::MultiSourceDescriptor::metadata_city_frame_enabled)
        .collect::<Vec<_>>();
    let memory = neutral_render_memory(
        &source_states,
        &metadata_city_offsets,
        &metadata_city_frame_enabled,
        &program_delays,
        &delayed_program,
        &reflection_mix,
        &program_interleaved_work,
        &roof_program_work,
        &effect_interleaved_work,
        &line_work,
        &steam_environment_bank,
    );

    Ok(NeutralMultiSourceRenderGraph {
        world,
        config,
        audio,
        environmental_order,
        environmental_channels,
        path_order,
        path_channels,
        reflection_channels,
        sources: source_states,
        metadata_city_offsets,
        metadata_city_frame_enabled,
        program_plane_counts: fixed_program_plane_counts,
        program_delays,
        delayed_program,
        roof_program_work,
        reflection_mixer,
        reflection_mix,
        program_interleaved_work,
        effect_interleaved_work,
        line_work,
        steam_environment_bank,
        publication,
        correlation_current: initial_snapshot,
        correlation_previous: None,
        correlation_history_hits: 0,
        correlation_misses: 0,
        governor_quality,
        applied_governor_quality,
        reflection_output_gain: applied_governor_quality.reflection_output_gain,
        tail_retiring: false,
        tail_retirement_state: SpatialTailRetirementState::TailComplete,
        tail_retirement_frames: 0,
        tail_retirement_deadline_frames: (f64::from(config.reflection_duration_s)
            * f64::from(audio.sample_rate_hz))
        .ceil()
        .max(audio.frame_size as f64) as u64,
        propagation_block_retention: (-(audio.frame_size as f32 / audio.sample_rate_hz as f32)
            / PROPAGATION_SLEW_TIME_SECONDS)
            .exp(),
        prepared_for_realtime: false,
        #[cfg(test)]
        prepared_reflection_effect_count: 0,
        memory,
    })
}

#[allow(clippy::too_many_arguments)]
fn create_neutral_source_render_state(
    context: ffi::IPLContext,
    audio_settings: &mut ffi::IPLAudioSettings,
    config: S3SimulationConfig,
    ir_size: i32,
    reflection_channels: i32,
    path_order: i32,
    path_channels: i32,
    presentation: NeutralPresentationShape,
    impulse_class: fightbox_api::ImpulseClass,
    pathing_send_enabled: bool,
    reflection_send_enabled: bool,
) -> Result<NeutralSourceRenderState, BackendError> {
    let direct_channels = presentation.direct_channel_count();
    let mut direct_settings = ffi::IPLDirectEffectSettings {
        numChannels: direct_channels,
    };
    let mut direct_effect = core::ptr::null_mut();
    sdk_status(
        "iplDirectEffectCreate(neutral)",
        ffi::direct_effect_create(
            context,
            audio_settings,
            &mut direct_settings,
            &mut direct_effect,
        ),
    )?;
    let direct_effect = NeutralDirectEffect(direct_effect as usize);

    let admits_indirect = presentation.admits_indirect();
    let path_effect = if admits_indirect {
        let mut path_settings = ffi::IPLPathEffectSettings {
            maxOrder: path_order,
            spatialize: ffi::IPL_FALSE,
            speakerLayout: ffi::IPLSpeakerLayout {
                type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
                numSpeakers: 0,
                speakers: core::ptr::null_mut(),
            },
            hrtf: core::ptr::null_mut(),
        };
        let mut path_effect = core::ptr::null_mut();
        sdk_status(
            "iplPathEffectCreate(neutral unspatialized)",
            ffi::path_effect_create(
                context,
                audio_settings,
                &mut path_settings,
                &mut path_effect,
            ),
        )?;
        Some(NeutralPathEffect(path_effect as usize))
    } else {
        None
    };

    let reflection_effect = if admits_indirect && reflection_send_enabled {
        let mut reflection_settings = ffi::IPLReflectionEffectSettings {
            type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)?,
            irSize: ir_size,
            numChannels: reflection_channels,
        };
        let mut reflection_effect = core::ptr::null_mut();
        sdk_status(
            "iplReflectionEffectCreate(neutral)",
            ffi::reflection_effect_create(
                context,
                audio_settings,
                &mut reflection_settings,
                &mut reflection_effect,
            ),
        )?;
        Some(NeutralReflectionEffect(reflection_effect as usize))
    } else {
        None
    };

    let line = match presentation {
        NeutralPresentationShape::LineSegment { .. } => Some(NeutralLineRenderState {
            renderer: LineWidthRenderer::new(audio_settings.samplingRate),
            presentation: OwnedAudioBuffer::allocate(context, 3, audio_settings.frameSize)?,
        }),
        NeutralPresentationShape::Point | NeutralPresentationShape::StereoImage { .. } => None,
    };
    let program_channels = match presentation {
        NeutralPresentationShape::StereoImage { .. } => 2,
        NeutralPresentationShape::Point | NeutralPresentationShape::LineSegment { .. } => 1,
    };
    Ok(NeutralSourceRenderState {
        direct_effect,
        path_effect,
        reflection_effect,
        reflection_ir_capacity: ir_size,
        program_input: OwnedAudioBuffer::allocate(
            context,
            program_channels,
            audio_settings.frameSize,
        )?,
        indirect_input: if admits_indirect {
            Some(OwnedAudioBuffer::allocate(
                context,
                1,
                audio_settings.frameSize,
            )?)
        } else {
            None
        },
        direct_output: OwnedAudioBuffer::allocate(
            context,
            direct_channels,
            audio_settings.frameSize,
        )?,
        path_field: if admits_indirect {
            Some(OwnedAudioBuffer::allocate(
                context,
                path_channels,
                audio_settings.frameSize,
            )?)
        } else {
            None
        },
        reflection_scratch: if admits_indirect && reflection_send_enabled {
            Some(OwnedAudioBuffer::allocate(
                context,
                reflection_channels,
                audio_settings.frameSize,
            )?)
        } else {
            None
        },
        propagation_smoother: SourcePropagationSmoother::default(),
        roof_head: crate::over_roof::RoofReadHead::default(),
        impulse_shapers: std::array::from_fn(|_| {
            ImpulseShaper::new(impulse_class, audio_settings.samplingRate)
        }),
        pathing_send_enabled,
        reflection_send_enabled,
        render_active: false,
        last_propagation_observation: None,
        quality_gains: [1.0; 3],
        presentation,
        line,
    })
}

#[allow(clippy::too_many_arguments)]
fn neutral_render_memory(
    sources: &Vec<NeutralSourceRenderState>,
    metadata_city_offsets: &Vec<ApiEnuVector3>,
    metadata_city_frame_enabled: &Vec<bool>,
    delays: &Vec<NeutralProgramDelay>,
    delayed_program: &Vec<[Vec<f32>; 2]>,
    reflection_mix: &Option<OwnedAudioBuffer>,
    program_interleaved_work: &Vec<f32>,
    roof_program_work: &[Vec<f32>; 2],
    effect_interleaved_work: &Vec<f32>,
    line_work: &Vec<f32>,
    steam_environment_bank: &Vec<f32>,
) -> crate::SpatialRenderMemoryTelemetry {
    let configured_stereo_source_count = delays
        .iter()
        .filter(|delay| delay.channel_count() == 2)
        .count() as u32;
    let delay_memory = delays.iter().fold(
        (0_u64, 0_u64, 0_u64),
        |(audio, geometry, additional), delay| {
            let memory = delay.memory();
            (
                audio.saturating_add(memory.audio_history_payload_bytes),
                geometry.saturating_add(memory.geometry_history_payload_bytes),
                additional.saturating_add(memory.additional_channel_payload_bytes),
            )
        },
    );
    let source_audio_buffers = sources.iter().fold(0_u64, |total, source| {
        let line = source
            .line
            .as_ref()
            .map_or(0, |line| line.presentation.payload_bytes());
        total
            .saturating_add(source.program_input.payload_bytes())
            .saturating_add(
                source
                    .indirect_input
                    .as_ref()
                    .map_or(0, OwnedAudioBuffer::payload_bytes),
            )
            .saturating_add(source.direct_output.payload_bytes())
            .saturating_add(
                source
                    .path_field
                    .as_ref()
                    .map_or(0, OwnedAudioBuffer::payload_bytes),
            )
            .saturating_add(
                source
                    .reflection_scratch
                    .as_ref()
                    .map_or(0, OwnedAudioBuffer::payload_bytes),
            )
            .saturating_add(line)
    });
    let steam_audio_buffer_payload_bytes = source_audio_buffers.saturating_add(
        reflection_mix
            .as_ref()
            .map_or(0, OwnedAudioBuffer::payload_bytes),
    );
    let delayed_program_scratch_samples = delayed_program
        .iter()
        .flat_map(|planes| planes.iter())
        .map(Vec::capacity)
        .sum::<usize>();
    let scratch_samples = delayed_program_scratch_samples
        .saturating_add(program_interleaved_work.capacity())
        .saturating_add(roof_program_work.iter().map(Vec::capacity).sum::<usize>())
        .saturating_add(effect_interleaved_work.capacity())
        .saturating_add(line_work.capacity())
        .saturating_add(steam_environment_bank.capacity());
    let rust_scratch_payload_bytes =
        (scratch_samples as u64).saturating_mul(size_of::<f32>() as u64);
    let delayed_program_scratch_payload_bytes =
        (delayed_program_scratch_samples as u64).saturating_mul(size_of::<f32>() as u64);
    let outer_vec_payload_bytes = (sources.capacity() as u64)
        .saturating_mul(size_of::<NeutralSourceRenderState>() as u64)
        .saturating_add(
            (metadata_city_offsets.capacity() as u64)
                .saturating_mul(size_of::<ApiEnuVector3>() as u64),
        )
        .saturating_add(
            (metadata_city_frame_enabled.capacity() as u64)
                .saturating_mul(size_of::<bool>() as u64),
        )
        .saturating_add(
            (delays.capacity() as u64).saturating_mul(size_of::<NeutralProgramDelay>() as u64),
        )
        .saturating_add(
            (delayed_program.capacity() as u64).saturating_mul(size_of::<[Vec<f32>; 2]>() as u64),
        );
    let total_tracked_payload_bytes = delay_memory
        .0
        .saturating_add(delay_memory.1)
        .saturating_add(steam_audio_buffer_payload_bytes)
        .saturating_add(rust_scratch_payload_bytes)
        .saturating_add(outer_vec_payload_bytes);
    crate::SpatialRenderMemoryTelemetry {
        source_capacity: MAX_ACTIVE_SOURCES as u32,
        configured_source_count: sources.len() as u32,
        configured_stereo_source_count,
        stereo_indirect_suppressed_source_count: configured_stereo_source_count,
        program_delay_audio_history_payload_bytes: delay_memory.0,
        program_delay_geometry_history_payload_bytes: delay_memory.1,
        additional_program_channel_payload_bytes: delay_memory.2,
        steam_audio_buffer_payload_bytes,
        delayed_program_scratch_payload_bytes,
        outer_vec_payload_bytes,
        rust_scratch_payload_bytes,
        total_tracked_payload_bytes,
        steam_audio_sdk_internal: MemoryTrackingStatus::Untracked,
    }
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

/// Returns true when this block's explicit trigger froze taps that trail a
/// routed primary but found no primary transfer, so they keep free-field law.
#[allow(clippy::too_many_arguments)]
fn render_echo_sidecar(
    echo: &mut EchoRenderState,
    dry_mono: &[f32],
    delivered_taps: u8,
    primary_transfer: Option<PrimaryTransfer>,
    listener: SteamPose,
    listener_position: SteamVector3,
    hrtf: usize,
    output_gain: f32,
    output_left: &mut [f32],
    output_right: &mut [f32],
    mono_work: &mut [f32],
    stereo_work: &mut [f32],
    reuse_silent_pairs: bool,
    spatial_export: Option<&mut FullSpatialExportTap>,
) -> bool {
    let frames = dry_mono.len();
    debug_assert_eq!(echo.tap_work.len(), frames * MAX_ECHO_TAPS_PER_SOURCE);
    echo.tap_work.fill(0.0);
    // An explicit trigger lands with this block's first dry sample. The delay
    // history is kept but gated: each new tap opens only once its own delay
    // has elapsed, so it never re-reads the previous shot.
    let mut transfer_fallback = false;
    if echo.pending_trigger {
        echo.pending_trigger = false;
        echo.freeze(delivered_taps);
        echo.primary_transfer = primary_transfer;
        echo.samples_since_trigger = 0;
        transfer_fallback = primary_transfer.is_none()
            && echo.active_plan.taps[..usize::from(echo.active_plan.tap_count)]
                .iter()
                .any(|tap| tap.inherits_primary);
    }
    for (frame, sample) in dry_mono.iter().copied().enumerate() {
        echo.delay.push(sample);
        let elapsed = echo.samples_since_trigger as f32;
        echo.samples_since_trigger = echo.samples_since_trigger.saturating_add(1);
        if !echo.trigger_mode && echo.scheduler.advance_sample(echo.profile) {
            echo.freeze(delivered_taps);
        }
        if !echo.has_triggered {
            continue;
        }
        let count = usize::from(echo.active_delivered_taps)
            .min(usize::from(echo.active_plan.tap_count))
            .min(MAX_ECHO_TAPS_PER_SOURCE);
        for tap_index in 0..count {
            let tap = echo.active_plan.taps[tap_index];
            if tap.valid && tap.delay_samples <= elapsed {
                echo.tap_work[tap_index * frames + frame] = echo.delay.read(tap.delay_samples);
            }
        }
    }

    render_echo_tap_work(
        echo,
        listener,
        listener_position,
        hrtf,
        output_gain,
        output_left,
        output_right,
        mono_work,
        stereo_work,
        reuse_silent_pairs,
        spatial_export,
    );
    transfer_fallback
}

#[allow(clippy::too_many_arguments)]
fn render_retiring_echo_sidecar(
    echo: &mut EchoRenderState,
    listener: SteamPose,
    listener_position: SteamVector3,
    hrtf: usize,
    output_gain: f32,
    output_left: &mut [f32],
    output_right: &mut [f32],
    mono_work: &mut [f32],
    stereo_work: &mut [f32],
    reuse_silent_pairs: bool,
) {
    let frames = output_left.len();
    debug_assert_eq!(output_right.len(), frames);
    debug_assert_eq!(echo.tap_work.len(), frames * MAX_ECHO_TAPS_PER_SOURCE);
    echo.tap_work.fill(0.0);
    for frame in 0..frames {
        echo.delay.push(0.0);
        let elapsed = echo.samples_since_trigger as f32;
        echo.samples_since_trigger = echo.samples_since_trigger.saturating_add(1);
        if !echo.has_triggered {
            continue;
        }
        let count = usize::from(echo.active_delivered_taps)
            .min(usize::from(echo.active_plan.tap_count))
            .min(MAX_ECHO_TAPS_PER_SOURCE);
        for tap_index in 0..count {
            let tap = echo.active_plan.taps[tap_index];
            if tap.valid && tap.delay_samples <= elapsed {
                echo.tap_work[tap_index * frames + frame] = echo.delay.read(tap.delay_samples);
            }
        }
    }
    render_echo_tap_work(
        echo,
        listener,
        listener_position,
        hrtf,
        output_gain,
        output_left,
        output_right,
        mono_work,
        stereo_work,
        reuse_silent_pairs,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_echo_tap_work(
    echo: &mut EchoRenderState,
    listener: SteamPose,
    listener_position: SteamVector3,
    hrtf: usize,
    output_gain: f32,
    output_left: &mut [f32],
    output_right: &mut [f32],
    mono_work: &mut [f32],
    stereo_work: &mut [f32],
    reuse_silent_pairs: bool,
    mut spatial_export: Option<&mut FullSpatialExportTap>,
) {
    let frames = output_left.len();
    let count = usize::from(echo.active_delivered_taps)
        .min(usize::from(echo.active_plan.tap_count))
        .min(MAX_ECHO_TAPS_PER_SOURCE);
    for tap_index in 0..count {
        let tap = echo.active_plan.taps[tap_index];
        if !tap.valid {
            continue;
        }
        crate::render_profile::count(8);
        mono_work.copy_from_slice(&echo.tap_work[tap_index * frames..(tap_index + 1) * frames]);
        if let Some(shaper) = &mut echo.tap_shapers[tap_index] {
            // The immutable key is total traveled path distance, never one leg
            // or the direct source-listener distance.
            let parameters = shaper.parameters_at_distance(tap.total_path_distance_m);
            for sample in mono_work.iter_mut() {
                *sample = shaper.process_sample(*sample, parameters);
            }
        }
        echo.input.write_mono(mono_work);
        let mut input = echo.input.raw();
        let (distance_gain, band_gain) = echo_tap_gains(&tap, echo.primary_transfer);
        let mut direct_params = ffi::IPLDirectEffectParams {
            flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
                | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION,
            transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
            distanceAttenuation: distance_gain,
            airAbsorption: band_gain,
            directivity: 1.0,
            occlusion: 1.0,
            transmission: [1.0; 3],
        };
        let mut binaural_params = ffi::IPLBinauralEffectParams {
            direction: relative_direction_steam(tap.arrival_position, listener_position, listener),
            interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
            spatialBlend: 1.0,
            hrtf: handle(hrtf),
            peakDelays: core::ptr::null_mut(),
        };
        echo.tap_silent_pairs[tap_index].render(
            reuse_silent_pairs,
            mono_work,
            echo.tap_direct_effects[tap_index],
            echo.tap_binaural_effects[tap_index],
            &mut direct_params,
            &mut binaural_params,
            &mut input,
            &mut echo.filtered,
            &mut echo.stereo,
            stereo_work,
        );
        if let Some(export) = spatial_export.as_deref_mut() {
            export.echo(&mut echo.filtered, tap.arrival_position, listener_position, output_gain);
        }
        accumulate_stereo_ramped(
            stereo_work,
            output_left,
            output_right,
            output_gain,
            GainRamp::new(1.0, 1.0, frames),
        );
    }
}

fn create_echo_render_state(
    context: ffi::IPLContext,
    audio_settings: &mut ffi::IPLAudioSettings,
    hrtf: ffi::IPLHRTF,
    maximum_delay_samples: usize,
    profile: EchoProfile,
) -> Result<EchoRenderState, BackendError> {
    let mut tap_direct_effects = [0; MAX_ECHO_TAPS_PER_SOURCE];
    let mut tap_binaural_effects = [0; MAX_ECHO_TAPS_PER_SOURCE];
    let mut direct_settings = ffi::IPLDirectEffectSettings { numChannels: 1 };
    let mut binaural_settings = ffi::IPLBinauralEffectSettings { hrtf };
    for tap_index in 0..MAX_ECHO_TAPS_PER_SOURCE {
        let mut direct = core::ptr::null_mut();
        sdk_status(
            "iplDirectEffectCreate(echo tap)",
            ffi::direct_effect_create(context, audio_settings, &mut direct_settings, &mut direct),
        )?;
        tap_direct_effects[tap_index] = direct as usize;
        let mut binaural = core::ptr::null_mut();
        sdk_status(
            "iplBinauralEffectCreate(echo tap)",
            ffi::binaural_effect_create(
                context,
                audio_settings,
                &mut binaural_settings,
                &mut binaural,
            ),
        )?;
        tap_binaural_effects[tap_index] = binaural as usize;
    }
    let frames = audio_settings.frameSize as usize;
    Ok(EchoRenderState {
        profile,
        scheduler: EchoLoopScheduler::default(),
        delay: EchoDelayRing::new(maximum_delay_samples),
        tap_direct_effects,
        tap_binaural_effects,
        tap_silent_pairs: Box::new(std::array::from_fn(|_| SteadySilentPair::new())),
        tap_shapers: std::array::from_fn(|_| {
            ImpulseShaper::new(profile.impulse_class(), audio_settings.samplingRate)
        }),
        input: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        filtered: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        stereo: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
        tap_work: vec![0.0; frames * MAX_ECHO_TAPS_PER_SOURCE],
        active_plan: EchoSourcePlan::default(),
        active_delivered_taps: 0,
        has_triggered: false,
        onset_plan: EchoSourcePlan::default(),
        trigger_mode: false,
        pending_trigger: false,
        primary_transfer: None,
        samples_since_trigger: u32::MAX,
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
    extent: ExtentDescriptor,
    impulse_class: fightbox_api::ImpulseClass,
    pathing_send_enabled: bool,
    reflection_send_enabled: bool,
    echo_profile: EchoProfile,
) -> Result<SourceRenderState, BackendError> {
    let line_length_m = match extent {
        ExtentDescriptor::LineSegment { length_m } => Some(length_m),
        _ => None,
    };
    let mut direct_settings = ffi::IPLDirectEffectSettings {
        numChannels: if line_length_m.is_some() { 3 } else { 1 },
    };
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
    let width = line_length_m
        .map(|length_m| {
            let mut plus_binaural = core::ptr::null_mut();
            sdk_status(
                "iplBinauralEffectCreate(line plus endpoint)",
                ffi::binaural_effect_create(
                    context,
                    audio_settings,
                    &mut binaural_settings,
                    &mut plus_binaural,
                ),
            )?;
            let mut minus_binaural = core::ptr::null_mut();
            sdk_status(
                "iplBinauralEffectCreate(line minus endpoint)",
                ffi::binaural_effect_create(
                    context,
                    audio_settings,
                    &mut binaural_settings,
                    &mut minus_binaural,
                ),
            )?;
            Ok::<_, BackendError>(LineWidthRenderState {
                length_m,
                renderer: LineWidthRenderer::new(audio_settings.samplingRate),
                plus_binaural_effect: plus_binaural as usize,
                minus_binaural_effect: minus_binaural as usize,
                silent_binaural: Box::new(std::array::from_fn(|_| SteadyBinaural::new())),
                presentation: OwnedAudioBuffer::allocate(context, 3, audio_settings.frameSize)?,
                direct: OwnedAudioBuffer::allocate(context, 3, audio_settings.frameSize)?,
            })
        })
        .transpose()?;
    let stereo_image = if let ExtentDescriptor::StereoImage { width_m } = extent {
        let mut delay = StereoProgramPropagationDelay::new(
            maximum_delay_samples,
            audio_settings.samplingRate,
        );
        delay.reset_to(initial_delay_samples);
        let mut stereo = StereoImageRenderState {
            width_m,
            direct_effect: 0,
            left_binaural_effect: 0,
            right_binaural_effect: 0,
            input: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
            direct: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
            delay,
            roof_head: crate::over_roof::RoofReadHead::default(),
            impulse_shapers: std::array::from_fn(|_| {
                ImpulseShaper::new(impulse_class, audio_settings.samplingRate)
            }),
        };
        let mut direct_settings = ffi::IPLDirectEffectSettings { numChannels: 2 };
        let mut stereo_direct = core::ptr::null_mut();
        sdk_status(
            "iplDirectEffectCreate(stereo image)",
            ffi::direct_effect_create(
                context,
                audio_settings,
                &mut direct_settings,
                &mut stereo_direct,
            ),
        )?;
        stereo.direct_effect = stereo_direct as usize;
        let mut left = core::ptr::null_mut();
        sdk_status(
            "iplBinauralEffectCreate(stereo left endpoint)",
            ffi::binaural_effect_create(context, audio_settings, &mut binaural_settings, &mut left),
        )?;
        stereo.left_binaural_effect = left as usize;
        let mut right = core::ptr::null_mut();
        sdk_status(
            "iplBinauralEffectCreate(stereo right endpoint)",
            ffi::binaural_effect_create(context, audio_settings, &mut binaural_settings, &mut right),
        )?;
        stereo.right_binaural_effect = right as usize;
        Some(stereo)
    } else {
        None
    };
    let echo = if echo_profile.is_enabled() {
        Some(create_echo_render_state(
            context,
            audio_settings,
            hrtf,
            maximum_delay_samples,
            echo_profile,
        )?)
    } else {
        None
    };
    Ok(SourceRenderState {
        direct_effect: direct as usize,
        binaural_effect: binaural as usize,
        path_effect: path as usize,
        reflection_effect: reflection as usize,
        reflection_ir_capacity: ir_size,
        input: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        direct_mono: OwnedAudioBuffer::allocate(context, 1, audio_settings.frameSize)?,
        direct_stereo: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
        direct_silent_pair: SteadySilentPair::new(),
        path_stereo: OwnedAudioBuffer::allocate(context, 2, audio_settings.frameSize)?,
        path_silent: SteadyPath::new(),
        reflection_scratch: OwnedAudioBuffer::allocate(
            context,
            channels,
            audio_settings.frameSize,
        )?,
        propagation_smoother: SourcePropagationSmoother::default(),
        impulse_shaper: ImpulseShaper::new(impulse_class, audio_settings.samplingRate),
        pathing_send_enabled,
        reflection_send_enabled,
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
        roof_head: crate::over_roof::RoofReadHead::default(),
        route_head: RouteReadHead::new(
            maximum_delay_samples,
            audio_settings.frameSize as usize,
            audio_settings.samplingRate,
        ),
        last_propagation_observation: None,
        rendered_since_reset: false,
        program_history: false,
        guard_reactivation_history: false,
        reactivation_epoch_samples: 0,
        quality_gains: [1.0; 3],
        reflection_channels: 0,
        reflection_adopted_sequence: 0,
        applied_reflections: SteamReflectionParams::default(),
        reflection_activity: ReflectionActivity::new(ir_size as u64 + audio_settings.frameSize as u64 * 2),
        width,
        stereo_image,
        echo,
    })
}

#[cfg(test)]
#[path = "neutral_multi_source_tests.rs"]
mod neutral_tests;

#[cfg(test)]
#[path = "multi_source_teleport_tests.rs"]
mod teleport_tests;

#[cfg(test)]
#[path = "megablock_corner_diagnostic.rs"]
mod megablock_corner_diagnostic;

#[cfg(test)]
#[path = "wave13_corner_gate.rs"]
mod wave13_corner_gate;

#[cfg(test)]
#[path = "wave14_echo_truth.rs"]
mod wave14_echo_truth;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "reflection_budget_diagnostics.rs"]
mod reflection_budget_diagnostics;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "width_binaural_prototype.rs"]
mod width_binaural_prototype;

#[cfg(test)]
#[path = "impulse_strip_prototype.rs"]
mod impulse_strip_prototype;

#[cfg(test)]
#[path = "legacy_eight_source_golden.rs"]
mod legacy_eight_source_golden;

#[cfg(all(test, feature = "linked-sdk"))]
#[path = "reflection_mailbox_tests.rs"]
mod reflection_mailbox_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_companion_native_ir_limit_preserves_ordinary_rays_bounces_and_duration() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let config = S3SimulationConfig { reflection_rays: 4096, reflection_bounces: 8,
            reflection_duration_s: 1.5, reflection_order: 1, ..test_config() };
        let position = ApiEnuVector3::new(0.5, 0.0, 2.0);
        let ordinary = MultiSourceDescriptor::at(position);
        let companion = ordinary.with_reflection_ir_limit_seconds(0.125)
            .with_reflection_simulation_ir_limit_seconds(0.125);
        let (mut simulation, _render) = build_multi_source_generation(
            &reflective_ground_mesh(), None, audio, config, &[ordinary, companion],
            1, QualityTier::Desktop,
        ).unwrap();
        let mut frame = one_source_update(true, position, ApiEnuVector3::new(0.0, 0.0, 2.0));
        frame.sources[1] = frame.sources[0]; simulation.update_inputs(&frame);
        simulation.run_direct().unwrap(); simulation.run_reflections().unwrap();
        assert_eq!(simulation.snapshot.sources[0].reflections.ir_size, 72000);
        assert_eq!(simulation.snapshot.sources[1].reflections.ir_size, 6000);
        let plan = simulation.reflection_budget_plan.unwrap(); assert_eq!(plan.group_count, 2);
        for group in &plan.groups {
            let budget = group.delivered(QualityTier::Desktop, simulation.governor.render_quality());
            assert_eq!(budget.rays, 4096); assert_eq!(budget.bounces, 8);
        }
        assert_eq!(plan.groups[0].requested.duration_s, 1.5);
        assert_eq!(plan.groups[1].requested.duration_s, 0.125);
    }

    #[test]
    fn linked_shared_reflections_sum_two_inputs_without_callback_allocation_and_fall_back() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let position = ApiEnuVector3::new(0.5, 0.0, 2.0);
        let descriptor = MultiSourceDescriptor::at(position).with_reflection_ir_limit_seconds(0.125);
        let build = |sharing: bool| {
            let d = if sharing { descriptor.with_reflection_share_radius_m(3.0) } else { descriptor };
            let (mut simulation, mut render) = build_multi_source_generation(
                &reflective_ground_mesh(), None, audio, test_config(), &[d, d],
                1, QualityTier::Desktop,
            ).unwrap();
            let mut frame = one_source_update(true, position, ApiEnuVector3::new(0.0, 0.0, 2.0));
            frame.sources[1] = frame.sources[0];
            simulation.update_inputs(&frame);
            simulation.run_direct().unwrap();
            simulation.run_reflections_for_realtime_prepare().unwrap();
            render.retire_silent_reflections = false;
            render.stage_output_gain_writer.as_mut().unwrap().publish(StageOutputGains {
                direct: 0.0, pathing: 0.0, reflections: 1.0,
            });
            // Warm both independent mailboxes before enabling the shared worker.
            for _ in 0..4 {
                let zero = [0.0; 128];
                let sources = [BackendSourceBlock { source_index: 0, input_mono: &zero },
                    BackendSourceBlock { source_index: 1, input_mono: &zero }];
                let mut left = zero; let mut right = zero;
                render.render_block(PropagationRenderBlock {
                    listener_orientation: ListenerOrientation { forward: ApiEnuVector3::new(0.0, 1.0, 0.0), up: ApiEnuVector3::new(0.0, 0.0, 1.0) },
                    sources: &sources, output_left: &mut left, output_right: &mut right,
                }).unwrap();
            }
            simulation.enable_reflection_worker(200_000_000).unwrap();
            (simulation, render, frame)
        };
        let (mut shared_sim, mut shared, mut frame) = build(true);
        let (_reference_sim, mut reference, _) = build(false);
        assert_eq!(shared_sim.shared_reflection_targets(shared_sim.governor.render_quality())[0], 1);
        let mut heard = 0.0_f32; let mut delta = 0.0_f32;
        let allocations = crate::propagation_delay_stereo_tests::count_allocations(|| {
            for block in 0..140 {
                let mut first = [0.0; 128]; let mut second = first;
                if block == 30 { first[0] = 0.0625; }
                if block == 35 { second[0] = 0.125; }
                let sources = [BackendSourceBlock { source_index: 0, input_mono: &first },
                    BackendSourceBlock { source_index: 1, input_mono: &second }];
                let mut a = [[0.0; 128]; 2]; let mut b = a;
                for (render, output) in [(&mut shared, &mut a), (&mut reference, &mut b)] {
                    let [left, right] = output;
                    render.render_block(PropagationRenderBlock {
                        listener_orientation: ListenerOrientation { forward: ApiEnuVector3::new(0.0, 1.0, 0.0), up: ApiEnuVector3::new(0.0, 0.0, 1.0) },
                        sources: &sources, output_left: left, output_right: right,
                    }).unwrap();
                }
                for (a, b) in a.iter().flatten().zip(b.iter().flatten()) {
                    heard = heard.max(a.abs()); delta = delta.max((a-b).abs());
                }
            }
        });
        assert_eq!(allocations, 0);
        assert!(heard > 1e-6, "nonzero reflection required");
        assert!(delta < heard * 1e-3, "shared sum differs: peak={heard} delta={delta}");
        let initial = [shared_sim.snapshot.sources[0].reflection_sequence, shared_sim.snapshot.sources[1].reflection_sequence];
        shared_sim.run_reflections().unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(5);
        while shared_sim.reflection_worker_busy {
            shared_sim.poll_reflection_worker().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(shared_sim.snapshot.sources[0].reflection_sequence, initial[0]);
        assert_eq!(shared_sim.snapshot.sources[1].reflection_sequence, initial[1] + 1);
        frame.sources[0].pose.position.east_m += 20.0;
        shared_sim.update_inputs(&frame);
        assert_eq!(shared_sim.shared_reflection_targets(shared_sim.governor.render_quality())[0], 0);
        frame.sources[0].pose.position = frame.sources[1].pose.position;
        frame.sources[1].active = false;
        shared_sim.update_inputs(&frame);
        assert_eq!(shared_sim.shared_reflection_targets(shared_sim.governor.render_quality())[0], 0);
    }

    #[test]
    fn linked_companion_cadence_preserves_gun_updates_and_forces_movement_refresh() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let config = test_config();
        let ordinary = MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5));
        assert!(validate_multi_source_config(&reflective_ground_mesh(), None, audio, config,
            &[ordinary.with_reflection_update_divisor(0)], QualityTier::Desktop).is_err());
        let companion = MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5))
            .with_reflection_ir_limit_seconds(0.125).with_reflection_update_divisor(5);
        let (mut simulation, mut render) = build_multi_source_generation(
            &reflective_ground_mesh(), None, audio, config, &[ordinary, companion],
            1, QualityTier::Desktop,
        ).unwrap();
        let mut frame = update(true, true);
        simulation.update_inputs(&frame);
        simulation.run_direct().unwrap();
        for _ in 0..20_000 { simulation.observe_render_timing(100_000); }
        simulation.run_reflections_for_realtime_prepare().unwrap();
        render.retire_silent_reflections = false;
        let acknowledge = |render: &mut MultiSourceRenderGraph| {
            let mut impulse = [0.0; 128]; impulse[0] = 0.0625;
            let sources = [BackendSourceBlock { source_index: 0, input_mono: &impulse },
                BackendSourceBlock { source_index: 1, input_mono: &impulse }];
            let mut left = [0.0; 128]; let mut right = [0.0; 128];
            render.render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation { forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0) },
                sources: &sources, output_left: &mut left, output_right: &mut right,
            }).unwrap();
            assert!(left.iter().chain(&right).all(|x| x.is_finite()));
        };
        acknowledge(&mut render);
        acknowledge(&mut render);
        for index in 0..2 { assert!(simulation.reflection_publication_acknowledged(index)); }
        let initial = [simulation.snapshot.sources[0].reflection_sequence,
            simulation.snapshot.sources[1].reflection_sequence];
        simulation.enable_reflection_worker(200_000_000).unwrap();
        let complete = |simulation: &mut MultiSourceSimulation| {
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while simulation.reflection_worker_busy {
                simulation.poll_reflection_worker().unwrap();
                assert!(Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        for _ in 0..6 {
            simulation.run_reflections().unwrap();
            complete(&mut simulation);
            acknowledge(&mut render);
            acknowledge(&mut render);
        }
        assert_eq!(simulation.snapshot.sources[0].reflection_sequence, initial[0] + 6);
        assert_eq!(simulation.snapshot.sources[1].reflection_sequence, initial[1] + 2);
        frame.sources[1].pose.position.east_m += 1.0;
        simulation.update_inputs(&frame);
        simulation.run_direct().unwrap();
        simulation.run_reflections().unwrap();
        complete(&mut simulation);
        acknowledge(&mut render);
        assert_eq!(simulation.snapshot.sources[0].reflection_sequence, initial[0] + 7);
        assert_eq!(simulation.snapshot.sources[1].reflection_sequence, initial[1] + 3);
    }

    #[test]
    fn linked_source_reflection_ir_cap_preserves_prefix_and_other_source_capacity() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let config = S3SimulationConfig {
            reflection_rays: 4_096, reflection_bounces: 8,
            reflection_duration_s: 1.5, reflection_order: 1,
            ..test_config()
        };
        let position = ApiEnuVector3::new(0.5, 0.0, 2.0);
        let ordinary = MultiSourceDescriptor::at(position);
        let capped = ordinary.with_reflection_ir_limit_seconds(0.125);
        for invalid in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert!(validate_multi_source_config(&reflective_ground_mesh(), None, audio,
                config, &[ordinary.with_reflection_ir_limit_seconds(invalid)],
                QualityTier::Desktop).is_err());
        }
        let (mut simulation, render) = build_multi_source_generation(
            &reflective_ground_mesh(), None, audio, config, &[ordinary, capped],
            1, QualityTier::Desktop,
        ).unwrap();
        assert_eq!(render.sources[0].reflection_ir_capacity, 72_000);
        assert_eq!(render.sources[1].reflection_ir_capacity, 6_000);
        for _ in 0..20_000 { simulation.observe_render_timing(100_000); }
        let mut update = one_source_update(true, position, ApiEnuVector3::new(0.0, 0.0, 2.0));
        update.sources[1] = update.sources[0];
        simulation.update_inputs(&update);
        for _ in 0..4 { simulation.run_reflections().unwrap(); }
        let ir = simulation.snapshot.sources[0].reflections;
        assert_eq!(ir.ir_size, 72_000, "simulation retains the full gun field");
        assert_eq!(simulation.snapshot.sources[1].reflections.ir_size, 72_000);
        let mut input = OwnedAudioBuffer::allocate(render.world.context(), 1, 128).unwrap();
        let mut output = OwnedAudioBuffer::allocate(render.world.context(), ir.num_channels, 128).unwrap();
        let mut mono = [0.0; 128];
        let mut interleaved = vec![0.0; 128 * ir.num_channels as usize];
        let frames = 9_600;
        let mut responses = [vec![0.0; frames * ir.num_channels as usize],
            vec![0.0; frames * ir.num_channels as usize]];
        let allocations = crate::propagation_delay_stereo_tests::count_allocations(|| {
            for (index, (source, response)) in render.sources.iter().zip(&mut responses).enumerate() {
                for block in 0..frames / 128 {
                    mono.fill(0.0);
                    if block == 0 { mono[0] = 1.0; }
                    input.write_mono(&mut mono);
                    let mut raw_input = input.raw();
                    let mut raw_output = output.raw();
                    // Each native IR mailbox has exactly one effect consumer.
                    let mut params = reflection_effect_params(
                        simulation.snapshot.sources[index].reflections, config);
                    params.irSize = params.irSize.min(source.reflection_ir_capacity);
                    ffi::reflection_effect_apply(handle(source.reflection_effect),
                        &mut params, &mut raw_input, &mut raw_output);
                    output.read_interleaved(&mut interleaved);
                    let start = block * interleaved.len();
                    response[start..start + interleaved.len()].copy_from_slice(&interleaved);
                }
            }
        });
        assert_eq!(allocations, 0);
        let prefix = 4_800 * ir.num_channels as usize;
        let peak = responses[0][..prefix].iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        assert!(peak > 1e-8, "nonzero physical reflection is required");
        let error = responses[0][..prefix].iter().zip(&responses[1][..prefix])
            .map(|(full, short)| (full - short).abs()).fold(0.0_f32, f32::max);
        assert!(error < peak * 1e-4, "the first100ms changed: peak={peak} error={error}");
        let end = (6_000 + 128) * ir.num_channels as usize;
        let late = responses[1][end..].iter().map(|x| x.abs()).fold(0.0_f32, f32::max);
        assert!(late < peak * 1e-4, "short effect retained a long tail: {late}");
    }

    #[test]
    fn linked_steady_silent_pairs_preserve_quiet_tail_resume_and_parameter_changes() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let profile = EchoProfile::from_loop_frames(1_000_000, &[0], fightbox_api::ImpulseClass::None).unwrap();
        let descriptors = [
            MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5)).with_echo_profile(profile),
            MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5)),
            MultiSourceDescriptor::at(ApiEnuVector3::new(3.0, 2.0, 1.5))
                .with_extent(ExtentDescriptor::LineSegment { length_m: 6.0 }),
        ];
        let build = || {
            let (_, mut render) = build_multi_source_generation(
                &reflective_ground_mesh(), None, audio, test_config(), &descriptors,
                1, QualityTier::Desktop,
            ).unwrap();
            let echo = render.sources[0].echo.as_mut().unwrap();
            echo.onset_plan.tap_count = 2;
            for (index, tap) in echo.onset_plan.taps[..2].iter_mut().enumerate() {
                *tap = EchoTapPlan {
                    valid: true, distance_gain: 0.25 + index as f32 * 0.25,
                    band_gain: [0.1, 0.4, 0.8],
                    arrival_position: api_enu_to_steam(ApiEnuVector3::new(index as f32 + 1.0, 3.0, 1.5)),
                    ..EchoTapPlan::default()
                };
            }
            echo.freeze(2);
            render
        };
        let mut optimized = build();
        let mut reference = build();
        let mut heard_tail = false;
        for block in 0..1_600 {
            let mut dry = [0.0; 128];
            if block == 80 || block == 700 { dry[0] = 0.125; }
            if block == 1_210 { dry.fill(-0.0); }
            let changed_gain = if block >= 900 { 0.6 } else { 0.9 };
            let direction = if block >= 1_100 { ffi::IPLVector3 { x: 0.3, y: 0.1, z: -0.9 } }
                else { ffi::IPLVector3 { x: 0.2, y: 0.0, z: -1.0 } };
            let mut outputs = [[[0.0; 128]; 2]; 2];
            let [optimized_output, reference_output] = &mut outputs;
            for (render, enabled, output) in [
                (&mut optimized, true, optimized_output),
                (&mut reference, false, reference_output),
            ] {
                let state = &mut render.sources[0];
                if block == 1_300 {
                    state.direct_silent_pair.reset();
                    ffi::direct_effect_reset(handle(state.direct_effect));
                    ffi::binaural_effect_reset(handle(state.binaural_effect));
                }
                state.input.write_mono(&mut dry);
                let mut input = state.input.raw();
                let mut direct = ffi::IPLDirectEffectParams {
                    flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION,
                    transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
                    distanceAttenuation: changed_gain, airAbsorption: [0.1, 0.4, 0.8],
                    directivity: 1.0, occlusion: 1.0, transmission: [1.0; 3],
                };
                let mut binaural = ffi::IPLBinauralEffectParams {
                    direction, interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
                    spatialBlend: 1.0, hrtf: handle(render.hrtf), peakDelays: core::ptr::null_mut(),
                };
                let reused_before = state.direct_silent_pair.reused_blocks;
                state.direct_silent_pair.render(
                    enabled, &dry, state.direct_effect, state.binaural_effect,
                    &mut direct, &mut binaural, &mut input, &mut state.direct_mono,
                    &mut state.direct_stereo, &mut render.stereo_work,
                );
                if [80, 700, 900, 1_100, 1_210, 1_300].contains(&block) {
                    assert_eq!(state.direct_silent_pair.reused_blocks, reused_before, "wake at block {block}");
                }
                heard_tail |= block == 81 && render.stereo_work.iter().any(|sample| *sample != 0.0);
                let [left, right] = output;
                accumulate_stereo_ramped(&render.stereo_work, left, right, 1.0, GainRamp::new(1.0, 1.0, 128));
                let echo = state.echo.as_mut().unwrap();
                if block == 600 { echo.freeze(2); }
                for (index, tap) in echo.active_plan.taps[..2].iter_mut().enumerate() {
                    tap.distance_gain = changed_gain * (0.25 + index as f32 * 0.25);
                }
                for tap in echo.tap_work.chunks_exact_mut(128) { tap.copy_from_slice(&dry); }
                let listener = listener_pose(ListenerOrientation {
                    forward: ApiEnuVector3::new(if block >= 1_100 { 0.5 } else { 0.0 }, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                }).unwrap();
                render_echo_tap_work(
                    echo, listener, api_enu_to_steam(ApiEnuVector3::new(0.0, 0.0, 1.5)),
                    render.hrtf, 1.0, left, right, &mut render.mono_work, &mut render.stereo_work, enabled,
                    None,
                );
                let line = &mut render.sources[2];
                let width = line.width.as_mut().unwrap();
                if block == 1_300 {
                    width.renderer.reset();
                    for cache in width.silent_binaural.iter_mut() { cache.reset(); }
                    ffi::direct_effect_reset(handle(line.direct_effect));
                    for effect in [line.binaural_effect, width.plus_binaural_effect, width.minus_binaural_effect] {
                        ffi::binaural_effect_reset(handle(effect));
                    }
                }
                width.renderer.render_presentation(&dry, 0.3, false, &mut render.width_work);
                width.presentation.write_interleaved(&mut render.width_work);
                let mut width_input = width.presentation.raw();
                let mut width_output = width.direct.raw();
                ffi::direct_effect_apply(handle(line.direct_effect), &mut direct, &mut width_input, &mut width_output);
                width.direct.read_interleaved(&mut render.width_work);
                for (channel, effect) in [line.binaural_effect, width.plus_binaural_effect, width.minus_binaural_effect].into_iter().enumerate() {
                    for (frame, sample) in render.width_feed_work.iter_mut().enumerate() {
                        *sample = render.width_work[frame * 3 + channel];
                    }
                    line.direct_mono.write_mono(&mut render.width_feed_work);
                    let mut filtered = line.direct_mono.raw();
                    width.silent_binaural[channel].render(
                        enabled && dry.iter().all(|sample| *sample == 0.0), &render.width_feed_work,
                        effect, &mut binaural, &mut filtered, &mut line.direct_stereo, &mut render.stereo_work,
                    );
                    accumulate_stereo_ramped(&render.stereo_work, left, right, 1.0, GainRamp::new(1.0, 1.0, 128));
                }
                let path = &mut render.sources[0];
                if block == 1_300 {
                    path.path_silent.reset();
                    ffi::path_effect_reset(handle(path.path_effect));
                }
                let mut sh = [0.0; 16];
                if !(350..600).contains(&block) {
                    sh[0] = if block >= 900 { 0.19 } else { 0.28 };
                    sh[1] = 0.03;
                }
                let mut path_params = ffi::IPLPathEffectParams {
                    eqCoeffs: [0.1, 0.4, 0.8], shCoeffs: sh.as_mut_ptr(),
                    order: if block >= 1_400 { 0 } else { 1 }, binaural: ffi::IPL_TRUE,
                    hrtf: handle(render.hrtf), listener: coordinate_space(listener).unwrap(),
                    normalizeEQ: if block >= 1_500 { ffi::IPL_TRUE } else { ffi::IPL_FALSE },
                };
                path.input.write_mono(&mut dry);
                let mut input = path.input.raw();
                let before = path.path_silent.reused_blocks;
                path.path_silent.render(enabled, &dry, &sh, path.path_effect,
                    &mut path_params, &mut input, &mut path.path_stereo, &mut render.stereo_work);
                if (350..600).contains(&block) {
                    assert_eq!(path.path_silent.reused_blocks, before, "zero SH must advance hidden EQ");
                }
                accumulate_stereo_ramped(&render.stereo_work, left, right, 1.0, GainRamp::new(1.0, 1.0, 128));
            }
            for (a, b) in outputs[0].iter().flatten().zip(outputs[1].iter().flatten()) {
                assert_eq!(a.to_bits(), b.to_bits(), "block {block}");
            }
        }
        assert!(heard_tail);
        assert!(optimized.sources[0].direct_silent_pair.reused_blocks > 100);
        assert!(optimized.sources[0].path_silent.reused_blocks > 100);
        let echo = optimized.sources[0].echo.as_ref().unwrap();
        assert!(echo.tap_silent_pairs[..2].iter().all(|cache| cache.reused_blocks > 100));
        let width = optimized.sources[2].width.as_ref().unwrap();
        assert!(width.silent_binaural.iter().all(|cache| cache.reused_blocks > 100));
    }

    #[test]
    fn silent_reflection_retirement_drains_and_primes_ir_handoffs() {
        let mut activity = ReflectionActivity::new(384);
        assert!(!activity.advance(false, 128, 0, true));
        assert!(activity.advance(false, 128, 1, true));
        assert!(!activity.advance(false, 128, 1, true));
        assert!(activity.advance(true, 128, 1, true));
        for _ in 0..3 {
            assert!(activity.advance(false, 128, 1, true));
        }
        assert!(!activity.advance(false, 128, 1, true));
        assert!(activity.advance(false, 128, 2, true));
        assert!(!activity.advance(false, 128, 2, true));
        assert!(activity.advance(true, 128, 2, true));
        activity.reset();
        assert!(!activity.has_history());
        assert!(!activity.advance(false, 128, 3, false));
        assert_eq!(activity.sequence, 0);
        assert!(activity.advance(false, 128, 3, true));
        activity.observe_dry(true, 256);
        assert!(activity.advance(false, 128, 4, false));
        assert!(activity.advance(false, 128, 5, false));
        assert!(!activity.advance(false, 128, 6, false));
    }

    #[test]
    fn linked_silent_reflection_retirement_preserves_tail_and_resumed_pcm() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let descriptors = [
            MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5)),
            MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5)),
        ];
        let build = || {
            let (mut simulation, mut render) = build_multi_source_generation(
                &reflective_ground_mesh(), None, audio, test_config(), &descriptors,
                1, QualityTier::Desktop,
            ).unwrap();
            simulation.update_inputs(&update(true, true));
            simulation.run_direct().unwrap();
            simulation.run_reflections().unwrap();
            render.retire_silent_reflections = true;
            render.stage_output_gain_writer.as_mut().unwrap().publish(StageOutputGains {
                direct: 0.0, pathing: 0.0, reflections: 1.0,
            });
            (simulation, render)
        };
        let (mut optimized_sim, mut optimized) = build();
        let (mut reference_sim, mut reference) = build();
        reference.retire_silent_reflections = false;
        let zeros = [0.0; 128];
        let mut impulse = zeros;
        impulse[0] = 0.125;
        let mut heard_tail = false;
        let mut maximum_delta = 0.0_f32;
        for block in 0..140 {
            if block % 23 == 0 {
                optimized_sim.run_reflections().unwrap();
                reference_sim.run_reflections().unwrap();
            }
            let input = if block == 30 || block == 100 { &impulse } else { &zeros };
            let sources = [
                fightbox_runtime::backend::BackendSourceBlock { source_index: 0, input_mono: input },
                fightbox_runtime::backend::BackendSourceBlock { source_index: 1, input_mono: &zeros },
            ];
            let mut a = [[0.0; 128]; 2];
            let mut b = [[0.0; 128]; 2];
            for (render, output) in [(&mut optimized, &mut a), (&mut reference, &mut b)] {
                let [left, right] = output;
                render.render_block(PropagationRenderBlock {
                    listener_orientation: ListenerOrientation {
                        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                    }, sources: &sources,
                    output_left: left, output_right: right,
                }).unwrap();
            }
            for (sample_a, sample_b) in a.iter().flatten().zip(b.iter().flatten()) {
                maximum_delta = maximum_delta.max((sample_a - sample_b).abs());
            }
            heard_tail |= optimized.stereo_work.iter().any(|sample| *sample != 0.0);
        }
        assert!(heard_tail);
        // Sleeping zero input rotates the FFT ring's summation order. Keep
        // its roundoff below -160 dBFS; the legacy route stays bit-exact.
        assert!(maximum_delta < 1.0e-8, "reflection delta {maximum_delta}");
        eprintln!("silent reflection maximum PCM delta: {maximum_delta:e}");
    }
    use crate::AcousticMaterial;
    use crate::{
        BallisticEventLevels, BallisticShot, BallisticShotPlan, plan_ballistic_shot,
        synthesize_crack_stem,
    };
    use fightbox_api::{
        AssetAnalysis, AssetMeasurementProvenance, EngineConfig, ReferenceLevel, SceneCalibration,
        SourceId, SourceProfile,
    };
    use fightbox_runtime::backend::{
        BackendSourceBlock, SimulationRunner, SourceMotion, SpatialBackendSourceBlock,
        SpatialOutputMetadata,
    };
    use fightbox_runtime::{
        BlockProcessor, ProcessBlock, PropagationSnapshot, RuntimeGraph, SnapshotPublication,
        SourceBlock, SourcePropagation,
    };
    use std::f32::consts::TAU;

    fn explicit_reflection_capacity() -> S3SimulationConfig {
        S3SimulationConfig {
            reflection_rays: SourceReflectionBudget::CINEMATIC.rays,
            reflection_bounces: SourceReflectionBudget::CINEMATIC.bounces,
            reflection_duration_s: SourceReflectionBudget::CINEMATIC.duration_s,
            reflection_order: SourceReflectionBudget::CINEMATIC.order,
            ..S3SimulationConfig::default()
        }
    }

    #[test]
    fn city_metadata_offset_keeps_direction_feed_exact_across_cell_local_frames() {
        let propagation = SteamSourcePropagation {
            source_forward: api_enu_to_steam(ApiEnuVector3::new(0.0, 1.0, 0.0)),
            source_up: api_enu_to_steam(ApiEnuVector3::new(0.0, 0.0, 1.0)),
            ..SteamSourcePropagation::default()
        };
        let source_city = ApiEnuVector3::new(1_246.221_4, -997.333_6, 1.543_2);
        let listener_city = ApiEnuVector3::new(1_247.719_8, -947.812_3, 1.499_6);
        let offset = ApiEnuVector3::new(1_003.777_2, -888.444_1, 0.0);
        let source_local = ApiEnuVector3::new(
            source_city.east_m - offset.east_m,
            source_city.north_m - offset.north_m,
            source_city.up_m - offset.up_m,
        );
        let listener_local = ApiEnuVector3::new(
            listener_city.east_m - offset.east_m,
            listener_city.north_m - offset.north_m,
            listener_city.up_m - offset.up_m,
        );
        let mut west = SpatialOutputMetadata::default();
        let mut east = SpatialOutputMetadata::default();
        NeutralMultiSourceRenderGraph::mark_feed(
            &mut west,
            0,
            SpatialPresentationComponent::DirectCenter,
            api_enu_to_steam(source_city),
            api_enu_to_steam(listener_city),
            ApiEnuVector3::default(),
            true,
            propagation,
            0,
        )
        .expect("west metadata");
        NeutralMultiSourceRenderGraph::mark_feed(
            &mut east,
            0,
            SpatialPresentationComponent::DirectCenter,
            api_enu_to_steam(source_local),
            api_enu_to_steam(listener_local),
            offset,
            true,
            propagation,
            0,
        )
        .expect("east metadata");
        assert_eq!(west.presentation_feeds[0], east.presentation_feeds[0]);
        assert_eq!(
            west.presentation_feeds[0].pose_enu.position,
            ApiEnuVector3::new(1_246.221, -997.334, 1.543)
        );
    }

    #[test]
    fn reflection_budget_plan_keeps_the_legacy_path_structurally_absent() {
        let descriptors = [crate::MultiSourceDescriptor::at(ApiEnuVector3::default())];
        assert_eq!(
            build_reflection_budget_plan(
                S3SimulationConfig::default(),
                &descriptors,
                QualityTier::Desktop,
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn reflection_budget_plan_accepts_one_featured_and_three_standard_sources() {
        let descriptors = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::default())
                .with_reflection_budget(SourceReflectionBudget::CINEMATIC),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0))
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 0.0, 0.0))
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(3.0, 0.0, 0.0))
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
        ];
        let plan = build_reflection_budget_plan(
            explicit_reflection_capacity(),
            &descriptors,
            QualityTier::Desktop,
        )
        .unwrap()
        .unwrap();
        assert_eq!(plan.group_count, 2);
        assert_ne!(plan.source_groups[0], plan.source_groups[1]);
        assert_eq!(plan.source_groups[1], plan.source_groups[2]);
        assert_eq!(plan.source_groups[2], plan.source_groups[3]);
    }

    #[test]
    fn reflection_budget_plan_rejects_mixed_inheritance_and_mobile_overcommit() {
        let mixed = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::default())
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0)),
        ];
        assert!(
            build_reflection_budget_plan(
                explicit_reflection_capacity(),
                &mixed,
                QualityTier::Desktop,
            )
            .is_err()
        );

        let mobile = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::default())
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0))
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
        ];
        assert!(
            build_reflection_budget_plan(
                explicit_reflection_capacity(),
                &mobile,
                QualityTier::Mobile,
            )
            .is_err()
        );
    }

    #[test]
    fn reflection_budget_groups_publish_cinematic_and_standard_ir_shapes_together() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptors = [
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5))
                .with_reflection_budget(SourceReflectionBudget::CINEMATIC),
            crate::MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5))
                .with_reflection_budget(SourceReflectionBudget::STANDARD),
        ];
        let (mut simulation, _render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            explicit_reflection_capacity(),
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        simulation.update_inputs(&update(true, true));
        simulation.run_direct().unwrap();
        for _ in 0..20_000 {
            simulation.observe_render_timing(100_000);
        }
        assert_eq!(
            simulation.quality_governor_telemetry().reflections.level,
            ReflectionQualityLevel::Full
        );
        simulation.run_reflections().unwrap();

        let cinematic = simulation.snapshot.sources[0].reflections;
        let standard = simulation.snapshot.sources[1].reflections;
        assert_eq!(cinematic.num_channels, 9);
        assert_eq!(cinematic.ir_size, 144_000);
        assert_eq!(standard.num_channels, 4);
        assert_eq!(standard.ir_size, 48_000);
        assert_eq!(
            simulation.work_counters.vendor_pass_runs[GovernorSimulationPass::Reflections.index()],
            2
        );
    }

    #[test]
    fn reflection_worker_pairs_native_publications_through_teleport_and_shutdown() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let descriptors = [
            MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 3.0, 1.5)),
            MultiSourceDescriptor::at(ApiEnuVector3::new(5.0, 2.0, 1.5)),
            MultiSourceDescriptor::at(ApiEnuVector3::default()).with_reflection_send(false),
        ];
        let render_block = |render: &mut MultiSourceRenderGraph| {
            let zeros = [0.0; 128];
            let mut impulse = zeros;
            impulse[0] = 0.0625;
            let sources = [
                BackendSourceBlock { source_index: 0, input_mono: &impulse },
                BackendSourceBlock { source_index: 1, input_mono: &zeros },
            ];
            let mut left = zeros;
            let mut right = zeros;
            render.render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                }, sources: &sources, output_left: &mut left, output_right: &mut right,
            }).unwrap();
            assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
        };
        let completion = |simulation: &MultiSourceSimulation| {
            let timeout = Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Some(result) = simulation.reflection_worker.as_ref().unwrap().poll().unwrap() {
                    return result;
                }
                assert!(Instant::now() < timeout, "reflection worker did not complete");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        };
        for source_count in [2, 3] {
            let (mut simulation, mut render) = build_multi_source_generation(
                &reflective_ground_mesh(), None, audio, test_config(), &descriptors[..source_count],
                1, QualityTier::Desktop,
            ).unwrap();
            assert_eq!(render.reflection_ir_hold != 0, source_count == 3);
            let retained = Arc::downgrade(&simulation.world);
            let mut frame = update(true, true);
            simulation.update_inputs(&frame);
            simulation.run_direct().unwrap();
            simulation.run_reflections_for_realtime_prepare().unwrap();
            render.retire_silent_reflections = false;
            render_block(&mut render);
            render_block(&mut render);
            render.retire_silent_reflections = true;
            let initial_sequence = simulation.snapshot.sources[0].reflection_sequence;
            assert_eq!(render.sources[0].reflection_channels, 4);
            simulation.enable_reflection_worker(40_000_000).unwrap();

            let mut source_groups = [NO_REFLECTION_GROUP; MAX_ACTIVE_SOURCES];
            source_groups[..2].fill(0);
            simulation.reflection_budget_plan = Some(ReflectionBudgetPlan {
                groups: [ReflectionBudgetGroup {
                    requested: SourceReflectionBudget::realtime(64, 1, 0.05, 0, 4), inherit_shared_quality: false, tick: 0,
                }, ReflectionBudgetGroup::default()],
                group_count: 1, source_groups, quality_tier: QualityTier::Desktop,
            });
            simulation.run_reflections().unwrap();
            let stale = completion(&simulation); // Native IR A is published; Rust has not polled it.
            assert_eq!(stale.params[0].unwrap().num_channels, 1);
            let direct_sequence = simulation.latest_direct_sequence();
            frame.sources[0].pose.position.east_m += 100.0;
            simulation.update_inputs(&frame);
            simulation.run_direct().unwrap();
            assert!(simulation.latest_direct_sequence() > direct_sequence);
            render_block(&mut render);
            assert_eq!(render.sources[0].reflection_adopted_sequence, initial_sequence);
            assert_eq!(render.sources[0].reflection_channels, 4);

            simulation.publish_reflection_completion(stale);
            assert_eq!(simulation.snapshot.sources[0].reflection_sequence, initial_sequence + 1);
            assert!(simulation.reflection_forced_due[0]);
            let runs = simulation.work_counters.vendor_pass_runs[GovernorSimulationPass::Reflections.index()];
            simulation.run_reflections().unwrap();
            simulation.run_reflections_for_realtime_prepare().unwrap();
            assert!(!simulation.reflection_worker_busy, "unread A must prevent publication B");
            assert_eq!(simulation.work_counters.vendor_pass_runs[GovernorSimulationPass::Reflections.index()], runs);
            for _ in 0..2 { render_block(&mut render); }
            for index in 0..2 {
                assert_eq!(render.sources[index].reflection_adopted_sequence, initial_sequence + 1);
                assert_eq!(render.sources[index].reflection_channels, 1);
                assert!(simulation.reflection_publication_acknowledged(index));
            }

            let plan = simulation.reflection_budget_plan.as_mut().unwrap();
            plan.groups[0].requested.order = 1;
            plan.groups[0].tick = 1; // Teleport forces the fresh pass despite cadence four.
            simulation.run_reflections().unwrap();
            assert!(simulation.reflection_worker_busy);
            let fresh = completion(&simulation);
            assert_eq!(fresh.params[0].unwrap().num_channels, 4);
            render.retire_silent_reflections = false;
            render.reset_scene_history();
            render.retire_silent_reflections = true;
            assert_eq!(render.sources[0].reflection_adopted_sequence, initial_sequence + 1);
            render_block(&mut render);
            assert_eq!(render.sources[0].reflection_adopted_sequence, initial_sequence + 1);
            assert_eq!(render.sources[0].reflection_channels, 1);
            simulation.publish_reflection_completion(fresh);
            assert_eq!(simulation.work_counters.vendor_pass_runs[GovernorSimulationPass::Reflections.index()], runs + 1);
            assert!(!simulation.reflection_forced_due[0]);
            for _ in 0..2 { render_block(&mut render); }
            for index in 0..2 {
                assert_eq!(render.sources[index].reflection_adopted_sequence, initial_sequence + 2);
                assert_eq!(render.sources[index].reflection_channels, 4);
                assert!(simulation.reflection_publication_acknowledged(index));
            }
            simulation.reflection_budget_plan.as_mut().unwrap().groups[0].tick = 0;
            simulation.run_reflections().unwrap();
            assert!(simulation.reflection_worker_busy);
            drop(simulation); // Joins an outstanding native job before native teardown.
            drop(render);
            assert!(retained.upgrade().is_none());
        }
    }

    #[test]
    fn simulation_cadence_target_changes_discard_incomparable_start_history() {
        let mut cadence = SimulationPassCadence::default();
        let fast_interval_ns = 400_000_000;
        let slow_interval_ns = 800_000_000;

        assert_eq!(cadence.observe_start(400_000_000, fast_interval_ns), 0);
        cadence.observe_target_interval(slow_interval_ns);
        cadence.observe_target_interval(fast_interval_ns);
        assert_eq!(
            cadence.observe_start(1_200_000_000, fast_interval_ns),
            0,
            "a policy-owned downshift and recovery must not look like a missed fast-cadence pass"
        );
        assert_eq!(
            cadence.observe_start(1_610_000_000, fast_interval_ns),
            10_000_000,
            "comparable consecutive starts must still report genuine interval lateness"
        );
    }

    #[test]
    fn full_relative_speed_invalidates_legacy_and_neutral_observation_caches() {
        let initial = propagation_observation_key(17, 4_800.25, 0.0, 30.0);
        let identical = propagation_observation_key(17, 4_800.25, 0.0, 30.0);
        let faster_tangency = propagation_observation_key(17, 4_800.25, 0.0, 110.0);
        let mut legacy_cache = None;
        let mut neutral_cache = None;

        for (route, cache) in [
            ("legacy", &mut legacy_cache),
            ("neutral", &mut neutral_cache),
        ] {
            assert!(
                propagation_observation_cache_miss(*cache, initial),
                "{route} cache rejected its first observation"
            );
            *cache = Some(initial);
            assert!(
                !propagation_observation_cache_miss(*cache, identical),
                "{route} cache did not deduplicate an identical observation"
            );
            assert!(
                propagation_observation_cache_miss(*cache, faster_tangency),
                "{route} cache ignored a full-relative-speed-only change"
            );
            *cache = Some(faster_tangency);
            assert!(
                !propagation_observation_cache_miss(*cache, faster_tangency),
                "{route} cache did not deduplicate the replacement observation"
            );
        }
    }

    fn steam_vector_bits(vector: SteamVector3) -> [u32; 3] {
        [vector.x.to_bits(), vector.y.to_bits(), vector.z.to_bits()]
    }

    fn assert_direct_owned_motion_eq(
        actual: &SteamPropagationSnapshot,
        expected: &SteamPropagationSnapshot,
    ) {
        assert_eq!(
            steam_vector_bits(actual.listener_position),
            steam_vector_bits(expected.listener_position)
        );
        assert_eq!(
            steam_vector_bits(actual.listener_linear_velocity_mps),
            steam_vector_bits(expected.listener_linear_velocity_mps)
        );
        let actual = actual.sources[0];
        let expected = expected.sources[0];
        assert_eq!(actual.active, expected.active);
        assert_eq!(
            steam_vector_bits(actual.source_position),
            steam_vector_bits(expected.source_position)
        );
        assert_eq!(
            steam_vector_bits(actual.source_forward),
            steam_vector_bits(expected.source_forward)
        );
        assert_eq!(
            steam_vector_bits(actual.source_up),
            steam_vector_bits(expected.source_up)
        );
        assert_eq!(
            steam_vector_bits(actual.linear_velocity_mps),
            steam_vector_bits(expected.linear_velocity_mps)
        );
        assert_eq!(actual.width.descriptor, expected.width.descriptor);
        assert_eq!(
            actual.width.geometric_k.to_bits(),
            expected.width.geometric_k.to_bits()
        );
        assert_eq!(
            actual.width.phi_eff_radians.to_bits(),
            expected.width.phi_eff_radians.to_bits()
        );
        assert_eq!(
            actual.width.declared_latency_samples,
            expected.width.declared_latency_samples
        );
        assert_eq!(
            actual.width.renderer_revision,
            expected.width.renderer_revision
        );
    }

    fn direct_epoch_fast_motion_snapshot(
        mut snapshot: SteamPropagationSnapshot,
        sequence: u64,
    ) -> SteamPropagationSnapshot {
        snapshot.direct_sequence = 17;
        snapshot.sequence = sequence;
        snapshot.simulated_at_ns = sequence;
        snapshot.listener_position = SteamVector3::default();
        snapshot.listener_linear_velocity_mps = SteamVector3::default();
        snapshot.sources[0].active = true;
        snapshot.sources[0].source_position = SteamVector3::new(1.0, 0.0, 0.0);
        snapshot.sources[0].linear_velocity_mps = SteamVector3::new(0.0, 0.0, 110.0);
        snapshot
    }

    #[test]
    fn a_path_silenced_before_it_ever_resolved_renders_finite() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let parked = ApiEnuVector3::new(10.0, 0.0, 1.5);
        let descriptor = crate::MultiSourceDescriptor::at(parked).with_reflection_send(false);
        let (mut simulation, mut render) = build_multi_source_generation(
            &ballistic_free_field_mesh(),
            None,
            audio,
            test_config(),
            &[descriptor],
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let listener = ApiEnuVector3::new(0.0, 0.0, 1.5);
        simulation.update_inputs(&one_source_update(false, parked, listener));
        simulation.run_direct().unwrap();
        // Activation elsewhere, as an armed crack companion slot, silences
        // the path gate before any path has resolved.
        let armed = ApiEnuVector3::new(40.0, 30.0, 1.5);
        simulation.update_inputs(&one_source_update(true, armed, listener));
        simulation.run_direct().unwrap();
        let path_eq = simulation.snapshot.sources[0].path_eq;
        assert!(simulation.snapshot.sources[0].active);
        drop(simulation);
        Arc::get_mut(&mut render.world).unwrap().has_baked_pathing = true;
        let mut rendered = Vec::new();
        for _ in 0..4 {
            let (left, right) = render_one_source_block(&mut render, &[0.25; 128]);
            rendered.extend(left.into_iter().chain(right));
        }
        assert!(
            rendered.iter().all(|sample| sample.is_finite()),
            "{path_eq:?}"
        );
    }

    #[test]
    fn optional_passes_retain_direct_owned_motion_until_the_next_direct_publication() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_a = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let descriptor = crate::MultiSourceDescriptor::at(source_a)
            .with_extent(ExtentDescriptor::StereoImage { width_m: 2.0 })
            .with_reflection_send(false);
        let (mut simulation, _render) = build_neutral_multi_source_generation(
            &SceneMesh::controlled_s3_corner(),
            None,
            audio,
            test_config(),
            &[descriptor],
            &[2],
            0,
            91,
            QualityTier::Desktop,
        )
        .unwrap();

        let mut update_a = one_source_update(true, source_a, ApiEnuVector3::new(-1.0, 0.5, 1.25));
        update_a.listener.linear_velocity_mps = ApiEnuVector3::new(1.0, 2.0, 3.0);
        update_a.sources[0].linear_velocity_mps = ApiEnuVector3::new(4.0, 5.0, 6.0);
        simulation.update_inputs(&update_a);
        simulation.run_direct().unwrap();
        let direct_a = simulation.snapshot;
        assert_eq!(direct_a.direct_sequence, 1);

        let path_sentinel = [0.25, 0.5, 0.75];
        let path_sh_sentinel = std::array::from_fn(|index| 0.03125 * (index as f32 + 1.0));
        let reflection_sentinel = SteamReflectionParams {
            ir: 37,
            reverb_times: [0.5, 0.75, 1.0],
            eq: [0.25, 0.5, 0.75],
            delay: 11,
            num_channels: 4,
            ir_size: 128,
            tan_slot: 3,
        };
        simulation.snapshot.sources[0].path_eq = path_sentinel;
        simulation.snapshot.sources[0].path_sh = path_sh_sentinel;
        simulation.snapshot.sources[0].reflections = reflection_sentinel;

        let source_b = ApiEnuVector3::new(24.0, -8.0, 4.0);
        let mut update_b = one_source_update(false, source_b, ApiEnuVector3::new(12.0, 16.0, 5.0));
        update_b.listener.pose.forward = ApiEnuVector3::new(1.0, 0.0, 0.0);
        update_b.listener.linear_velocity_mps = ApiEnuVector3::new(-7.0, 8.0, 9.0);
        update_b.sources[0].pose.forward = ApiEnuVector3::new(-1.0, 0.0, 0.0);
        update_b.sources[0].linear_velocity_mps = ApiEnuVector3::new(10.0, -11.0, 12.0);
        simulation.update_inputs(&update_b);

        // Activation and endpoint discontinuities publish path invalidation
        // immediately, before any blocking SDK pass. That publication is also
        // non-direct and therefore retains the accepted A motion.
        assert_eq!(simulation.snapshot.sequence, direct_a.sequence + 1);
        assert_eq!(
            simulation.snapshot.direct_sequence,
            direct_a.direct_sequence
        );
        assert_direct_owned_motion_eq(&simulation.snapshot, &direct_a);
        assert_eq!(simulation.snapshot.sources[0].path_eq, path_sentinel);
        assert_eq!(
            simulation.snapshot.sources[0].path_sh,
            [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS]
        );
        assert_eq!(
            simulation.snapshot.sources[0].reflections,
            reflection_sentinel
        );

        simulation.snapshot.sources[0].path_eq = path_sentinel;
        simulation.snapshot.sources[0].path_sh = path_sh_sentinel;
        let path_sequence = simulation.snapshot.sequence;
        simulation.run_pathing().unwrap();
        assert_eq!(simulation.snapshot.sequence, path_sequence + 1);
        assert_eq!(
            simulation.snapshot.direct_sequence,
            direct_a.direct_sequence
        );
        assert_direct_owned_motion_eq(&simulation.snapshot, &direct_a);
        assert_eq!(simulation.snapshot.sources[0].path_eq, path_sentinel);
        assert_eq!(
            simulation.snapshot.sources[0].path_sh,
            [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS]
        );
        assert_eq!(
            simulation.snapshot.sources[0].reflections,
            reflection_sentinel
        );

        let reflection_sequence = simulation.snapshot.sequence;
        simulation.run_reflections_for_realtime_prepare().unwrap();
        assert_eq!(simulation.snapshot.sequence, reflection_sequence + 1);
        assert_eq!(
            simulation.snapshot.direct_sequence,
            direct_a.direct_sequence
        );
        assert_direct_owned_motion_eq(&simulation.snapshot, &direct_a);
        assert_eq!(
            simulation.snapshot.sources[0].reflections,
            SteamReflectionParams::default()
        );

        let before_direct_b = simulation.snapshot;
        simulation.run_direct().unwrap();
        assert_eq!(simulation.snapshot.sequence, before_direct_b.sequence + 1);
        assert_eq!(
            simulation.snapshot.direct_sequence,
            direct_a.direct_sequence + 1
        );
        assert_ne!(
            steam_vector_bits(simulation.snapshot.listener_position),
            steam_vector_bits(direct_a.listener_position)
        );
        assert_eq!(
            steam_vector_bits(simulation.snapshot.listener_position),
            steam_vector_bits(simulation.frame.listener.position)
        );
        assert_eq!(
            steam_vector_bits(simulation.snapshot.listener_linear_velocity_mps),
            steam_vector_bits(simulation.frame.listener_linear_velocity_mps)
        );
        assert!(!simulation.snapshot.sources[0].active);
        assert_eq!(
            steam_vector_bits(simulation.snapshot.sources[0].source_position),
            steam_vector_bits(simulation.frame.sources[0].position)
        );
        assert_eq!(
            steam_vector_bits(simulation.snapshot.sources[0].source_forward),
            steam_vector_bits(simulation.frame.sources[0].forward)
        );
        assert_eq!(
            steam_vector_bits(simulation.snapshot.sources[0].source_up),
            steam_vector_bits(simulation.frame.sources[0].up)
        );
        assert_eq!(
            steam_vector_bits(simulation.snapshot.sources[0].linear_velocity_mps),
            steam_vector_bits(simulation.frame.source_linear_velocities_mps[0])
        );
        assert_eq!(
            simulation.snapshot.sources[0].width,
            width_snapshot(
                simulation.source_extents[0],
                simulation.frame.sources[0],
                simulation.frame.listener.position,
            )
        );
    }

    #[test]
    fn legacy_same_direct_optional_publication_schedule_is_bit_exact() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptor = crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0))
            .with_reflection_send(false);
        let mesh = ballistic_free_field_mesh();
        let (mut simulation_a, mut render_a) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            101,
            QualityTier::Desktop,
        )
        .unwrap();
        let (mut simulation_b, mut render_b) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            101,
            QualityTier::Desktop,
        )
        .unwrap();
        direct_only(&mut render_a);
        direct_only(&mut render_b);

        let direct_a = direct_epoch_fast_motion_snapshot(simulation_a.snapshot, 41);
        let mut optional_a = direct_a;
        optional_a.sequence = 42;
        optional_a.simulated_at_ns = 42;
        optional_a.sources[0].path_eq = [0.25, 0.5, 0.75];
        let direct_b = direct_epoch_fast_motion_snapshot(simulation_b.snapshot, 41);
        let mut optional_b = direct_b;
        optional_b.sequence = 42;
        optional_b.simulated_at_ns = 42;
        optional_b.sources[0].path_eq = [0.25, 0.5, 0.75];

        simulation_a.publication.publish(direct_a);
        simulation_b.publication.publish(direct_b);
        simulation_b.publication.publish(optional_b);
        let mut global_frame = 0_usize;
        for block in 0..4 {
            if block == 1 {
                simulation_a.publication.publish(optional_a);
            }
            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = ((global_frame % 29) as f32 - 14.0) / 29.0;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            render_one_source_block(&mut render_a, &input);
            render_one_source_block(&mut render_b, &input);

            assert_eq!(
                render_a
                    .mono_work
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                render_b
                    .mono_work
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                "same-direct optional publication changed delayed legacy PCM in block {block}"
            );
            assert_eq!(
                render_a.sources[0]
                    .propagation_delay
                    .current_delay_samples()
                    .to_bits(),
                render_b.sources[0]
                    .propagation_delay
                    .current_delay_samples()
                    .to_bits(),
                "same-direct optional publication changed the legacy read head in block {block}"
            );
        }
        assert_eq!(
            render_a.sources[0].last_propagation_observation,
            render_b.sources[0].last_propagation_observation
        );
        assert_eq!(
            render_a.sources[0]
                .last_propagation_observation
                .expect("legacy direct observation")
                .0,
            direct_a.direct_sequence
        );
    }

    #[test]
    fn neutral_same_direct_optional_publication_schedule_is_bit_exact() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptor = crate::MultiSourceDescriptor::at(ApiEnuVector3::new(1.0, 0.0, 0.0))
            .with_reflection_send(false);
        let mesh = ballistic_free_field_mesh();
        let (mut simulation_a, mut render_a) = build_neutral_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            &[1],
            0,
            102,
            QualityTier::Desktop,
        )
        .unwrap();
        let (mut simulation_b, mut render_b) = build_neutral_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            &[1],
            0,
            102,
            QualityTier::Desktop,
        )
        .unwrap();
        render_a.prepare_for_realtime().unwrap();
        render_b.prepare_for_realtime().unwrap();

        let direct_a = direct_epoch_fast_motion_snapshot(simulation_a.snapshot, 51);
        let mut optional_a = direct_a;
        optional_a.sequence = 52;
        optional_a.simulated_at_ns = 52;
        optional_a.sources[0].path_eq = [0.25, 0.5, 0.75];
        let direct_b = direct_epoch_fast_motion_snapshot(simulation_b.snapshot, 51);
        let mut optional_b = direct_b;
        optional_b.sequence = 52;
        optional_b.simulated_at_ns = 52;
        optional_b.sources[0].path_eq = [0.25, 0.5, 0.75];

        simulation_a.publication.publish(direct_a);
        simulation_b.publication.publish(direct_b);
        simulation_b.publication.publish(optional_b);
        let mut global_frame = 0_usize;
        for block in 0..4 {
            if block == 1 {
                simulation_a.publication.publish(optional_a);
            }
            let program = (0..audio.frame_size)
                .map(|_| {
                    let sample = ((global_frame % 29) as f32 - 14.0) / 29.0;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            let sources = [SpatialBackendSourceBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&program, &[]],
            }];
            let mut presentation_a =
                vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * audio.frame_size as usize];
            let mut environment_a =
                vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * audio.frame_size as usize];
            let mut metadata_a = SpatialOutputMetadata::default();
            let mut presentation_b =
                vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * audio.frame_size as usize];
            let mut environment_b =
                vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * audio.frame_size as usize];
            let mut metadata_b = SpatialOutputMetadata::default();
            render_a
                .render_spatial_block(SpatialPropagationRenderBlock {
                    block_start_frame: block as u64 * audio.frame_size as u64,
                    propagation_sequence: direct_a.direct_sequence,
                    sources: &sources,
                    presentation_bank: &mut presentation_a,
                    environmental_bank: &mut environment_a,
                    metadata: &mut metadata_a,
                })
                .unwrap();
            render_b
                .render_spatial_block(SpatialPropagationRenderBlock {
                    block_start_frame: block as u64 * audio.frame_size as u64,
                    propagation_sequence: direct_b.direct_sequence,
                    sources: &sources,
                    presentation_bank: &mut presentation_b,
                    environmental_bank: &mut environment_b,
                    metadata: &mut metadata_b,
                })
                .unwrap();

            assert_eq!(metadata_a.validity, SpatialOutputValidity::Valid);
            assert_eq!(metadata_b.validity, SpatialOutputValidity::Valid);
            assert_eq!(
                render_a.delayed_program_for_source(0).unwrap()[0]
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                render_b.delayed_program_for_source(0).unwrap()[0]
                    .iter()
                    .map(|sample| sample.to_bits())
                    .collect::<Vec<_>>(),
                "same-direct optional publication changed delayed neutral PCM in block {block}"
            );
            let delay_bits = |render: &NeutralMultiSourceRenderGraph| match &render.program_delays
                [0]
            {
                NeutralProgramDelay::Mono(delay) => delay.delay.current_delay_samples().to_bits(),
                NeutralProgramDelay::Stereo(_) => unreachable!("test constructs mono program"),
            };
            assert_eq!(
                delay_bits(&render_a),
                delay_bits(&render_b),
                "same-direct optional publication changed the neutral read head in block {block}"
            );
        }
        assert_eq!(
            render_a.sources[0].last_propagation_observation,
            render_b.sources[0].last_propagation_observation
        );
        assert_eq!(
            render_a.sources[0]
                .last_propagation_observation
                .expect("neutral direct observation")
                .0,
            direct_a.direct_sequence
        );
    }

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

    fn signed_ballistic_plan(listener: ApiEnuVector3) -> BallisticShotPlan {
        plan_ballistic_shot(
            BallisticShot {
                muzzle_position_enu: ApiEnuVector3::default(),
                direction_enu: ApiEnuVector3::new(0.0, 1.0, 0.0),
                mach: 2.5,
                levels: BallisticEventLevels {
                    blast_spl_at_one_meter_db: 155.0,
                    crack_over_blast_db_at_reference: 3.0,
                },
            },
            listener,
        )
        .unwrap()
    }

    fn ballistic_descriptors(plan: BallisticShotPlan) -> [crate::MultiSourceDescriptor; 2] {
        let crack = plan.crack.unwrap_or(plan.blast);
        [
            crate::MultiSourceDescriptor::at(crack.position_enu)
                .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter {
                    db_spl: crack.spl_at_one_meter_db as f32,
                })
                .with_initially_active(false)
                .with_source_priority(crate::SourcePriorityClass::TransientEvent)
                .with_reflection_send(false),
            crate::MultiSourceDescriptor::at(plan.blast.position_enu)
                .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter {
                    db_spl: plan.blast.spl_at_one_meter_db as f32,
                })
                .with_impulse_class(fightbox_api::ImpulseClass::ArtilleryThunder)
                .with_initially_active(false)
                .with_source_priority(crate::SourcePriorityClass::TransientEvent),
        ]
    }

    fn ballistic_update(plan: BallisticShotPlan, listener: ApiEnuVector3) -> SimulationUpdate {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        if let Some(crack) = plan.crack {
            sources[0] = SourceMotion {
                active: true,
                pose: default_api_pose(crack.position_enu),
                linear_velocity_mps: ApiEnuVector3::default(),
            };
        }
        sources[1] = SourceMotion {
            active: true,
            pose: default_api_pose(plan.blast.position_enu),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(listener),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        }
    }

    fn render_ballistic_program(
        render: &mut MultiSourceRenderGraph,
        crack: &[f32],
        blast: &[f32],
    ) -> Vec<f32> {
        assert_eq!(crack.len(), blast.len());
        assert_eq!(crack.len() % render.audio.frame_size as usize, 0);
        let mut interleaved = Vec::with_capacity(crack.len() * 2);
        for (crack_block, blast_block) in crack
            .chunks_exact(render.audio.frame_size as usize)
            .zip(blast.chunks_exact(render.audio.frame_size as usize))
        {
            let sources = [
                BackendSourceBlock {
                    source_index: 0,
                    input_mono: crack_block,
                },
                BackendSourceBlock {
                    source_index: 1,
                    input_mono: blast_block,
                },
            ];
            let mut left = vec![0.0; render.audio.frame_size as usize];
            let mut right = vec![0.0; render.audio.frame_size as usize];
            render
                .render_block(PropagationRenderBlock {
                    listener_orientation: ListenerOrientation {
                        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                    },
                    sources: &sources,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            interleaved.extend(
                left.into_iter()
                    .zip(right)
                    .flat_map(|(left, right)| [left, right]),
            );
        }
        interleaved
    }

    fn isolated_onset_near(stereo: &[f32], expected_s: f64, sample_rate_hz: usize) -> usize {
        let expected = (expected_s * sample_rate_hz as f64).round() as usize;
        let probe_radius = sample_rate_hz / 100;
        let start = expected.saturating_sub(probe_radius);
        let end = (expected + probe_radius + 1).min(stereo.len() / 2);
        let peak = (start..end)
            .map(|frame| stereo[frame * 2].abs().max(stereo[frame * 2 + 1].abs()))
            .fold(0.0_f32, f32::max);
        assert!(
            peak > 0.0,
            "isolated onset probe is silent near {expected_s}s"
        );
        let threshold = peak * 1.0e-3;
        let onset = (start..end)
            .find(|frame| stereo[*frame * 2].abs().max(stereo[*frame * 2 + 1].abs()) >= threshold)
            .expect("isolated onset near ballistic oracle");
        eprintln!(
            "ballistic_onset_probe expected_frame={expected} start={start} end={end} peak={peak:.9e} threshold={threshold:.9e} start_amp={:.9e} onset={onset}",
            stereo[start * 2].abs().max(stereo[start * 2 + 1].abs()),
        );
        onset
    }

    fn direct_only(render: &mut MultiSourceRenderGraph) {
        render
            .take_stage_output_gain_writer()
            .unwrap()
            .publish(StageOutputGains {
                direct: 1.0,
                pathing: 0.0,
                reflections: 0.0,
            });
    }

    fn path_terms(seed: f32) -> ([f32; 3], [f32; crate::backend_snapshot::MAX_PATH_SH_COEFFS]) {
        (
            [seed, seed + 0.125, seed + 0.25],
            std::array::from_fn(|index| seed + index as f32 * 0.03125),
        )
    }

    #[test]
    fn path_gate_holds_two_misses_trips_on_third_and_resolve_resets() {
        let mut gate = PathGateState::default();
        let mut propagation = SteamSourcePropagation::default();
        let (initial_eq, initial_sh) = path_terms(0.25);
        gate.resolve(&mut propagation, initial_eq, initial_sh);

        for expected_misses in 1..PATH_GATE_MISS_THRESHOLD {
            assert!(!gate.miss(&mut propagation));
            assert_eq!(gate.consecutive_misses, expected_misses);
            assert_eq!(
                propagation.path_eq.map(f32::to_bits),
                initial_eq.map(f32::to_bits)
            );
            assert_eq!(
                propagation.path_sh.map(f32::to_bits),
                initial_sh.map(f32::to_bits)
            );
        }

        assert!(gate.miss(&mut propagation));
        assert_eq!(gate.consecutive_misses, PATH_GATE_MISS_THRESHOLD);
        assert_eq!(propagation.path_eq, initial_eq);
        assert_eq!(
            propagation.path_sh,
            [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS]
        );

        let (recovered_eq, recovered_sh) = path_terms(0.75);
        gate.resolve(&mut propagation, recovered_eq, recovered_sh);
        assert_eq!(gate.consecutive_misses, 0);
        assert_eq!(
            propagation.path_eq.map(f32::to_bits),
            recovered_eq.map(f32::to_bits)
        );
        assert_eq!(
            propagation.path_sh.map(f32::to_bits),
            recovered_sh.map(f32::to_bits)
        );
        assert!(!gate.miss(&mut propagation));
        assert_eq!(gate.consecutive_misses, 1);
        assert_eq!(
            propagation.path_sh.map(f32::to_bits),
            recovered_sh.map(f32::to_bits)
        );
        gate.invalidate(&mut propagation);
        assert_eq!(gate.consecutive_misses, 0);
        assert_eq!(propagation.path_eq, recovered_eq);
        assert_eq!(
            propagation.path_sh,
            [0.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS]
        );
        eprintln!(
            "path_gate miss_1=hold miss_2=hold miss_3=fade resolve_reset_misses={}",
            gate.consecutive_misses
        );
    }

    #[test]
    fn ballistic_pair_is_atomic_timed_prioritized_and_reuses_its_slots() {
        let listener = ApiEnuVector3::new(0.0, 60.0, 30.0);
        let plan = signed_ballistic_plan(listener);
        let descriptors = ballistic_descriptors(plan);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let mesh = ballistic_free_field_mesh();
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
        assert_eq!(simulation.source_count(), 2);
        assert!(!simulation.snapshot.sources[0].active);
        assert!(!simulation.snapshot.sources[1].active);
        assert!(!render.sources[0].reflection_send_enabled);
        assert!(render.sources[1].reflection_send_enabled);

        let update = ballistic_update(plan, listener);
        simulation.update_inputs(&update);
        // Activation invalidates paths but republishes the old inactive frame;
        // there is no default-occlusion audible snapshot between trigger and
        // the direct tick.
        assert!(!simulation.snapshot.sources[0].active);
        assert!(!simulation.snapshot.sources[1].active);
        simulation.run_direct().unwrap();
        let first_activation_sequence = simulation.snapshot.sequence;
        assert!(simulation.snapshot.sources[0].active);
        assert!(simulation.snapshot.sources[1].active);
        let telemetry = simulation.quality_governor_telemetry();
        for source in &telemetry.sources[..2] {
            assert_eq!(
                source.priority_class,
                crate::SourcePriorityClass::TransientEvent
            );
            assert!(source.transient_protection_remaining_blocks > 0);
        }

        direct_only(&mut render);
        // The signed strip measures the later blast on an isolated component
        // probe so the crack's long, low-level HRTF/filter residue cannot be
        // mistaken for a second onset. Keep an equivalent triggered pair with
        // a silent crack stem for that measurement.
        let (mut blast_probe_simulation, mut blast_probe_render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &descriptors,
            2,
            QualityTier::Desktop,
        )
        .unwrap();
        blast_probe_simulation.update_inputs(&update);
        blast_probe_simulation.run_direct().unwrap();
        direct_only(&mut blast_probe_render);
        let program_frames = 48_000;
        let (crack, _) = synthesize_crack_stem(&plan, 48_000, program_frames).unwrap();
        let mut blast = vec![0.0; program_frames];
        blast[0] = 1.0;
        let silent_crack = vec![0.0; program_frames];
        let first_blast_probe =
            render_ballistic_program(&mut blast_probe_render, &silent_crack, &blast);
        let first = render_ballistic_program(&mut render, &crack, &blast);
        assert_eq!(
            render.sources[0].quality_gains[2].to_bits(),
            0.0_f32.to_bits()
        );
        let crack_frame = isolated_onset_near(
            &first,
            plan.crack.unwrap().arrival_time_s,
            audio.sample_rate_hz as usize,
        );
        let blast_frame = isolated_onset_near(
            &first_blast_probe,
            plan.blast.arrival_time_s,
            audio.sample_rate_hz as usize,
        );
        let crack_measured_ms = crack_frame as f64 * 1_000.0 / 48_000.0;
        let blast_measured_ms = blast_frame as f64 * 1_000.0 / 48_000.0;
        let crack_delta_ms = crack_measured_ms - plan.crack.unwrap().arrival_time_s * 1_000.0;
        let blast_delta_ms = blast_measured_ms - plan.blast.arrival_time_s * 1_000.0;
        assert!(
            crack_delta_ms.abs() <= 2.0,
            "crack delta {crack_delta_ms:+.4} ms"
        );
        assert!(
            blast_delta_ms.abs() <= 2.0,
            "blast delta {blast_delta_ms:+.4} ms"
        );

        // Retire and reactivate the same two stable indices. The inactive
        // direct publication resets retained delay history; no source or SDK
        // object is created for the second shot.
        let mut inactive = update;
        inactive.sources[0].active = false;
        inactive.sources[1].active = false;
        simulation.update_inputs(&inactive);
        simulation.run_direct().unwrap();
        blast_probe_simulation.update_inputs(&inactive);
        blast_probe_simulation.run_direct().unwrap();
        let zeros = vec![0.0; 128];
        render_ballistic_program(&mut render, &zeros, &zeros);
        render_ballistic_program(&mut blast_probe_render, &zeros, &zeros);
        simulation.update_inputs(&update);
        simulation.run_direct().unwrap();
        blast_probe_simulation.update_inputs(&update);
        blast_probe_simulation.run_direct().unwrap();
        let second_activation_sequence = simulation.snapshot.sequence;
        assert!(second_activation_sequence > first_activation_sequence);
        assert_eq!(simulation.source_count(), 2);
        let second_blast_probe =
            render_ballistic_program(&mut blast_probe_render, &silent_crack, &blast);
        let second = render_ballistic_program(&mut render, &crack, &blast);
        let second_crack = isolated_onset_near(
            &second,
            plan.crack.unwrap().arrival_time_s,
            audio.sample_rate_hz as usize,
        );
        let second_blast = isolated_onset_near(
            &second_blast_probe,
            plan.blast.arrival_time_s,
            audio.sample_rate_hz as usize,
        );
        let second_crack_delta_ms =
            second_crack as f64 * 1_000.0 / 48_000.0 - plan.crack.unwrap().arrival_time_s * 1_000.0;
        let second_blast_delta_ms =
            second_blast as f64 * 1_000.0 / 48_000.0 - plan.blast.arrival_time_s * 1_000.0;
        assert!(second_crack_delta_ms.abs() <= 2.0);
        assert!(second_blast_delta_ms.abs() <= 2.0);
        eprintln!(
            "ballistic_timing crack={crack_measured_ms:.4}ms delta={crack_delta_ms:+.4}ms blast={blast_measured_ms:.4}ms delta={blast_delta_ms:+.4}ms second_crack_delta={second_crack_delta_ms:+.4}ms second_blast_delta={second_blast_delta_ms:+.4}ms activation_sequences={first_activation_sequence}/{second_activation_sequence} slots={}",
            simulation.source_count(),
        );
    }

    #[test]
    fn ballistic_out_of_cone_is_blast_only_with_ballistic_timing() {
        let listener = ApiEnuVector3::new(0.0, -30.0, 30.0);
        let plan = signed_ballistic_plan(listener);
        assert!(plan.crack.is_none());
        let descriptors = ballistic_descriptors(plan);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let mesh = ballistic_free_field_mesh();
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
        simulation.update_inputs(&ballistic_update(plan, listener));
        simulation.run_direct().unwrap();
        assert!(!simulation.snapshot.sources[0].active);
        assert!(simulation.snapshot.sources[1].active);
        direct_only(&mut render);
        let crack = vec![0.0; 48_000];
        let mut blast = vec![0.0; 48_000];
        blast[0] = 1.0;
        let output = render_ballistic_program(&mut render, &crack, &blast);
        let onset = isolated_onset_near(
            &output,
            plan.blast.arrival_time_s,
            audio.sample_rate_hz as usize,
        );
        let measured_ms = onset as f64 * 1_000.0 / 48_000.0;
        let delta_ms = measured_ms - plan.blast.arrival_time_s * 1_000.0;
        assert!(delta_ms.abs() <= 2.0, "blast-only delta {delta_ms:+.4} ms");
        eprintln!(
            "ballistic_out_of_cone blast={measured_ms:.4}ms delta={delta_ms:+.4}ms crack_active=false"
        );
    }

    #[test]
    fn ballistic_cold_start_occlusion_has_no_unoccluded_onset_block() {
        let mesh = wall_mesh(AcousticMaterial::MASONRY);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source = ApiEnuVector3::new(0.0, -1.0, 1.5);
        let listener = ApiEnuVector3::new(0.0, 1.0, 1.5);
        let descriptors = [crate::MultiSourceDescriptor::at(source)
            .with_initially_active(false)
            .with_source_priority(crate::SourcePriorityClass::TransientEvent)
            .with_reflection_send(false)];
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
        direct_only(&mut render);
        let update = one_source_update(true, source, listener);
        simulation.update_inputs(&update);
        let mut onset_block = vec![0.0; 128];
        onset_block[0] = 1.0;
        let (pre_tick_left, pre_tick_right) = render_one_source_block(&mut render, &onset_block);
        assert!(
            pre_tick_left
                .iter()
                .chain(&pre_tick_right)
                .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
        );

        simulation.run_direct().unwrap();
        assert!(simulation.snapshot.sources[0].active);
        assert!(simulation.snapshot.sources[0].direct.occlusion <= 1.0e-4);
        let mut stem = vec![0.0; 24_064];
        stem[960] = 1.0; // 20 ms direct-simulation window before emission.
        let mut peak = 0.0_f32;
        for block in stem.chunks_exact(128) {
            let (left, right) = render_one_source_block(&mut render, block);
            peak = left
                .into_iter()
                .chain(right)
                .map(f32::abs)
                .fold(peak, f32::max);
        }
        assert!(
            peak <= 1.0e-8,
            "occluded cold-start onset leaked at {peak:.9e}"
        );
        eprintln!(
            "ballistic_cold_start occlusion={:.9} onset_peak={peak:.9e}",
            simulation.snapshot.sources[0].direct.occlusion,
        );
    }

    #[test]
    fn probe_valid_path_gate_is_bit_identical_to_direct_publication() {
        let (path_eq, path_sh) = path_terms(0.375);
        let mut expected = SteamSourcePropagation::default();
        expected.path_eq = path_eq;
        expected.path_sh = path_sh;
        let mut gated = SteamSourcePropagation::default();
        let mut gate = PathGateState::default();

        for _ in 0..8 {
            gate.resolve(&mut gated, path_eq, path_sh);
            assert_eq!(gate.consecutive_misses, 0);
            assert_eq!(
                gated.path_eq.map(f32::to_bits),
                expected.path_eq.map(f32::to_bits)
            );
            assert_eq!(
                gated.path_sh.map(f32::to_bits),
                expected.path_sh.map(f32::to_bits)
            );
        }
    }

    #[test]
    fn probe_valid_path_gate_render_is_bit_identical_to_the_pre_gate_assignment() {
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
        let source = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let listener = ApiEnuVector3::new(4.0, 6.0, 1.5);
        let descriptors = [crate::MultiSourceDescriptor::at(source)];
        let (mut gated_simulation, mut gated_render) =
            build_multi_source_session(&mesh, &baked, audio, config, &descriptors).unwrap();
        let (mut baseline_simulation, mut baseline_render) =
            build_multi_source_session(&mesh, &baked, audio, config, &descriptors).unwrap();
        let update = one_source_update(true, source, listener);
        gated_simulation.update_inputs(&update);
        baseline_simulation.update_inputs(&update);
        gated_simulation.run_direct().unwrap();
        baseline_simulation.run_direct().unwrap();
        gated_simulation.run_pathing().unwrap();

        assert!(
            gated_simulation
                .world
                .has_influencing_probe(gated_simulation.frame.listener.position)
        );
        assert!(
            gated_simulation
                .world
                .has_influencing_probe(gated_simulation.frame.sources[0].position)
        );
        assert_eq!(gated_simulation.path_gates[0].consecutive_misses, 0);
        let valid_path = gated_simulation.snapshot.sources[0];
        assert!(
            valid_path
                .path_sh
                .iter()
                .any(|coefficient| *coefficient != 0.0)
        );

        // This is the old happy-path operation: copy the already validated
        // SDK terms directly into the otherwise equivalent published frame.
        let mut baseline_snapshot = baseline_simulation.snapshot;
        baseline_snapshot.sequence = baseline_snapshot.sequence.wrapping_add(1);
        baseline_snapshot.sources[0].path_eq = valid_path.path_eq;
        baseline_snapshot.sources[0].path_sh = valid_path.path_sh;
        baseline_snapshot.sources[0].configured_pathing_order = valid_path.configured_pathing_order;
        baseline_simulation.snapshot = baseline_snapshot;
        baseline_simulation.publication.publish(baseline_snapshot);

        let path_only = StageOutputGains {
            direct: 0.0,
            pathing: 1.0,
            reflections: 0.0,
        };
        gated_render
            .take_stage_output_gain_writer()
            .unwrap()
            .publish(path_only);
        baseline_render
            .take_stage_output_gain_writer()
            .unwrap()
            .publish(path_only);
        let mut gated_samples = Vec::new();
        let mut baseline_samples = Vec::new();
        let mut global_frame = 0_usize;
        for _ in 0..16 {
            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = (TAU * 617.0 * global_frame as f32 / audio.sample_rate_hz as f32)
                        .sin()
                        * 0.1;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            let (gated_left, gated_right) = render_one_source_block(&mut gated_render, &input);
            let (baseline_left, baseline_right) =
                render_one_source_block(&mut baseline_render, &input);
            gated_samples.extend(
                gated_left
                    .into_iter()
                    .zip(gated_right)
                    .flat_map(|(left, right)| [left, right]),
            );
            baseline_samples.extend(
                baseline_left
                    .into_iter()
                    .zip(baseline_right)
                    .flat_map(|(left, right)| [left, right]),
            );
        }
        assert_samples_bit_equal(
            &gated_samples,
            &baseline_samples,
            "probe-valid hysteresis path changed pre-gate PCM",
        );
        assert!(gated_samples.iter().any(|sample| sample.abs() > 1.0e-8));
    }

    #[test]
    fn path_gate_trip_and_recovery_reuse_the_bounded_propagation_slew() {
        let block_seconds = 128.0 / 48_000.0;
        let retention = (-block_seconds / PROPAGATION_SLEW_TIME_SECONDS).exp();
        let maximum_fractional_step = 1.0 - retention;
        let listener = SteamVector3::default();
        let mut gate = PathGateState::default();
        let mut propagation = SteamSourcePropagation::default();
        let path_eq = [1.0; 3];
        let path_sh = [1.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS];
        gate.resolve(&mut propagation, path_eq, path_sh);
        let mut smoother = SourcePropagationSmoother::default();
        let initial = smoother
            .advance(propagation, listener, 0.0, retention)
            .endpoint();

        assert!(!gate.miss(&mut propagation));
        assert!(!gate.miss(&mut propagation));
        assert!(gate.miss(&mut propagation));
        let faded = smoother
            .advance(propagation, listener, 0.0, retention)
            .endpoint();
        let trip_step = initial.path_sh[0] - faded.path_sh[0];
        assert!(faded.path_sh[0] > 0.0 && faded.path_sh[0] < initial.path_sh[0]);
        assert!(trip_step <= maximum_fractional_step + f32::EPSILON);

        for _ in 0..119 {
            smoother.advance(propagation, listener, 0.0, retention);
        }
        let before_recovery = smoother.applied();
        gate.resolve(&mut propagation, path_eq, path_sh);
        let recovered = smoother
            .advance(propagation, listener, 0.0, retention)
            .endpoint();
        let recovery_step = recovered.path_sh[0] - before_recovery.path_sh[0];
        assert!(recovery_step > 0.0);
        assert!(recovery_step <= maximum_fractional_step + f32::EPSILON);
        eprintln!(
            "path_gate_slew retention={retention:.9} max_fractional_step={maximum_fractional_step:.9} trip_step={trip_step:.9} recovery_step={recovery_step:.9}"
        );
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

    fn ballistic_free_field_mesh() -> SceneMesh {
        SceneMesh {
            vertices_enu_m: vec![
                EnuVector3::new(-200.0, -200.0, -100.0),
                EnuVector3::new(200.0, -200.0, -100.0),
                EnuVector3::new(200.0, 200.0, -100.0),
                EnuVector3::new(-200.0, 200.0, -100.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
            material_indices: vec![0; 4],
            materials: vec![AcousticMaterial::GROUND],
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
        extent: Option<ExtentDescriptor>,
    ) -> (Vec<f32>, SteamDirectParams) {
        directivity_capture_with_impulse(directivity, source_forward, extent, None)
    }

    fn directivity_capture_with_impulse(
        directivity: Option<Directivity>,
        source_forward: ApiEnuVector3,
        extent: Option<ExtentDescriptor>,
        impulse_class: Option<fightbox_api::ImpulseClass>,
    ) -> (Vec<f32>, SteamDirectParams) {
        directivity_capture_with_profile(directivity, source_forward, extent, impulse_class, None)
    }

    fn directivity_capture_with_profile(
        directivity: Option<Directivity>,
        source_forward: ApiEnuVector3,
        extent: Option<ExtentDescriptor>,
        impulse_class: Option<fightbox_api::ImpulseClass>,
        echo_profile: Option<EchoProfile>,
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
        if let Some(extent) = extent {
            descriptor = descriptor.with_extent(extent);
        }
        if let Some(impulse_class) = impulse_class {
            descriptor = descriptor.with_impulse_class(impulse_class);
        }
        if let Some(echo_profile) = echo_profile {
            descriptor = descriptor.with_echo_profile(echo_profile);
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

    struct ImpulseTestCapture {
        interleaved: Vec<f32>,
        delayed_mono: Vec<f32>,
        width_presentation: Option<Vec<f32>>,
        source_count: usize,
        has_impulse_shaper: bool,
        has_width: bool,
        width_declared_latency_samples: u32,
    }

    fn impulse_test_floor_scene() -> SceneMesh {
        SceneMesh {
            vertices_enu_m: vec![
                EnuVector3::new(-1_024.0, -1_024.0, 0.0),
                EnuVector3::new(1_024.0, -1_024.0, 0.0),
                EnuVector3::new(1_024.0, 1_024.0, 0.0),
                EnuVector3::new(-1_024.0, 1_024.0, 0.0),
            ],
            triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
            material_indices: vec![0; 4],
            materials: vec![AcousticMaterial::GROUND],
        }
    }

    fn impulse_test_capture(
        distance_m: f32,
        extent: ExtentDescriptor,
        impulse_class: Option<fightbox_api::ImpulseClass>,
        input: &[f32],
    ) -> ImpulseTestCapture {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        assert_eq!(input.len() % audio.frame_size as usize, 0);
        let source_position = ApiEnuVector3::new(0.0, distance_m, 1.5);
        let listener_position = ApiEnuVector3::new(0.0, 0.0, 1.5);
        let source_pose = Pose {
            position: source_position,
            forward: ApiEnuVector3::new(1.0, 0.0, 0.0),
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        };
        let mut descriptor = crate::MultiSourceDescriptor::at(source_position)
            .with_initial_pose(source_pose)
            .with_extent(extent);
        if let Some(impulse_class) = impulse_class {
            descriptor = descriptor.with_impulse_class(impulse_class);
        }
        let (mut simulation, mut render) = build_multi_source_generation(
            &impulse_test_floor_scene(),
            None,
            audio,
            test_config(),
            &[descriptor],
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut source_update = one_source_update(true, source_position, listener_position);
        source_update.sources[0].pose = source_pose;
        simulation.update_inputs(&source_update);
        simulation.run_direct().unwrap();
        let mut stage_gains = render.take_stage_output_gain_writer().unwrap();
        stage_gains.publish(StageOutputGains {
            direct: 1.0,
            pathing: 0.0,
            reflections: 0.0,
        });

        let mut interleaved = Vec::with_capacity(input.len() * 2);
        let mut delayed_mono = Vec::with_capacity(input.len());
        let mut width_presentation = render.sources[0]
            .width
            .as_ref()
            .map(|_| Vec::with_capacity(input.len() * 3));
        for block in input.chunks_exact(audio.frame_size as usize) {
            let (left, right) = render_one_source_block(&mut render, block);
            interleaved.extend(
                left.into_iter()
                    .zip(right)
                    .flat_map(|(left, right)| [left, right]),
            );
            delayed_mono.extend_from_slice(&render.mono_work);
            if let (Some(captured), Some(width)) =
                (&mut width_presentation, &mut render.sources[0].width)
            {
                let mut presentation = vec![0.0; audio.frame_size as usize * 3];
                width.presentation.read_interleaved(&mut presentation);
                captured.extend_from_slice(&presentation);
            }
        }

        ImpulseTestCapture {
            interleaved,
            delayed_mono,
            width_presentation,
            source_count: render.sources.len(),
            has_impulse_shaper: render.sources[0].impulse_shaper.is_some(),
            has_width: render.sources[0].width.is_some(),
            width_declared_latency_samples: simulation.snapshot.sources[0]
                .width
                .declared_latency_samples,
        }
    }

    fn padded_to_block(mut samples: Vec<f32>) -> Vec<f32> {
        const BLOCK_FRAMES: usize = 128;
        let padded = samples.len().div_ceil(BLOCK_FRAMES) * BLOCK_FRAMES;
        samples.resize(padded, 0.0);
        samples
    }

    fn reference_artillery_program() -> Vec<f32> {
        const PROGRAM_FRAMES: usize = 96_000;
        const FADE_FRAMES: usize = 2_400;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/assets/music/artillery-impact-48k-mono.wav");
        let bytes = std::fs::read(path).expect("read signed artillery reference");
        let decoded = decode_pcm16_mono(&bytes);
        let peak = decoded
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let onset = decoded
            .iter()
            .position(|sample| sample.abs() >= peak * 1.0e-3)
            .expect("signed artillery onset");
        let mut program = decoded[onset..onset + PROGRAM_FRAMES].to_vec();
        let fade_start = program.len() - FADE_FRAMES;
        for (offset, sample) in program[fade_start..].iter_mut().enumerate() {
            let phase = std::f64::consts::PI * 0.5 * offset as f64 / FADE_FRAMES as f64;
            *sample *= phase.cos().powi(2) as f32;
        }
        program
    }

    fn decode_pcm16_mono(bytes: &[u8]) -> Vec<f32> {
        assert!(bytes.len() >= 12);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let mut position = 12;
        let mut format = None;
        let mut data = None;
        while position + 8 <= bytes.len() {
            let id = &bytes[position..position + 4];
            let size =
                u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
            position += 8;
            let end = position.checked_add(size).expect("WAV chunk size overflow");
            assert!(end <= bytes.len(), "truncated source WAV chunk");
            if id == b"fmt " {
                assert!(size >= 16);
                format = Some((
                    u16::from_le_bytes(bytes[position..position + 2].try_into().unwrap()),
                    u16::from_le_bytes(bytes[position + 2..position + 4].try_into().unwrap()),
                    u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()),
                    u16::from_le_bytes(bytes[position + 14..position + 16].try_into().unwrap()),
                ));
            } else if id == b"data" {
                data = Some(&bytes[position..end]);
            }
            position = end + (size & 1);
        }
        assert_eq!(format, Some((1, 1, 48_000, 16)));
        data.expect("source WAV data chunk")
            .chunks_exact(2)
            .map(|sample| f32::from(i16::from_le_bytes([sample[0], sample[1]])) / 32_768.0)
            .collect()
    }

    fn stereo_frequency_magnitude(samples: &[f32], frequency_hz: f64) -> f64 {
        assert_eq!(samples.len() % 2, 0);
        let frames = samples.len() / 2;
        let omega = std::f64::consts::TAU * frequency_hz / 48_000.0;
        let mut channel_power = 0.0_f64;
        for channel in 0..2 {
            let (real, imaginary) = (0..frames).fold((0.0_f64, 0.0_f64), |sum, frame| {
                let phase = omega * frame as f64;
                let sample = f64::from(samples[frame * 2 + channel]);
                (sum.0 + sample * phase.cos(), sum.1 - sample * phase.sin())
            });
            channel_power += real * real + imaginary * imaginary;
        }
        channel_power.sqrt()
    }

    fn assert_samples_bit_equal(left: &[f32], right: &[f32], message: &str) {
        assert_eq!(left.len(), right.len(), "{message}: sample count");
        if let Some((index, (left, right))) = left
            .iter()
            .zip(right)
            .enumerate()
            .find(|(_, (left, right))| left.to_bits() != right.to_bits())
        {
            panic!(
                "{message}: first mismatch at sample {index}: {:08x} != {:08x}",
                left.to_bits(),
                right.to_bits()
            );
        }
    }

    #[test]
    fn weight_zero_unbaked_direct_render_is_bit_identical_to_the_prechange_fingerprint() {
        let toward = ApiEnuVector3::new(2.0, 3.0, 0.0);
        let (legacy_default, legacy_direct) = directivity_capture(None, toward, None);
        let (explicit_omni, explicit_direct) =
            directivity_capture(Some(Directivity::OMNIDIRECTIONAL), toward, None);
        let (explicit_point, point_direct) = directivity_capture(
            Some(Directivity::OMNIDIRECTIONAL),
            toward,
            Some(ExtentDescriptor::Point),
        );
        let (explicit_impulse_none, impulse_none_direct) = directivity_capture_with_impulse(
            Some(Directivity::OMNIDIRECTIONAL),
            toward,
            Some(ExtentDescriptor::Point),
            Some(fightbox_api::ImpulseClass::None),
        );
        let (explicit_echo_off, echo_off_direct) = directivity_capture_with_profile(
            Some(Directivity::OMNIDIRECTIONAL),
            toward,
            Some(ExtentDescriptor::Point),
            Some(fightbox_api::ImpulseClass::None),
            Some(EchoProfile::OFF),
        );
        let enabled_profile =
            EchoProfile::from_loop_frames(24 * 128, &[0], fightbox_api::ImpulseClass::None)
                .unwrap();
        let (enabled_direct, enabled_direct_params) = directivity_capture_with_profile(
            Some(Directivity::OMNIDIRECTIONAL),
            toward,
            Some(ExtentDescriptor::Point),
            Some(fightbox_api::ImpulseClass::None),
            Some(enabled_profile),
        );
        assert_eq!(sample_bits(&explicit_omni), sample_bits(&legacy_default));
        assert_eq!(sample_bits(&explicit_point), sample_bits(&legacy_default));
        assert_eq!(
            sample_bits(&explicit_impulse_none),
            sample_bits(&legacy_default)
        );
        assert_eq!(
            sample_bits(&explicit_echo_off),
            sample_bits(&legacy_default)
        );
        assert_eq!(sample_bits(&enabled_direct), sample_bits(&legacy_default));
        assert_eq!(explicit_direct, legacy_direct);
        assert_eq!(point_direct, legacy_direct);
        assert_eq!(impulse_none_direct, legacy_direct);
        assert_eq!(echo_off_direct, legacy_direct);
        assert_eq!(enabled_direct_params, legacy_direct);

        // Captured before directivity was plumbed through `source_inputs`: this
        // pins the same deterministic scene, tone, direct effect, and HRTF PCM.
        // `directivity_capture` deliberately constructs no baked probe batch,
        // so this golden covers the unbaked direct chain, not pathing.
        // Re-pinned when direct air moved to the shared ISO 9613-1 model.
        let hash = crate::sha256_hex(&sample_bytes(&explicit_omni));
        assert_eq!(
            hash,
            "050e8e536e9839aa32ed67c5060daad2c5a80e6b26d8ec4d529ab4d3d89f4698"
        );
        eprintln!(
            "omni_prechange_sha256={hash} energy={:.12e} samples={}",
            energy(&explicit_omni),
            explicit_omni.len()
        );
    }

    #[test]
    fn enabled_sidecar_renders_a_physical_late_branch_without_changing_block_length() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let profile = EchoProfile::from_loop_frames(
            audio.frame_size as u32,
            &[0],
            fightbox_api::ImpulseClass::None,
        )
        .unwrap();
        let descriptor = crate::MultiSourceDescriptor::at(ApiEnuVector3::new(10.0, 0.0, 1.5))
            .with_echo_profile(profile)
            .with_reflection_send(false);
        let (mut simulation, mut render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut stage_gains = render.take_stage_output_gain_writer().unwrap();
        stage_gains.publish(StageOutputGains {
            direct: 0.0,
            pathing: 0.0,
            reflections: 1.0,
        });
        let mut echo_gain = render.take_echo_output_gain_writer().unwrap();
        echo_gain.publish(1.0);

        let mut snapshot = simulation.snapshot;
        snapshot.sources[0].echo = EchoSourcePlan {
            generation: 1,
            tap_count: 1,
            taps: [
                EchoTapPlan {
                    valid: true,
                    stable_path_id: 7,
                    total_path_distance_m: 1.0,
                    delay_samples: 2.0,
                    arrival_position: api_enu_to_steam(ApiEnuVector3::new(10.0, 0.0, 1.5)),
                    distance_gain: 1.0,
                    band_gain: [1.0; 3],
                    score: 1.0,
                    ..EchoTapPlan::default()
                },
                EchoTapPlan::default(),
                EchoTapPlan::default(),
                EchoTapPlan::default(),
            ],
        };
        simulation.snapshot = snapshot;
        simulation.publication.publish(snapshot);

        let mut input = vec![0.0; audio.frame_size as usize];
        input[0] = 0.25;
        let (left, right) = render_one_source_block(&mut render, &input);
        assert_eq!(left.len(), input.len());
        assert_eq!(right.len(), input.len());
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
        assert!(
            energy(&left) + energy(&right) > 0.0,
            "the isolated echo branch was silent"
        );
    }

    struct HostEchoHarness {
        simulation: MultiSourceSimulation,
        render: MultiSourceRenderGraph,
        trigger: EchoTrigger,
        plans: EchoPlanWriter,
        _stage_gains: fightbox_runtime::SnapshotWriter<StageOutputGains>,
        _echo_gain: fightbox_runtime::SnapshotWriter<f32>,
    }

    /// One echo-enabled source whose only audible branch is the sidecar.
    fn host_echo_harness(loop_frames: u32) -> HostEchoHarness {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let profile =
            EchoProfile::from_loop_frames(loop_frames, &[0], fightbox_api::ImpulseClass::None)
                .unwrap();
        let descriptor = crate::MultiSourceDescriptor::at(ApiEnuVector3::new(10.0, 0.0, 1.5))
            .with_echo_profile(profile)
            .with_reflection_send(false);
        let (simulation, mut render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &[descriptor],
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        let mut stage_gains = render.take_stage_output_gain_writer().unwrap();
        stage_gains.publish(StageOutputGains {
            direct: 0.0,
            pathing: 0.0,
            reflections: 1.0,
        });
        let mut echo_gain = render.take_echo_output_gain_writer().unwrap();
        echo_gain.publish(1.0);
        let (trigger, plans) = render.take_echo_trigger_control().unwrap();
        assert!(render.take_echo_trigger_control().is_none());
        HostEchoHarness {
            simulation,
            render,
            trigger,
            plans,
            _stage_gains: stage_gains,
            _echo_gain: echo_gain,
        }
    }

    fn host_path(stable_path_id: u32, render_delay_samples: f32) -> crate::EchoPathGeometry {
        crate::EchoPathGeometry {
            kind: EchoPathKind::Specular,
            stable_path_id,
            physical_path_length_m: 20.0,
            render_delay_path_m: render_delay_samples / 48_000.0 * SPEED_OF_SOUND_METERS_PER_SECOND,
            arrival_position_enu: ApiEnuVector3::new(10.0, 5.0, 1.5),
            band_pressure_gain: [0.9, 0.8, 0.7],
        }
    }

    #[test]
    fn ambix_full_spatial_export_keeps_echo_and_scene_reset() {
        let mut harness = host_echo_harness(48_000 * 4);
        harness.plans.publish(0, EchoPrimary::LineOfSight, &[host_path(42, 400.0)]).unwrap();
        harness.trigger.trigger(0, 1);
        let reset = harness.render.scene_reset_control();
        let mut graph = FullSpatialExportGraph::new(harness.render).unwrap();
        graph.prepare_for_realtime().unwrap();
        let mut presentation = vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * 128];
        let mut environment = vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * 128];
        let mut metadata = fightbox_runtime::backend::SpatialOutputMetadata::default();
        let mut input = [0.0_f32; 128];
        input[0] = 0.5;
        let mut echo_energy = 0.0_f64;
        for block in 0..12 {
            if block == 1 { input.fill(0.0); }
            let sources = [SpatialBackendSourceBlock {
                source_index: 0, program_plane_count: 1, program_planes: [&input, &[]],
            }];
            graph.render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: block * 128, propagation_sequence: 1, sources: &sources,
                presentation_bank: &mut presentation, environmental_bank: &mut environment,
                metadata: &mut metadata,
            }).unwrap();
            assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
            assert_eq!(metadata.environmental_basis, SpatialEnvironmentalBasis::RightHandedXRightYUpZBack);
            echo_energy += environment.iter().map(|sample| f64::from(*sample).powi(2)).sum::<f64>();
        }
        assert!(echo_energy > 0.0, "the discrete echo must survive the pre-HRTF export tap");
        reset.reset();
        let sources = [SpatialBackendSourceBlock {
            source_index: 0, program_plane_count: 1, program_planes: [&input, &[]],
        }];
        graph.render_spatial_block(SpatialPropagationRenderBlock {
            block_start_frame: 12 * 128, propagation_sequence: 1, sources: &sources,
            presentation_bank: &mut presentation, environmental_bank: &mut environment,
            metadata: &mut metadata,
        }).unwrap();
        assert!(environment.iter().all(|sample| *sample == 0.0), "scene reset must clear old export echoes");
    }

    fn active_echo_delays(render: &MultiSourceRenderGraph) -> Vec<f32> {
        let echo = render.sources[0].echo.as_ref().unwrap();
        echo.active_plan.taps[..usize::from(echo.active_plan.tap_count)]
            .iter()
            .map(|tap| tap.delay_samples)
            .collect()
    }

    #[test]
    fn scene_reset_clears_pending_propagation_and_echo_before_restart() {
        let mut harness = host_echo_harness(48_000 * 4);
        harness._stage_gains.publish(StageOutputGains::UNITY);
        harness.plans.publish(0, EchoPrimary::LineOfSight, &[host_path(42, 960.0)]).unwrap();
        harness.trigger.trigger(0, 1);
        let mut input = [0.0_f32; 128];
        input[0] = 0.5;
        render_one_source_block(&mut harness.render, &input);
        input.fill(0.0);
        harness.render.scene_reset_control().reset();
        for block in 0..32 {
            let (left, right) = render_one_source_block(&mut harness.render, &input);
            let peak = left.iter().chain(&right).fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
            // SDK FFT roundoff may remain far below -140 dBFS.
            assert!(peak < 1.0e-7, "previous scene leaked after reset: block={block} peak={peak:e}");
        }
        harness.trigger.trigger(0, 2);
        input[0] = 0.5;
        render_one_source_block(&mut harness.render, &input);
        input.fill(0.0);
        let mut restarted_energy = 0.0;
        for _ in 0..32 {
            let (left, right) = render_one_source_block(&mut harness.render, &input);
            restarted_energy += energy(&left) + energy(&right);
        }
        assert!(restarted_energy > 0.0, "fresh scene must still render");
    }

    #[test]
    fn scene_air_reaches_direct_route_and_echo() {
        let mut harness = host_echo_harness(48_000 * 4);
        let exponents = fightbox_runtime::FrozenAtmosphere::freeze(Some(
            fightbox_api::atmosphere::AtmosphereObservation::new(35.0, 80.0, 101.325).unwrap(),
        )).three_band_air_pressure_exponents_per_m();
        let activity = harness.simulation.frame.active;
        let mut air_control = harness.simulation.take_scene_air_writer().unwrap();
        air_control.publish(exponents);
        harness.simulation.run_direct().unwrap();
        let config = harness.simulation.config;
        assert_eq!(config.air_pressure_exponents_per_m, exponents);
        let inputs = source_inputs(
            harness.simulation.frame.sources[0], Directivity::OMNIDIRECTIONAL,
            config.direct_occlusion, harness.simulation.world.probe_batch(), config,
            harness.simulation.governor.render_quality(), ffi::IPL_SIMULATIONFLAGS_DIRECT,
        ).unwrap();
        assert_eq!(inputs.airAbsorptionModel.coefficients, exponents);
        let length_m = 20.0;
        let (_, direct_air) = path_attenuation(harness.plans.world.context(), length_m, exponents);
        let routed_air = RouteAirVoicing::default().voice(
            [1.0; 3], PATH_SH_Y00 / length_m, length_m, config.air_pressure_exponents_per_m,
        );
        for (routed, direct) in routed_air.into_iter().zip(direct_air) {
            assert!((routed - direct).abs() < 1.0e-6);
        }
        harness.plans.set_air_exponents(exponents);
        let mut path = host_path(42, 960.0);
        path.band_pressure_gain = [1.0; 3];
        let planned = harness.plans.publish(0, EchoPrimary::LineOfSight, &[path]).unwrap();
        assert_eq!(planned.as_slice()[0].band_gain, direct_air);
        // Publication alone leaves the source's activity and trigger generations alone.
        assert_eq!(harness.simulation.frame.active, activity);
    }

    #[test]
    fn host_trigger_freezes_the_published_plan_at_its_render_delay() {
        let mut harness = host_echo_harness(48_000 * 4);
        let path = host_path(42, 960.0);
        let planned = harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[path])
            .unwrap();
        assert_eq!(planned.count, 1);
        let tap = planned.as_slice()[0];
        // Physical length alone keys the gain law; the render-delay path alone
        // sets timing.
        let (distance_gain, air) = path_attenuation(harness.plans.world.context(), 20.0, harness.plans.air_exponents);
        assert_eq!(tap.physical_path_length_m, 20.0);
        assert_eq!(tap.distance_gain, distance_gain);
        assert_eq!(
            tap.band_gain,
            std::array::from_fn(|band| air[band] * path.band_pressure_gain[band])
        );
        assert!((tap.delay_samples - 960.0).abs() < 1.0e-2, "{tap:?}");

        harness.trigger.trigger(0, 1);
        let mut left = Vec::new();
        let mut right = Vec::new();
        for block in 0..12 {
            let mut input = vec![0.0; 128];
            if block == 0 {
                input[0] = 0.5;
            }
            let (block_left, block_right) = render_one_source_block(&mut harness.render, &input);
            left.extend(block_left);
            right.extend(block_right);
        }
        assert_eq!(active_echo_delays(&harness.render), vec![tap.delay_samples]);
        let peak = left
            .iter()
            .chain(&right)
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        assert!(peak > 0.0, "the triggered echo was silent");
        let onset = left
            .iter()
            .zip(&right)
            .position(|(l, r)| l.abs().max(r.abs()) > peak * 1.0e-3)
            .unwrap();
        eprintln!("host_trigger_echo onset_frame={onset} peak={peak:.6e}");
        assert!(
            (955..=960 + 64).contains(&onset),
            "echo onset at frame {onset}, planned 960"
        );
    }

    #[test]
    fn trigger_mode_never_self_fires_the_loop_scheduler() {
        // Control: an untriggered source re-freezes at every descriptor onset
        // and adopts the host plan there.
        let mut scheduled = host_echo_harness(128);
        scheduled
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(1, 96.0)])
            .unwrap();
        render_one_source_block(&mut scheduled.render, &[0.0; 128]);
        let delays = active_echo_delays(&scheduled.render);
        assert_eq!(delays.len(), 1);
        assert!((delays[0] - 96.0).abs() < 1.0e-2);

        let mut harness = host_echo_harness(128);
        harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(1, 32.0)])
            .unwrap();
        harness.trigger.trigger(0, 1);
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        let first = active_echo_delays(&harness.render);
        assert!((first[0] - 32.0).abs() < 1.0e-2);

        // A new plan stays staged across blocks where the scheduler would fire.
        harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(2, 96.0)])
            .unwrap();
        for _ in 0..4 {
            render_one_source_block(&mut harness.render, &[0.0; 128]);
            assert_eq!(active_echo_delays(&harness.render), first);
        }
        // Re-storing an already observed generation is not a new trigger.
        harness.trigger.trigger(0, 1);
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        assert_eq!(active_echo_delays(&harness.render), first);

        harness.trigger.trigger(0, 2);
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        let second = active_echo_delays(&harness.render);
        assert!((second[0] - 96.0).abs() < 1.0e-2);
    }

    #[test]
    fn host_routes_reach_the_pathing_head_and_are_validated() {
        let mut harness = host_echo_harness(128);
        let route = crate::PrimaryRoute {
            length_m: 20.0,
            topology_id: 1,
        };
        assert_eq!(
            harness.plans.publish_route(1, Some(route)),
            Err(EchoPlanError::SourceOutOfRange)
        );
        for length_m in [
            0.0,
            f32::NAN,
            crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS + 1.0,
        ] {
            assert_eq!(
                harness
                    .plans
                    .publish_route(0, Some(crate::PrimaryRoute { length_m, ..route })),
                Err(EchoPlanError::InvalidPath)
            );
        }
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        assert!(!harness.render.sources[0].route_head.is_heard());
        harness.plans.publish_route(0, Some(route)).unwrap();
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        assert!(harness.render.sources[0].route_head.is_heard());
        // Leaving the route fades back to the straight line within 90 ms.
        harness.plans.publish_route(0, None).unwrap();
        for _ in 0..34 {
            render_one_source_block(&mut harness.render, &[0.0; 128]);
        }
        assert!(!harness.render.sources[0].route_head.is_heard());
    }

    #[test]
    fn host_plan_ranks_taps_by_pressure_with_no_reserved_corner() {
        let mut harness = host_echo_harness(48_000);
        let near = host_path(1, 480.0);
        let far = crate::EchoPathGeometry {
            stable_path_id: 2,
            physical_path_length_m: 400.0,
            ..host_path(2, 480.0)
        };
        let corner = crate::EchoPathGeometry {
            kind: EchoPathKind::Diffraction,
            stable_path_id: 3,
            ..host_path(3, 480.0)
        };
        let planned = harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[far, corner, near])
            .unwrap();
        let ids = planned
            .as_slice()
            .iter()
            .map(|tap| tap.stable_path_id)
            .collect::<Vec<_>>();
        // The corner voicing puts the diffraction path below the equally long
        // specular one; nothing reserves it the first slot.
        assert_eq!(ids, vec![1, 3, 2]);
        let invalid = crate::EchoPathGeometry {
            render_delay_path_m: crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS + 1.0,
            ..near
        };
        assert_eq!(
            harness
                .plans
                .publish(0, EchoPrimary::LineOfSight, &[invalid]),
            Err(EchoPlanError::InvalidPath)
        );
        assert_eq!(
            harness.plans.publish(1, EchoPrimary::LineOfSight, &[near]),
            Err(EchoPlanError::SourceOutOfRange)
        );
    }

    #[test]
    fn host_trigger_render_block_is_allocation_free() {
        let mut harness = host_echo_harness(48_000);
        harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(1, 480.0)])
            .unwrap();
        let input = [0.25_f32; 128];
        let mut left = [0.0_f32; 128];
        let mut right = [0.0_f32; 128];
        let render = |render: &mut MultiSourceRenderGraph, left: &mut [f32], right: &mut [f32]| {
            let source = [BackendSourceBlock {
                source_index: 0,
                input_mono: &input,
            }];
            render
                .render_block(PropagationRenderBlock {
                    listener_orientation: ListenerOrientation {
                        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                    },
                    sources: &source,
                    output_left: left,
                    output_right: right,
                })
                .unwrap();
        };
        harness.trigger.trigger(0, 1);
        for _ in 0..4 {
            render(&mut harness.render, &mut left, &mut right);
        }
        let allocations = crate::propagation_delay_stereo_tests::count_allocations(|| {
            harness.trigger.trigger(0, 2);
            render(&mut harness.render, &mut left, &mut right);
            render(&mut harness.render, &mut left, &mut right);
        });
        assert_eq!(allocations, 0);
        assert!(left.iter().chain(&right).any(|sample| *sample != 0.0));
    }

    /// Street demo Spot A: the planned routes over the primary Steam renders.
    /// The primary transfer is the one captured there
    /// (evidence/fullsuite-shot-20260930 spot A): path EQ, and order-2 path SH
    /// energy 2.295e-6, i.e. W = Y00 / 558.6 m.
    #[test]
    fn host_echoes_inherit_the_routed_primary_and_sit_below_it() {
        const STRAIGHT_M: f32 = 505.3;
        const PRIMARY_M: f32 = 559.5;
        let path_eq = [0.930_508_2, 0.794_549_2, 0.664_344_97];
        let path_sh0 = PATH_SH_Y00 * 1.790_238_5e-3;
        let primary = primary_path_transfer(path_eq, path_sh0).unwrap();
        // (id, kind, physical length, host band pressure) from the planner.
        let routes = [
            (0xa12f_1f5b, EchoPathKind::Diffraction, 634.6, [1.0; 3]),
            (0x963a_cfba, EchoPathKind::Diffraction, 641.9, [1.0; 3]),
            (0xe661_82fc, EchoPathKind::Diffraction, 691.9, [1.0; 3]),
            (
                0x6629_c7ec,
                EchoPathKind::Specular,
                660.5,
                [0.322, 0.161, 0.056],
            ),
        ];
        let paths = routes.map(|(id, kind, length, pressure)| crate::EchoPathGeometry {
            kind,
            stable_path_id: id,
            physical_path_length_m: length,
            render_delay_path_m: STRAIGHT_M + (length - PRIMARY_M),
            arrival_position_enu: ApiEnuVector3::new(470.7, 492.5, 1.5),
            band_pressure_gain: pressure,
        });
        let mut harness = host_echo_harness(48_000 * 4);
        let routed = EchoPrimary::Routed {
            path_length_m: PRIMARY_M,
        };
        let planned = harness.plans.publish(0, routed, &paths).unwrap();
        assert_eq!(planned.count, 4);
        let db = |gain: f32| 20.0 * gain.log10();
        for tap in planned.as_slice() {
            let relative = tap.primary_relative_gain.unwrap();
            let (length, interaction) = routes
                .iter()
                .find(|route| route.0 == tap.stable_path_id)
                .map(|route| (route.2, path_interaction(route.1, route.3)))
                .unwrap();
            let (_, excess_air) =
                path_attenuation(harness.plans.world.context(), length - PRIMARY_M, harness.plans.air_exponents);
            let before: [f32; 3] = std::array::from_fn(|band| {
                tap.distance_gain * tap.band_gain[band] / (primary.broadband * primary.bands[band])
            });
            eprintln!(
                "spot_a_echo id={:#010x} L={length} before_db={:?} after_db={:?}",
                tap.stable_path_id,
                before.map(db),
                relative.map(db)
            );
            for band in 0..3 {
                let expected = PRIMARY_M / length * excess_air[band] * interaction[band];
                assert!((relative[band] - expected).abs() <= expected * 1.0e-5);
                assert!(
                    relative[band] > 0.0 && db(relative[band]) < -9.0,
                    "{relative:?}"
                );
            }
        }

        // The trigger block reads the rendered primary and freezes it.
        let mut snapshot = harness.simulation.snapshot;
        snapshot.sources[0].path_eq = path_eq;
        snapshot.sources[0].path_sh[0] = path_sh0;
        harness.simulation.snapshot = snapshot;
        harness.simulation.publication.publish(snapshot);
        harness.trigger.trigger(0, 1);
        render_one_source_block(&mut harness.render, &[0.0; 128]);
        let echo = harness.render.sources[0].echo.as_ref().unwrap();
        assert_eq!(echo.primary_transfer, Some(primary));
        for tap in &echo.active_plan.taps[..usize::from(echo.active_plan.tap_count)] {
            let (distance, bands) = echo_tap_gains(tap, echo.primary_transfer);
            for (band, gain) in bands.into_iter().enumerate() {
                let rendered = distance * gain;
                let expected =
                    primary.broadband * primary.bands[band] * tap.primary_relative_gain[band];
                assert!((rendered - expected).abs() <= expected * 1.0e-5);
                assert!(rendered < primary.broadband * primary.bands[band]);
            }
        }
        assert_eq!(harness.trigger.take_transfer_fallbacks(), 0);

        // No baked-path transfer at the trigger: free-field law, flagged once.
        let mut fallback = host_echo_harness(48_000 * 4);
        fallback.plans.publish(0, routed, &paths).unwrap();
        fallback.trigger.trigger(0, 1);
        render_one_source_block(&mut fallback.render, &[0.0; 128]);
        let echo = fallback.render.sources[0].echo.as_ref().unwrap();
        assert_eq!(echo.primary_transfer, None);
        let tap = echo.active_plan.taps[0];
        assert_eq!(
            echo_tap_gains(&tap, echo.primary_transfer),
            (tap.distance_gain, tap.band_gain)
        );
        assert_eq!(fallback.trigger.take_transfer_fallbacks(), 1);
        assert_eq!(fallback.trigger.take_transfer_fallbacks(), 0);
    }

    #[test]
    fn a_retrigger_never_replays_the_previous_shot_on_its_new_plan() {
        let mut impulse = [0.0_f32; 128];
        impulse[0] = 0.5;
        let render_blocks = |harness: &mut HostEchoHarness, from: usize, blocks: usize| {
            let mut peak_by_frame = Vec::new();
            for block in from..from + blocks {
                let input = if block == 0 { impulse } else { [0.0; 128] };
                let (left, right) = render_one_source_block(&mut harness.render, &input);
                peak_by_frame.extend(left.iter().zip(&right).map(|(l, r)| l.abs().max(r.abs())));
            }
            peak_by_frame
        };

        // Control: one shot at frame 0 on a 400-sample plan echoes at 400, so
        // the ring still holds it there.
        let mut control = host_echo_harness(48_000 * 4);
        control
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(1, 400.0)])
            .unwrap();
        control.trigger.trigger(0, 1);
        let output = render_blocks(&mut control, 0, 5);
        // The effect chain idles at a ~1e-17 floor; an echo is far above it.
        let audible = output.iter().copied().fold(0.0_f32, f32::max) * 1.0e-3;
        let first = output.iter().position(|peak| *peak > audible).unwrap();
        assert!((400..=464).contains(&first), "control echo at {first}");

        // A 256-sample plan fires at frame 0; the 400-sample plan retriggers
        // at frame 128 with a new shot there. The first shot sits in the ring
        // at the new tap's delay from frame 400 on, but must never sound.
        let mut harness = host_echo_harness(48_000 * 4);
        harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(1, 256.0)])
            .unwrap();
        harness.trigger.trigger(0, 1);
        let mut output = render_blocks(&mut harness, 0, 1);
        harness
            .plans
            .publish(0, EchoPrimary::LineOfSight, &[host_path(2, 400.0)])
            .unwrap();
        harness.trigger.trigger(0, 2);
        let (left, right) = render_one_source_block(&mut harness.render, &impulse);
        output.extend(left.iter().zip(&right).map(|(l, r)| l.abs().max(r.abs())));
        output.extend(render_blocks(&mut harness, 2, 5));
        let first = output.iter().position(|peak| *peak > audible).unwrap();
        assert!(
            (128 + 400..=128 + 464).contains(&first),
            "first sound at frame {first}; the second shot's echo is due at 528"
        );
    }

    #[test]
    fn impulse_makeup_preserves_signed_reference_energy_through_the_real_chain() {
        let reference = reference_artillery_program();
        for distance_m in [5.0_f32, 50.0, 200.0, 500.0] {
            let tail_frames = (distance_m * 48_000.0 / 343.0).ceil() as usize + 8_192;
            let mut input = reference.clone();
            input.resize(input.len() + tail_frames, 0.0);
            let input = padded_to_block(input);
            let plain = impulse_test_capture(distance_m, ExtentDescriptor::Point, None, &input);
            let shaped = impulse_test_capture(
                distance_m,
                ExtentDescriptor::Point,
                Some(fightbox_api::ImpulseClass::ArtilleryThunder),
                &input,
            );
            let delta_db =
                10.0 * (energy(&shaped.interleaved) / energy(&plain.interleaved)).log10();
            assert!(
                delta_db.abs() <= 0.1,
                "{distance_m} m signed reference energy changed by {delta_db:+.6} dB"
            );
            eprintln!("impulse_energy distance_m={distance_m:.0} delta_db={delta_db:+.9}");
        }
    }

    #[test]
    fn impulse_residual_curve_matches_signed_knots_through_steam_air_and_hrtf() {
        const WINDOW_FRAMES: usize = 12_288;
        const MID_HZ: f64 = 2_000.0;
        const HIGH_HZ: f64 = 10_000.0;
        for distance_m in [5.0_f32, 50.0, 200.0, 500.0] {
            let delay_frames = (distance_m * 48_000.0 / 343.0).ceil() as usize;
            let frame_count = (delay_frames + WINDOW_FRAMES * 3).div_ceil(128) * 128;
            let input = (0..frame_count)
                .map(|frame| {
                    let time = frame as f64 / 48_000.0;
                    (0.1 * ((std::f64::consts::TAU * MID_HZ * time).sin()
                        + (std::f64::consts::TAU * HIGH_HZ * time).sin()))
                        as f32
                })
                .collect::<Vec<_>>();
            let plain = impulse_test_capture(distance_m, ExtentDescriptor::Point, None, &input);
            let shaped = impulse_test_capture(
                distance_m,
                ExtentDescriptor::Point,
                Some(fightbox_api::ImpulseClass::ArtilleryThunder),
                &input,
            );
            let window_start = (frame_count - WINDOW_FRAMES) * 2;
            let plain_window = &plain.interleaved[window_start..];
            let shaped_window = &shaped.interleaved[window_start..];
            let shaper =
                ImpulseShaper::new(fightbox_api::ImpulseClass::ArtilleryThunder, 48_000).unwrap();
            let parameters = shaper.parameters_at_distance(distance_m);
            for frequency_hz in [MID_HZ, HIGH_HZ] {
                let plain_magnitude = stereo_frequency_magnitude(plain_window, frequency_hz);
                let shaped_magnitude = stereo_frequency_magnitude(shaped_window, frequency_hz);
                assert!(plain_magnitude > 1.0e-12 && shaped_magnitude > 0.0);
                let measured_db = 20.0 * (shaped_magnitude / plain_magnitude).log10();
                let expected_db =
                    20.0 * parameters.residual_gain_at(frequency_hz, 48_000.0).log10();
                assert!(
                    (measured_db - expected_db).abs() <= 0.5,
                    "{distance_m} m at {frequency_hz} Hz measured {measured_db:.6} dB, expected signed residual {expected_db:.6} dB"
                );
                eprintln!(
                    "impulse_curve distance_m={distance_m:.0} frequency_hz={frequency_hz:.0} measured_db={measured_db:.6} expected_db={expected_db:.6}"
                );
            }
        }
    }

    #[test]
    fn line_width_consumes_the_shaped_then_delayed_mono_without_interaction() {
        const DISTANCE_M: f32 = 50.0;
        const LINE_LENGTH_M: f32 = 6.0;
        let mut random = 0x243f_6a88_85a3_08d3_u64;
        let raw = (0..32_768)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                ((random >> 40) as f32 / (1_u32 << 24) as f32 - 0.5) * 0.2
            })
            .collect::<Vec<_>>();
        let mut offline_shaper =
            ImpulseShaper::new(fightbox_api::ImpulseClass::ArtilleryThunder, 48_000).unwrap();
        let parameters = offline_shaper.parameters_at_distance(DISTANCE_M);
        let pre_shaped = raw
            .iter()
            .copied()
            .map(|sample| offline_shaper.process_sample(sample, parameters))
            .collect::<Vec<_>>();
        let extent = ExtentDescriptor::LineSegment {
            length_m: LINE_LENGTH_M,
        };
        let integrated = impulse_test_capture(
            DISTANCE_M,
            extent,
            Some(fightbox_api::ImpulseClass::ArtilleryThunder),
            &raw,
        );
        let sequential = impulse_test_capture(DISTANCE_M, extent, None, &pre_shaped);

        assert_eq!(integrated.source_count, 1);
        assert!(integrated.has_impulse_shaper && integrated.has_width);
        assert_eq!(integrated.width_declared_latency_samples, 0);
        assert_samples_bit_equal(
            &integrated.delayed_mono,
            &sequential.delayed_mono,
            "integrated stage was not shaping before the one shared delay",
        );
        assert_samples_bit_equal(
            integrated.width_presentation.as_ref().unwrap(),
            sequential.width_presentation.as_ref().unwrap(),
            "Wave 11 width did not consume the shaped delayed mono",
        );
        assert_samples_bit_equal(
            &integrated.interleaved,
            &sequential.interleaved,
            "shaping and width interacted in the direct C-arm output",
        );

        let source = api_enu_to_steam(ApiEnuVector3::new(0.0, DISTANCE_M, 1.5));
        let listener = api_enu_to_steam(ApiEnuVector3::new(0.0, 0.0, 1.5));
        let forward = api_enu_to_steam(ApiEnuVector3::new(1.0, 0.0, 0.0));
        let geometry = line_geometry(source, forward, listener, LINE_LENGTH_M);
        let mut width = LineWidthRenderer::new(48_000);
        let mut expected_presentation = Vec::with_capacity(integrated.delayed_mono.len() * 3);
        let mut block_output = vec![0.0; 128 * 3];
        for block in integrated.delayed_mono.chunks_exact(128) {
            width.render_presentation(block, geometry.k, false, &mut block_output);
            expected_presentation.extend_from_slice(&block_output);
        }
        assert_samples_bit_equal(
            integrated.width_presentation.as_ref().unwrap(),
            &expected_presentation,
            "captured C-arm presentation differed from shaping -> delay -> width",
        );
    }

    #[test]
    fn smoothed_distance_crosses_impulse_knots_without_a_block_discontinuity() {
        fn crossing(start_m: f32, end_m: f32) -> f32 {
            let audio = AudioConfig {
                sample_rate_hz: 48_000,
                frame_size: 128,
            };
            let listener = ApiEnuVector3::new(0.0, 0.0, 1.5);
            let start = ApiEnuVector3::new(start_m, 0.0, 1.5);
            let descriptor = [crate::MultiSourceDescriptor::at(start)
                .with_impulse_class(fightbox_api::ImpulseClass::ArtilleryThunder)];
            let (mut simulation, mut render) = build_multi_source_generation(
                &SceneMesh::controlled_s3_corner(),
                None,
                audio,
                test_config(),
                &descriptor,
                1,
                QualityTier::Desktop,
            )
            .unwrap();
            simulation.update_inputs(&one_source_update(true, start, listener));
            simulation.run_direct().unwrap();
            let constant = vec![0.25; audio.frame_size as usize];
            let warmup_blocks =
                (start_m * 48_000.0 / 343.0 / audio.frame_size as f32).ceil() as usize + 100;
            for _ in 0..warmup_blocks {
                render_one_source_block(&mut render, &constant);
            }

            let mut snapshot = simulation.snapshot;
            snapshot.sequence = snapshot.sequence.wrapping_add(1);
            snapshot.sources[0].source_position =
                api_enu_to_steam(ApiEnuVector3::new(end_m, 0.0, 1.5));
            simulation.publication.publish(snapshot);
            let mut captured = Vec::with_capacity(160 * audio.frame_size as usize);
            let mut crossed = false;
            let knot = (start_m + end_m) * 0.5;
            for _ in 0..160 {
                render_one_source_block(&mut render, &constant);
                captured.extend_from_slice(&render.mono_work);
                let applied = render.sources[0].propagation_smoother.applied();
                let distance =
                    smoothed_source_distance_m(applied.source_position, applied.listener_position);
                crossed |= distance >= knot;
            }
            assert!(crossed, "smoothed distance never crossed the {knot} m knot");

            let mut maximum_boundary_ratio = 0.0_f32;
            for boundary in
                (audio.frame_size as usize..captured.len()).step_by(audio.frame_size as usize)
            {
                let step = (captured[boundary] - captured[boundary - 1]).abs();
                let local_peak = captured[boundary - 64..boundary + 64]
                    .iter()
                    .copied()
                    .map(f32::abs)
                    .fold(0.0_f32, f32::max);
                maximum_boundary_ratio = maximum_boundary_ratio.max(step / local_peak.max(1.0e-12));
            }
            assert!(
                maximum_boundary_ratio <= 1.0,
                "{start_m}->{end_m} m exceeded the existing click bound: {maximum_boundary_ratio}"
            );
            maximum_boundary_ratio
        }

        let at_50_m = crossing(45.0, 55.0);
        let at_200_m = crossing(195.0, 205.0);
        eprintln!(
            "impulse_slew max_boundary_ratio_50m={at_50_m:.9} max_boundary_ratio_200m={at_200_m:.9} bound=1.0"
        );
    }

    #[test]
    fn live_stereo_program_keeps_endpoint_image_and_correlated_level() {
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let source_position = ApiEnuVector3::new(493.8, 473.0, 8.0);
        let listener_position = ApiEnuVector3::new(426.02, 483.82, 1.5);
        let forward = ApiEnuVector3::new(0.987496, -0.157637, 0.0);
        let orientation = ListenerOrientation {
            forward,
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        };
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest { mesh: mesh.clone(), ..S3BakeRequest::default() }).unwrap();
        let config = S3SimulationConfig {
            direct_occlusion: DirectOcclusionMode::Volumetric { radius_m: 1.0, sample_count: 64 },
            ..test_config()
        };
        let capture = |stereo: bool, correlated: bool| {
            let descriptor = crate::MultiSourceDescriptor::at(source_position)
                .with_initially_active(false)
                .with_extent(if stereo {
                    ExtentDescriptor::StereoImage { width_m: 4.0 }
                } else {
                    ExtentDescriptor::Point
                });
            let (mut simulation, mut render) = build_multi_source_generation(
                &mesh, Some(&baked), audio, config, &[descriptor],
                1, QualityTier::Desktop,
            ).unwrap();
            let mut update = one_source_update(true, source_position, listener_position);
            update.listener.pose.forward = forward;
            update.sources[0].pose.forward = forward;
            simulation.update_inputs(&update);
            simulation.run_direct().unwrap();
            simulation.run_pathing().unwrap();
            simulation.run_reflections().unwrap();
            let mut captured = [Vec::new(), Vec::new()];
            let mut left = vec![0.0; audio.frame_size as usize];
            let mut right = left.clone();
            for block in 0..512 {
                let mut program_left = (0..audio.frame_size as usize).map(|frame| {
                    let t = (block * audio.frame_size as usize + frame) as f64 / 48_000.0;
                    (std::f64::consts::TAU * 440.0 * t).sin() as f32 * 0.2
                }).collect::<Vec<_>>();
                let program_right = if correlated {
                    program_left.clone()
                } else {
                    (0..audio.frame_size as usize).map(|frame| {
                        let t = (block * audio.frame_size as usize + frame) as f64 / 48_000.0;
                        (std::f64::consts::TAU * 660.0 * t).sin() as f32 * 0.2
                    }).collect::<Vec<_>>()
                };
                if !stereo && !correlated {
                    for (left, right) in program_left.iter_mut().zip(&program_right) {
                        *left = 0.5 * *left + 0.5 * *right;
                    }
                }
                let sources = [SpatialBackendSourceBlock {
                    source_index: 0,
                    program_plane_count: if stereo { 2 } else { 1 },
                    program_planes: [&program_left, if stereo { &program_right } else { &[] }],
                }];
                left.fill(0.0);
                right.fill(0.0);
                render.render_program_block(fightbox_runtime::ProgramRenderBlock {
                    listener_orientation: orientation,
                    sources: &sources,
                    output_left: &mut left,
                    output_right: &mut right,
                }).unwrap();
                if block >= 128 {
                    captured[0].extend_from_slice(&left);
                    captured[1].extend_from_slice(&right);
                }
            }
            for plane in &mut captured {
                plane.truncate(48_000);
                assert!(plane.iter().copied().all(f32::is_finite));
            }
            captured
        };
        let tone_energy = |samples: &[f32], hz: f64| {
            let (real, imaginary) = samples.iter().enumerate().fold((0.0, 0.0), |acc, (i, x)| {
                let phase = std::f64::consts::TAU * hz * i as f64 / 48_000.0;
                (acc.0 + f64::from(*x) * phase.cos(),
                 acc.1 + f64::from(*x) * phase.sin())
            });
            real * real + imaginary * imaginary
        };
        let stereo = capture(true, false);
        let left_440 = tone_energy(&stereo[0], 440.0);
        let right_440 = tone_energy(&stereo[1], 440.0);
        let left_660 = tone_energy(&stereo[0], 660.0);
        let right_660 = tone_energy(&stereo[1], 660.0);
        let centered = capture(false, false);
        let left_bias_440_db = 10.0 * ((left_440 / right_440)
            / (tone_energy(&centered[0], 440.0) / tone_energy(&centered[1], 440.0))).log10();
        let right_bias_660_db = 10.0 * ((right_660 / left_660)
            / (tone_energy(&centered[1], 660.0) / tone_energy(&centered[0], 660.0))).log10();
        assert!(left_bias_440_db > 0.0 && right_bias_660_db > 0.0,
            "wrong stereo sign relative to center: 440 left bias={left_bias_440_db} dB, 660 right bias={right_bias_660_db} dB");
        assert!(left_440 > 1.0e-8 && right_660 > 1.0e-8);
        let uncorrelated_delta_db = 10.0 * ((energy(&stereo[0]) + energy(&stereo[1]))
            / (energy(&centered[0]) + energy(&centered[1]))).log10();
        assert!(uncorrelated_delta_db.abs() < 1.0,
            "uncorrelated stereo level differed by {uncorrelated_delta_db} dB from mono fold");
        let correlated = capture(true, true);
        let mono = capture(false, true);
        let delta_db = 10.0 * ((energy(&correlated[0]) + energy(&correlated[1]))
            / (energy(&mono[0]) + energy(&mono[1]))).log10();
        assert!(delta_db.abs() < 1.0, "correlated stereo level differed by {delta_db} dB");
        eprintln!("live_stereo correlated_delta_db={delta_db:.6} uncorrelated_delta_db={uncorrelated_delta_db:.6} 440_left_bias_db={left_bias_440_db:.6} 660_right_bias_db={right_bias_660_db:.6}");
    }

    #[test]
    fn live_stereo_program_preserves_indirect_stages_and_mono_fallback() {
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest { mesh: mesh.clone(), ..S3BakeRequest::default() }).unwrap();
        let audio = AudioConfig { sample_rate_hz: 48_000, frame_size: 128 };
        let position = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let descriptor = crate::MultiSourceDescriptor::at(position)
            .with_extent(ExtentDescriptor::StereoImage { width_m: 2.0 });
        let (mut simulation, mut render) = build_multi_source_session(
            &mesh, &baked, audio, test_config(), &[descriptor],
        ).unwrap();
        simulation.update_inputs(&one_source_update(true, position, ApiEnuVector3::new(4.0, 6.0, 1.5)));
        simulation.run_direct().unwrap();
        simulation.run_pathing().unwrap();
        simulation.run_reflections().unwrap();
        let mut energy_reader = render.take_live_stage_energy_reader().unwrap();
        let mut left = vec![0.0; audio.frame_size as usize];
        let mut right = left.clone();
        let mut path_energy = 0.0;
        let mut reflection_energy = 0.0;
        for block in 0..64 {
            let program = (0..audio.frame_size as usize).map(|frame| {
                let t = (block * audio.frame_size as usize + frame) as f32 / 48_000.0;
                (TAU * 440.0 * t).sin() * 0.1
            }).collect::<Vec<_>>();
            let sources = [SpatialBackendSourceBlock {
                source_index: 0,
                program_plane_count: 2,
                program_planes: [&program, &program],
            }];
            left.fill(0.0);
            right.fill(0.0);
            render.render_program_block(fightbox_runtime::ProgramRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            }).unwrap();
            render.sources[0].path_stereo.read_interleaved(&mut render.stereo_work);
            path_energy += energy(&render.stereo_work);
            reflection_energy += energy_reader.read().reflection_energy;
            assert!(left.iter().chain(&right).copied().all(f32::is_finite));
        }
        assert!(path_energy > 1.0e-10, "stereo lost baked pathing");
        assert!(reflection_energy > 1.0e-10, "stereo lost reflections");
        let stereo = render.sources[0].stereo_image.as_ref().unwrap();
        let timing = stereo.delay.instrumentation();
        assert_eq!(timing.trajectory_advances, 64 * audio.frame_size as u64);
        assert_eq!(timing.read_plan_advances, timing.trajectory_advances);

        let (mut mono_simulation, mut mono_render) = build_multi_source_session(
            &mesh, &baked, audio, test_config(), &[crate::MultiSourceDescriptor::at(position)],
        ).unwrap();
        mono_simulation.update_inputs(&one_source_update(true, position, ApiEnuVector3::new(4.0, 6.0, 1.5)));
        mono_simulation.run_direct().unwrap();
        mono_simulation.run_pathing().unwrap();
        mono_simulation.run_reflections().unwrap();
        let (mut fallback_simulation, mut fallback_render) = build_multi_source_session(
            &mesh, &baked, audio, test_config(), &[descriptor],
        ).unwrap();
        fallback_simulation.update_inputs(&one_source_update(true, position, ApiEnuVector3::new(4.0, 6.0, 1.5)));
        fallback_simulation.run_direct().unwrap();
        fallback_simulation.run_pathing().unwrap();
        fallback_simulation.run_reflections().unwrap();
        let input = vec![0.1; audio.frame_size as usize];
        for _ in 0..16 {
            render_one_source_block(&mut mono_render, &input);
            render_one_source_block(&mut fallback_render, &input);
            assert_eq!(sample_bits(&mono_render.live_direct_path_left), sample_bits(&fallback_render.live_direct_path_left));
            assert_eq!(sample_bits(&mono_render.live_direct_path_right), sample_bits(&fallback_render.live_direct_path_right));
            assert_eq!(sample_bits(&mono_render.mono_work), sample_bits(&fallback_render.mono_work));
        }
        eprintln!("live_stereo path_energy={path_energy:.9e} reflection_energy={reflection_energy:.9e}");
    }

    #[test]
    fn line_segment_uses_one_source_slot_and_three_directional_feeds() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_position = ApiEnuVector3::new(2.0, 3.0, 1.5);
        let listener_position = ApiEnuVector3::new(4.0, 6.0, 1.5);
        let descriptors = [crate::MultiSourceDescriptor::at(source_position)
            .with_extent(ExtentDescriptor::LineSegment { length_m: 6.0 })];
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
        simulation.update_inputs(&one_source_update(true, source_position, listener_position));
        simulation.run_direct().unwrap();

        assert_eq!(simulation.world.source_count, 1);
        assert_eq!(render.sources.len(), 1);
        let width = render.sources[0]
            .width
            .as_ref()
            .expect("LineSegment must own width state");
        assert_ne!(render.sources[0].binaural_effect, 0);
        assert_ne!(width.plus_binaural_effect, 0);
        assert_ne!(width.minus_binaural_effect, 0);

        let mut energy = 0.0_f64;
        let mut global_frame = 0_usize;
        for _ in 0..24 {
            let input = (0..audio.frame_size)
                .map(|_| {
                    let sample = (TAU * 731.0 * global_frame as f32 / audio.sample_rate_hz as f32)
                        .sin()
                        * 0.125;
                    global_frame += 1;
                    sample
                })
                .collect::<Vec<_>>();
            let (left, right) = render_one_source_block(&mut render, &input);
            for sample in left.into_iter().chain(right) {
                assert!(sample.is_finite());
                energy += f64::from(sample * sample);
            }
        }
        assert!(energy > 1.0e-6, "line renderer produced no direct output");
    }

    #[test]
    fn directional_source_outputs_less_direct_energy_when_facing_away() {
        let directivity = Directivity {
            dipole_weight: 0.7,
            dipole_power: 2.0,
        };
        let toward_axis = ApiEnuVector3::new(2.0, 3.0, 0.0);
        let away_axis = ApiEnuVector3::new(-2.0, -3.0, 0.0);
        let (toward, toward_direct) = directivity_capture(Some(directivity), toward_axis, None);
        let (away, away_direct) = directivity_capture(Some(directivity), away_axis, None);
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
    fn snapshot_seeds_pose_and_publishes_line_width_state_only() {
        let mesh = SceneMesh::controlled_s3_corner();
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let source_pose = Pose {
            position: ApiEnuVector3::new(0.0, 5.0, 0.0),
            forward: ApiEnuVector3::new(1.0, 0.0, 0.0),
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        };
        let descriptors = [
            crate::MultiSourceDescriptor::at(source_pose.position)
                .with_initial_pose(source_pose)
                .with_extent(ExtentDescriptor::LineSegment { length_m: 6.0 }),
            crate::MultiSourceDescriptor::at(source_pose.position)
                .with_initial_pose(source_pose)
                .with_extent(ExtentDescriptor::StereoImage { width_m: 6.0 }),
        ];
        let (simulation, render) = build_multi_source_generation(
            &mesh,
            None,
            audio,
            test_config(),
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();

        assert_eq!(simulation.world.source_count, 2);
        assert!(render.sources[0].width.is_some());
        assert!(render.sources[1].width.is_none());

        let line = simulation.snapshot.sources[0];
        assert_eq!(line.source_forward, api_enu_to_steam(source_pose.forward));
        assert_eq!(line.source_up, api_enu_to_steam(source_pose.up));
        assert_eq!(
            line.width.descriptor,
            ExtentDescriptor::LineSegment { length_m: 6.0 }
        );
        let expected_k = 3.0_f32 / (5.0_f32 * 5.0 + 3.0_f32 * 3.0).sqrt();
        assert!((line.width.geometric_k - expected_k).abs() <= 2.0e-6);
        assert_eq!(
            line.width.phi_eff_radians.to_bits(),
            (crate::width_render::PHI_MAX_RADIANS * line.width.geometric_k).to_bits()
        );
        assert_eq!(line.width.declared_latency_samples, 0);

        let stereo = simulation.snapshot.sources[1].width;
        assert_eq!(
            stereo.descriptor,
            ExtentDescriptor::StereoImage { width_m: 6.0 }
        );
        assert_eq!(stereo.geometric_k.to_bits(), 0.0_f32.to_bits());
        assert_eq!(stereo.phi_eff_radians.to_bits(), 0.0_f32.to_bits());
    }

    #[test]
    fn governor_direct_only_estimate_retains_transport_and_reads_one_snapshot_per_block() {
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
            render.sources[0].quality_gains, [1.0; 3],
            "an available detailed slot must retain every transport branch"
        );
        render_one_source_block(&mut render, &zeros);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        simulation.observe_render_timing(100_000);
        render_one_source_block(&mut render, &ones);
        assert_eq!(
            simulation.quality_governor_telemetry().sources[0].quality,
            SourceQualityLevel::Full
        );

        assert_eq!(render.governor_snapshot_reads - reads_before, 3);
        assert_eq!(render.sources[0].quality_gains, [1.0; 3]);
        assert!(
            render.mono_work.iter().any(|sample| *sample > 0.5),
            "the delay/transport path stopped while the source remained admitted"
        );

        // A zero direct observation cannot prove a baked path inaudible. With
        // no challenger for this available slot, it must not schedule a global
        // reflection fade or silence any transport branch.
        simulation.governor.observe_source_gain(0, 0.0);
        simulation.governor.rebalance_detailed_sources();
        let source = simulation.quality_governor_telemetry().sources[0];
        assert!(source.below_hearing_threshold);
        assert_eq!(source.quality, SourceQualityLevel::Full);
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
    fn offline_real_chain_corner_clip_has_one_physical_pitch_turn_without_residual_warble() {
        const SAMPLE_RATE: usize = 48_000;
        const FRAMES: usize = 128;
        const TONE_HZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 30.0;
        const TURN_SECONDS: f32 = 1.5;
        const TOTAL_SECONDS: f32 = 4.0;
        let mesh = SceneMesh::controlled_s3_corner();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let audio = AudioConfig {
            sample_rate_hz: SAMPLE_RATE as i32,
            frame_size: FRAMES as i32,
        };
        let start_x = 300.0 - SPEED_MPS * TURN_SECONDS;
        let descriptor = [crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
            start_x, 0.0, 10.0,
        ))];
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, test_config(), &descriptor).unwrap();
        direct_only(&mut render);

        let block_count = (TOTAL_SECONDS * SAMPLE_RATE as f32 / FRAMES as f32) as usize;
        let mut delayed_mono = Vec::with_capacity(block_count * FRAMES);
        let mut stereo = Vec::with_capacity(block_count * FRAMES * 2);
        let mut global_frame = 0_usize;
        for block in 0..block_count {
            let seconds = block as f32 * FRAMES as f32 / SAMPLE_RATE as f32;
            let (source_position, source_velocity) = if seconds < TURN_SECONDS {
                (
                    SteamVector3::new(start_x + SPEED_MPS * seconds, 10.0, 0.0),
                    SteamVector3::new(SPEED_MPS, 0.0, 0.0),
                )
            } else {
                (
                    SteamVector3::new(300.0, 10.0, -SPEED_MPS * (seconds - TURN_SECONDS)),
                    SteamVector3::new(0.0, 0.0, -SPEED_MPS),
                )
            };
            let mut snapshot = simulation.snapshot;
            snapshot.sequence = snapshot.sequence.wrapping_add(1);
            snapshot.sources[0].source_position = source_position;
            snapshot.sources[0].linear_velocity_mps = source_velocity;
            simulation.publication.publish(snapshot);
            let input: Vec<f32> = (0..FRAMES)
                .map(|_| {
                    let sample =
                        (TAU * TONE_HZ * global_frame as f32 / SAMPLE_RATE as f32).sin() * 0.1;
                    global_frame += 1;
                    sample
                })
                .collect();
            let (left, right) = render_one_source_block(&mut render, &input);
            delayed_mono.extend_from_slice(&render.mono_work);
            stereo.extend(
                left.into_iter()
                    .zip(right)
                    .flat_map(|(left, right)| [left, right]),
            );
        }

        let turn_distance_m = (300.0_f32 * 300.0 + 10.0 * 10.0).sqrt();
        let received_turn_seconds =
            TURN_SECONDS + turn_distance_m / SPEED_OF_SOUND_METERS_PER_SECOND;
        let pitch_window = SAMPLE_RATE / 10;
        let pitch_at = |seconds: f32| {
            let center = (seconds * SAMPLE_RATE as f32) as usize;
            let start = center - pitch_window / 2;
            let samples = &delayed_mono[start..start + pitch_window];
            samples
                .windows(2)
                .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
                .count() as f32
                * SAMPLE_RATE as f32
                / samples.len() as f32
        };
        let before_hz = pitch_at(received_turn_seconds - 0.20);
        let after_hz = pitch_at(received_turn_seconds + 0.20);
        let mut post_turn_pitch = Vec::new();
        for offset in [0.10_f32, 0.20, 0.30, 0.40, 0.50] {
            post_turn_pitch.push(pitch_at(received_turn_seconds + offset));
        }
        let maximum_residual_rise_hz = post_turn_pitch
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .fold(0.0_f32, f32::max);

        let level_window = SAMPLE_RATE / 20;
        let level_at = |seconds: f32| {
            let center = (seconds * SAMPLE_RATE as f32) as usize;
            let start = center - level_window / 2;
            let frames = &stereo[start * 2..(start + level_window) * 2];
            let mean_square = frames
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / frames.len() as f64;
            10.0 * mean_square.max(f64::MIN_POSITIVE).log10()
        };
        let levels: Vec<f64> = (-4..=8)
            .map(|step| level_at(received_turn_seconds + step as f32 * 0.05))
            .collect();
        let maximum_level_step_db = levels
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).abs())
            .fold(0.0_f64, f64::max);

        println!(
            "FAST_MOVER_CLIP received_turn_s={received_turn_seconds:.6} pitch_before_hz={before_hz:.3} pitch_after_hz={after_hz:.3} residual_post_turn_rise_hz={maximum_residual_rise_hz:.3} maximum_50ms_level_step_db={maximum_level_step_db:.3}"
        );
        assert!(
            (before_hz - 920.0).abs() <= 10.0,
            "pre-turn pitch was {before_hz}"
        );
        assert!(
            (after_hz - 1_000.0).abs() <= 15.0,
            "post-turn pitch was {after_hz}"
        );
        assert!(
            maximum_residual_rise_hz <= 10.0,
            "post-turn pitch oscillated upward by {maximum_residual_rise_hz} Hz"
        );
        assert!(
            maximum_level_step_db < 3.0,
            "corner level moved {maximum_level_step_db} dB in 50 ms"
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
                        0.0,
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
    fn linked_sixteen_source_session_boots_admits_and_renders_offline() {
        assert_eq!(MAX_ACTIVE_SOURCES, 16);
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptors = (0..MAX_ACTIVE_SOURCES)
            .map(|index| {
                crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
                    index as f32 * 0.5 - 4.0,
                    4.0 + index as f32 * 0.125,
                    1.5,
                ))
            })
            .collect::<Vec<_>>();
        let (mut simulation, mut render) = build_multi_source_generation(
            &impulse_test_floor_scene(),
            None,
            audio,
            test_config(),
            &descriptors,
            1,
            QualityTier::Desktop,
        )
        .unwrap();
        assert_eq!(render.sources.len(), MAX_ACTIVE_SOURCES);
        assert_eq!(
            simulation.quality_governor_telemetry().source_count,
            MAX_ACTIVE_SOURCES as u8
        );

        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for (source, descriptor) in sources.iter_mut().zip(&descriptors) {
            *source = SourceMotion {
                active: true,
                pose: descriptor.initial_pose(),
                linear_velocity_mps: ApiEnuVector3::default(),
            };
        }
        simulation.update_inputs(&SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 1.5)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        });
        simulation.run_direct().unwrap();

        let inputs = (0..MAX_ACTIVE_SOURCES)
            .map(|source_index| {
                (0..audio.frame_size)
                    .map(|frame| ((frame as f32 * 0.05) + source_index as f32 * 0.17).sin() * 0.001)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let blocks = inputs
            .iter()
            .enumerate()
            .map(|(source_index, input_mono)| BackendSourceBlock {
                source_index,
                input_mono,
            })
            .collect::<Vec<_>>();
        let mut left = vec![0.0; audio.frame_size as usize];
        let mut right = vec![0.0; audio.frame_size as usize];
        let mut rendered_energy = 0.0_f64;
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
            rendered_energy += left
                .iter()
                .chain(&right)
                .map(|sample| f64::from(*sample * *sample))
                .sum::<f64>();
        }
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
        assert!(rendered_energy > 0.0);
    }

    #[test]
    fn linked_mobile_session_admits_sixteen_logical_sources_but_only_four_full() {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: 128,
        };
        let descriptors = (0..MAX_ACTIVE_SOURCES)
            .map(|index| {
                crate::MultiSourceDescriptor::at(ApiEnuVector3::new(index as f32 * 0.25, 4.0, 1.5))
                    .with_reference_level(ReferenceLevel::CreativeDb { db: index as f32 })
            })
            .collect::<Vec<_>>();
        let (mut simulation, render) = build_multi_source_generation(
            &impulse_test_floor_scene(),
            None,
            audio,
            QualityTier::Mobile.simulation_defaults(),
            &descriptors,
            1,
            QualityTier::Mobile,
        )
        .unwrap();
        assert_eq!(render.sources.len(), MAX_ACTIVE_SOURCES);
        let telemetry = simulation.quality_governor_telemetry();
        assert_eq!(telemetry.source_count, 16);
        assert_eq!(telemetry.tier_source_cap, 4);
        assert!(
            telemetry.sources[..12]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::DirectOnly)
        );
        assert!(
            telemetry.sources[12..]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::Full)
        );

        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for (source, descriptor) in sources.iter_mut().zip(&descriptors) {
            *source = SourceMotion {
                active: true,
                pose: descriptor.initial_pose(),
                linear_velocity_mps: ApiEnuVector3::default(),
            };
        }
        simulation.update_inputs(&SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 1.5)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        });
        simulation.run_direct().unwrap();
    }

    #[test]
    fn callback_capacity_growth_has_a_bounded_inline_layout_delta() {
        const PREVIOUS_CAPACITY: usize = 8;
        let added_slots = MAX_ACTIVE_SOURCES - PREVIOUS_CAPACITY;
        let runtime_per_slot = size_of::<fightbox_runtime::SourceBlock<'static>>()
            + size_of::<BackendSourceBlock<'static>>()
            + size_of::<usize>()
            + size_of::<bool>()
            + size_of::<fightbox_runtime::SourcePropagation>()
            + size_of::<f32>();
        let backend_per_slot = size_of::<SteamSourcePropagation>()
            + size_of::<SourceQualityLevel>()
            + size_of::<bool>();
        let echo_per_slot = size_of::<EchoSourcePlan>() + size_of::<bool>() + size_of::<u8>() * 2;
        let common_delta_bytes = added_slots * (runtime_per_slot + backend_per_slot) + added_slots;
        let echo_delta_bytes = added_slots * echo_per_slot;
        let inline_layout_delta_bytes = common_delta_bytes + echo_delta_bytes;

        eprintln!(
            "capacity_callback_layout added_slots={added_slots} runtime_per_slot_bytes={runtime_per_slot} backend_per_slot_bytes={backend_per_slot} echo_per_slot_bytes={echo_per_slot} common_delta_bytes={common_delta_bytes} echo_delta_bytes={echo_delta_bytes} inline_layout_delta_bytes={inline_layout_delta_bytes} vec_payloads=excluded"
        );
        assert!(inline_layout_delta_bytes <= 16 * 1_024);
    }

    #[test]
    fn direct_only_preserves_direct_hrtf_occlusion_and_pathing_but_removes_reflections() {
        let direct_only = source_quality_targets(SourceQualityLevel::DirectOnly, true);
        assert_eq!(direct_only, [1.0, 1.0, 0.0]);
        let retained_path_sample = 0.25_f32;
        assert_eq!(retained_path_sample * direct_only[1], retained_path_sample);
        assert_eq!(
            source_quality_targets(SourceQualityLevel::Virtualized, true),
            [0.0, 0.0, 0.0]
        );
    }

    #[derive(Clone, Copy, Debug)]
    struct CapacitySoakObservation {
        p50_ns: u64,
        p99_ns: u64,
        p99_9_ns: u64,
        steam_tracked_payload_bytes: u64,
        runtime_delay_payload_bytes: u64,
        runtime_scratch_payload_bytes: u64,
        runtime_total_payload_bytes: u64,
        reported_total_payload_bytes: u64,
        deadline_misses: u64,
        detailed_sources: usize,
    }

    #[cfg(feature = "linked-sdk")]
    const FAST_MOVER_CPU_BLOCK_FRAMES: usize = 128;
    #[cfg(feature = "linked-sdk")]
    const FAST_MOVER_CPU_WARMUP_CALLBACKS: usize = 1_000;
    #[cfg(feature = "linked-sdk")]
    const FAST_MOVER_CPU_MEASURED_CALLBACKS: usize = 10_000;
    #[cfg(feature = "linked-sdk")]
    const FAST_MOVER_CPU_RADIUS_M: f32 = 100.0;
    #[cfg(feature = "linked-sdk")]
    const FAST_MOVER_CPU_SPEED_MPS: f32 = 110.0;

    #[cfg(feature = "linked-sdk")]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum FastMoverCpuScenario {
        Static,
        Moving,
    }

    #[cfg(feature = "linked-sdk")]
    impl FastMoverCpuScenario {
        const fn label(self) -> &'static str {
            match self {
                Self::Static => "static",
                Self::Moving => "moving_110_mps",
            }
        }
    }

    #[cfg(feature = "linked-sdk")]
    #[derive(Clone, Copy, Debug)]
    struct FastMoverCpuObservation {
        p50_ns: u64,
        p99_ns: u64,
        p99_9_ns: u64,
        measured_callbacks: usize,
        control_updates: usize,
        detailed_sources: usize,
        minimum_detailed_sources: usize,
        deadline_misses: u64,
        omitted_sources: usize,
        finite: bool,
        nonzero: bool,
        checksum: u64,
    }

    #[cfg(feature = "linked-sdk")]
    #[test]
    #[ignore = "Wave 17 linked-SDK timing gate; requires --release and an uncontended host"]
    fn wave17_fast_mover_full_callback_cpu_matrix() {
        assert!(
            !cfg!(debug_assertions),
            "wave17_fast_mover_full_callback_cpu_matrix must run with --release"
        );
        let mesh = impulse_test_floor_scene();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let (revision, tree) = fast_mover_cpu_git_provenance();
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };

        let maximum_update_step_m = 2.0
            * FAST_MOVER_CPU_RADIUS_M
            * (FAST_MOVER_CPU_SPEED_MPS / FAST_MOVER_CPU_RADIUS_M
                * 7.0
                * FAST_MOVER_CPU_BLOCK_FRAMES as f32
                / 48_000.0
                / 2.0)
                .sin();
        let teleport_step_m = TELEPORT_DELAY_STEP_SECONDS * SPEED_OF_SOUND_METERS_PER_SECOND;
        assert!(
            maximum_update_step_m < teleport_step_m,
            "the longest 60 Hz cadence interval must remain below the teleport threshold"
        );
        assert!(FAST_MOVER_CPU_RADIUS_M < crate::motion_smoothing::MAX_PROPAGATION_DISTANCE_METERS);

        let mut moving_rows = 0;
        let mut static_first_pairs = 0;
        let mut moving_first_pairs = 0;
        for (tier_index, quality_tier) in [QualityTier::Desktop, QualityTier::Mobile]
            .into_iter()
            .enumerate()
        {
            for (count_index, source_count) in [1_usize, 4, 8, 16].into_iter().enumerate() {
                let scenarios = if (tier_index + count_index) % 2 == 0 {
                    static_first_pairs += 1;
                    [FastMoverCpuScenario::Static, FastMoverCpuScenario::Moving]
                } else {
                    moving_first_pairs += 1;
                    [FastMoverCpuScenario::Moving, FastMoverCpuScenario::Static]
                };
                for scenario in scenarios {
                    let observation = observe_fast_mover_cpu_row(
                        source_count,
                        quality_tier,
                        scenario,
                        &mesh,
                        &baked,
                    );
                    println!(
                        "WAVE17_FAST_MOVER_FULL_CALLBACK_CPU revision={revision} tree={tree} \
                         profile={profile} features=linked-sdk tier={} count={source_count} \
                         scenario={} callbacks={} p50_ms={:.6} p99_ms={:.6} p99_9_ms={:.6} \
                         detail={} detail_min={} misses={} finite={} nonzero={} omitted={} \
                         checksum={:016x} control_updates={}",
                        fast_mover_cpu_tier_label(quality_tier),
                        scenario.label(),
                        observation.measured_callbacks,
                        observation.p50_ns as f64 / 1_000_000.0,
                        observation.p99_ns as f64 / 1_000_000.0,
                        observation.p99_9_ns as f64 / 1_000_000.0,
                        observation.detailed_sources,
                        observation.minimum_detailed_sources,
                        observation.deadline_misses,
                        observation.finite,
                        observation.nonzero,
                        observation.omitted_sources,
                        observation.checksum,
                        observation.control_updates,
                    );

                    assert_eq!(
                        observation.measured_callbacks,
                        FAST_MOVER_CPU_MEASURED_CALLBACKS
                    );
                    assert_eq!(observation.control_updates, 1_760);
                    assert_eq!(observation.omitted_sources, 0);
                    assert!(observation.finite, "non-finite output: {observation:?}");
                    assert!(observation.nonzero, "silent output: {observation:?}");

                    if scenario == FastMoverCpuScenario::Moving {
                        moving_rows += 1;
                        let expected_detail = quality_tier.detailed_source_cap().min(source_count);
                        assert_eq!(
                            observation.detailed_sources, expected_detail,
                            "moving row ended outside its tier detail policy: {observation:?}"
                        );
                        assert_eq!(
                            observation.minimum_detailed_sources, expected_detail,
                            "moving row shed detail during the measured callbacks: {observation:?}"
                        );
                        assert_eq!(
                            observation.deadline_misses, 0,
                            "moving row recorded callback deadline misses: {observation:?}"
                        );
                        assert!(
                            observation.p99_ns < 1_330_000,
                            "moving-row callback p99 exceeded 1.33 ms: {observation:?}"
                        );
                        assert!(
                            observation.p99_9_ns < 2_130_000,
                            "moving-row callback p99.9 exceeded 2.13 ms: {observation:?}"
                        );
                    }
                }
            }
        }
        assert_eq!(moving_rows, 8);
        assert_eq!(static_first_pairs, 4);
        assert_eq!(moving_first_pairs, 4);
    }

    #[cfg(feature = "linked-sdk")]
    fn observe_fast_mover_cpu_row(
        source_count: usize,
        quality_tier: QualityTier,
        scenario: FastMoverCpuScenario,
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
    ) -> FastMoverCpuObservation {
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: FAST_MOVER_CPU_BLOCK_FRAMES as i32,
        };
        let descriptors = (0..source_count)
            .map(|source_index| {
                crate::MultiSourceDescriptor::at(fast_mover_cpu_source_position(
                    source_index,
                    source_count,
                    0.0,
                ))
                .with_reference_level(ReferenceLevel::CreativeDb {
                    db: source_index as f32,
                })
            })
            .collect::<Vec<_>>();
        let (mut simulation, render) = crate::build_multi_source_session_for_tier(
            mesh,
            baked,
            audio,
            quality_tier.simulation_defaults(),
            &descriptors,
            quality_tier,
        )
        .unwrap();

        let (mut propagation_writer, propagation_reader) =
            SnapshotPublication::new(PropagationSnapshot::default());
        propagation_writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < source_count,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let config = EngineConfig {
            sample_rate_hz: 48_000,
            block_size_frames: FAST_MOVER_CPU_BLOCK_FRAMES as u32,
            max_active_sources: source_count as u8,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_backend(config, propagation_reader, Box::new(render)).unwrap();
        graph.set_listener_state(fast_mover_cpu_listener());
        for source_index in 0..source_count {
            graph
                .set_source(
                    source_index,
                    &capacity_source_profile(source_index),
                    SceneCalibration::default(),
                )
                .unwrap();
        }

        let inputs = (0..source_count)
            .map(|source_index| {
                (0..FAST_MOVER_CPU_BLOCK_FRAMES)
                    .map(|frame| {
                        ((frame as f32 * 0.05) + source_index as f32 * 0.17).sin() * 0.000_1
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let source_blocks = inputs
            .iter()
            .enumerate()
            .map(|(source_index, decoded_mono)| SourceBlock {
                source_index,
                decoded_mono,
            })
            .collect::<Vec<_>>();
        let omitted_sources = source_count.saturating_sub(source_blocks.len());
        assert_eq!(source_blocks.len(), source_count);
        assert!(
            source_blocks
                .iter()
                .enumerate()
                .all(|(index, block)| block.source_index == index)
        );

        let mut left = [0.0_f32; FAST_MOVER_CPU_BLOCK_FRAMES];
        let mut right = [0.0_f32; FAST_MOVER_CPU_BLOCK_FRAMES];
        let mut next_control_block = 0;
        let mut cadence_phase = 0;
        let mut control_updates = 0;
        let mut finite = true;
        let mut minimum_detailed_sources = usize::MAX;
        for total_block in 0..FAST_MOVER_CPU_WARMUP_CALLBACKS {
            if publish_fast_mover_cpu_control_if_due(
                &mut simulation,
                source_count,
                scenario,
                total_block,
                &mut next_control_block,
                &mut cadence_phase,
            ) {
                control_updates += 1;
                if total_block == 0 {
                    simulation.run_pathing().unwrap();
                    simulation.run_reflections().unwrap();
                }
                minimum_detailed_sources = minimum_detailed_sources
                    .min(fast_mover_cpu_detailed_sources(&simulation, source_count));
            }
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            finite &= left.iter().chain(&right).all(|sample| sample.is_finite());
        }

        let misses_before_measurement = graph.fault_counters().deadline_miss;
        let mut timings = Vec::with_capacity(FAST_MOVER_CPU_MEASURED_CALLBACKS);
        let mut nonzero = false;
        let mut checksum = 0xcbf2_9ce4_8422_2325_u64;
        for measured_block in 0..FAST_MOVER_CPU_MEASURED_CALLBACKS {
            let total_block = FAST_MOVER_CPU_WARMUP_CALLBACKS + measured_block;
            if publish_fast_mover_cpu_control_if_due(
                &mut simulation,
                source_count,
                scenario,
                total_block,
                &mut next_control_block,
                &mut cadence_phase,
            ) {
                control_updates += 1;
                minimum_detailed_sources = minimum_detailed_sources
                    .min(fast_mover_cpu_detailed_sources(&simulation, source_count));
            }

            let started = Instant::now();
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            timings.push(elapsed_ns);
            simulation.observe_render_timing(elapsed_ns);

            finite &= left.iter().chain(&right).all(|sample| sample.is_finite());
            nonzero |= left.iter().chain(&right).any(|sample| *sample != 0.0);
            for sample in left.iter().chain(&right) {
                checksum ^= u64::from(sample.to_bits());
                checksum = checksum.wrapping_mul(0x0000_0100_0000_01b3);
            }
            minimum_detailed_sources = minimum_detailed_sources
                .min(fast_mover_cpu_detailed_sources(&simulation, source_count));
        }

        timings.sort_unstable();
        let faults = graph.fault_counters();
        assert_eq!(
            faults.backend_render_error, 0,
            "backend render errors invalidate the CPU row"
        );
        FastMoverCpuObservation {
            p50_ns: capacity_percentile(&timings, 0.50),
            p99_ns: capacity_percentile(&timings, 0.99),
            p99_9_ns: capacity_percentile(&timings, 0.999),
            measured_callbacks: timings.len(),
            control_updates,
            detailed_sources: fast_mover_cpu_detailed_sources(&simulation, source_count),
            minimum_detailed_sources,
            deadline_misses: faults
                .deadline_miss
                .saturating_sub(misses_before_measurement),
            omitted_sources,
            finite,
            nonzero,
            checksum,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn publish_fast_mover_cpu_control_if_due(
        simulation: &mut crate::SteamAudioSimulationRunner,
        source_count: usize,
        scenario: FastMoverCpuScenario,
        block_index: usize,
        next_control_block: &mut usize,
        cadence_phase: &mut usize,
    ) -> bool {
        if block_index != *next_control_block {
            return false;
        }
        let elapsed_seconds = block_index as f32 * FAST_MOVER_CPU_BLOCK_FRAMES as f32 / 48_000.0;
        simulation.update_inputs(&fast_mover_cpu_simulation_update(
            source_count,
            scenario,
            elapsed_seconds,
        ));
        simulation.run_direct().unwrap();

        const INTERVALS: [usize; 4] = [6, 6, 6, 7];
        *next_control_block += INTERVALS[*cadence_phase];
        *cadence_phase = (*cadence_phase + 1) % INTERVALS.len();
        true
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_simulation_update(
        source_count: usize,
        scenario: FastMoverCpuScenario,
        elapsed_seconds: f32,
    ) -> SimulationUpdate {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for (source_index, source) in sources.iter_mut().take(source_count).enumerate() {
            let phase = fast_mover_cpu_source_phase(source_index, source_count);
            let angle = match scenario {
                FastMoverCpuScenario::Static => phase,
                FastMoverCpuScenario::Moving => {
                    phase + elapsed_seconds * FAST_MOVER_CPU_SPEED_MPS / FAST_MOVER_CPU_RADIUS_M
                }
            };
            let linear_velocity_mps = match scenario {
                FastMoverCpuScenario::Static => ApiEnuVector3::default(),
                FastMoverCpuScenario::Moving => ApiEnuVector3::new(
                    -FAST_MOVER_CPU_SPEED_MPS * angle.sin(),
                    FAST_MOVER_CPU_SPEED_MPS * angle.cos(),
                    0.0,
                ),
            };
            *source = SourceMotion {
                active: true,
                pose: default_api_pose(fast_mover_cpu_position_at_angle(angle)),
                linear_velocity_mps,
            };
        }
        SimulationUpdate {
            listener: fast_mover_cpu_listener(),
            sources,
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_listener() -> fightbox_api::ListenerState {
        fightbox_api::ListenerState {
            pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_source_position(
        source_index: usize,
        source_count: usize,
        elapsed_seconds: f32,
    ) -> ApiEnuVector3 {
        let angle = fast_mover_cpu_source_phase(source_index, source_count)
            + elapsed_seconds * FAST_MOVER_CPU_SPEED_MPS / FAST_MOVER_CPU_RADIUS_M;
        fast_mover_cpu_position_at_angle(angle)
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_source_phase(source_index: usize, source_count: usize) -> f32 {
        TAU * source_index as f32 / source_count as f32
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_position_at_angle(angle: f32) -> ApiEnuVector3 {
        ApiEnuVector3::new(
            FAST_MOVER_CPU_RADIUS_M * angle.cos(),
            FAST_MOVER_CPU_RADIUS_M * angle.sin(),
            1.5,
        )
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_detailed_sources(
        simulation: &crate::SteamAudioSimulationRunner,
        source_count: usize,
    ) -> usize {
        simulation.quality_governor_telemetry().unwrap().sources[..source_count]
            .iter()
            .filter(|source| source.quality == SourceQualityLevel::Full)
            .count()
    }

    #[cfg(feature = "linked-sdk")]
    const fn fast_mover_cpu_tier_label(quality_tier: QualityTier) -> &'static str {
        match quality_tier {
            QualityTier::Desktop => "desktop",
            QualityTier::Mobile => "mobile",
        }
    }

    #[cfg(feature = "linked-sdk")]
    fn fast_mover_cpu_git_provenance() -> (String, &'static str) {
        let revision = std::process::Command::new("git")
            .arg("-C")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .args(["rev-parse", "--verify", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|revision| revision.trim().to_owned())
            .filter(|revision| !revision.is_empty())
            .unwrap_or_else(|| "unavailable".to_owned());
        let tree = std::process::Command::new("git")
            .arg("-C")
            .arg(env!("CARGO_MANIFEST_DIR"))
            .args(["status", "--porcelain=v1", "--untracked-files=no"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map_or("unavailable", |output| {
                if output.stdout.is_empty() {
                    "clean"
                } else {
                    "dirty"
                }
            });
        (revision, tree)
    }

    #[test]
    #[ignore = "Wave 0 observational gate; requires the local Steam Audio SDK and an uncontended host"]
    fn wave0_eight_vs_sixteen_full_callback_and_footprint_short_soak() {
        let mesh = impulse_test_floor_scene();
        let baked = bake_s3(&S3BakeRequest {
            mesh: mesh.clone(),
            ..S3BakeRequest::default()
        })
        .unwrap();
        let eight = observe_capacity_soak(8, &mesh, &baked);
        let sixteen = observe_capacity_soak(16, &mesh, &baked);
        let steam_delta_bytes = sixteen
            .steam_tracked_payload_bytes
            .saturating_sub(eight.steam_tracked_payload_bytes);
        let reported_total_delta_bytes = sixteen
            .reported_total_payload_bytes
            .saturating_sub(eight.reported_total_payload_bytes);
        let fixed_runtime_capacity_delta_from_legacy_eight_slots = sixteen
            .runtime_total_payload_bytes
            .saturating_mul((MAX_ACTIVE_SOURCES - 8) as u64)
            / MAX_ACTIVE_SOURCES as u64;

        println!(
            "CAPACITY_WAVE0_SHORT_SOAK duration_s=2 blocks_per_row=750 \
             eight_p50_ms={:.4} eight_p99_ms={:.4} eight_p99_9_ms={:.4} \
             eight_steam_tracked_mib={:.2} eight_runtime_source_buffers_mib={:.2} \
             eight_reported_buffer_payload_mib={:.2} eight_detailed={} \
             sixteen_p50_ms={:.4} sixteen_p99_ms={:.4} sixteen_p99_9_ms={:.4} \
             sixteen_steam_tracked_mib={:.2} sixteen_runtime_source_buffers_mib={:.2} \
             sixteen_reported_buffer_payload_mib={:.2} sixteen_detailed={} \
             steam_delta_mib={:.2} reported_buffer_payload_delta_mib={:.2} \
             fixed_runtime_buffer_delta_from_legacy_8slot_mib={:.2} \
             runtime_delay_payload_mib={:.2} runtime_block_scratch_kib={:.2} \
             sdk_internal=untracked allocator_overhead=untracked",
            eight.p50_ns as f64 / 1_000_000.0,
            eight.p99_ns as f64 / 1_000_000.0,
            eight.p99_9_ns as f64 / 1_000_000.0,
            eight.steam_tracked_payload_bytes as f64 / (1024.0 * 1024.0),
            eight.runtime_total_payload_bytes as f64 / (1024.0 * 1024.0),
            eight.reported_total_payload_bytes as f64 / (1024.0 * 1024.0),
            eight.detailed_sources,
            sixteen.p50_ns as f64 / 1_000_000.0,
            sixteen.p99_ns as f64 / 1_000_000.0,
            sixteen.p99_9_ns as f64 / 1_000_000.0,
            sixteen.steam_tracked_payload_bytes as f64 / (1024.0 * 1024.0),
            sixteen.runtime_total_payload_bytes as f64 / (1024.0 * 1024.0),
            sixteen.reported_total_payload_bytes as f64 / (1024.0 * 1024.0),
            sixteen.detailed_sources,
            steam_delta_bytes as f64 / (1024.0 * 1024.0),
            reported_total_delta_bytes as f64 / (1024.0 * 1024.0),
            fixed_runtime_capacity_delta_from_legacy_eight_slots as f64 / (1024.0 * 1024.0),
            sixteen.runtime_delay_payload_bytes as f64 / (1024.0 * 1024.0),
            sixteen.runtime_scratch_payload_bytes as f64 / 1024.0,
        );

        assert_eq!(eight.detailed_sources, 8);
        assert_eq!(sixteen.detailed_sources, 8);
        assert!(sixteen.steam_tracked_payload_bytes > eight.steam_tracked_payload_bytes);
        assert_eq!(
            sixteen.runtime_total_payload_bytes,
            eight.runtime_total_payload_bytes
        );
        assert_eq!(
            sixteen.runtime_delay_payload_bytes,
            eight.runtime_delay_payload_bytes
        );
        assert_eq!(
            sixteen.runtime_scratch_payload_bytes,
            eight.runtime_scratch_payload_bytes
        );
        assert_eq!(reported_total_delta_bytes, steam_delta_bytes);
        for observation in [eight, sixteen] {
            assert_eq!(
                observation.runtime_total_payload_bytes,
                observation
                    .runtime_delay_payload_bytes
                    .saturating_add(observation.runtime_scratch_payload_bytes)
            );
            assert_eq!(
                observation.reported_total_payload_bytes,
                observation
                    .steam_tracked_payload_bytes
                    .saturating_add(observation.runtime_total_payload_bytes)
            );
            assert_eq!(observation.deadline_misses, 0);
            assert!(
                observation.p99_ns < 1_330_000,
                "p99 callback gate exceeded: {observation:?}"
            );
            assert!(
                observation.p99_9_ns < 2_130_000,
                "p99.9 callback gate exceeded: {observation:?}"
            );
        }
    }

    fn observe_capacity_soak(
        source_count: usize,
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
    ) -> CapacitySoakObservation {
        const BLOCK_FRAMES: usize = 128;
        const MEASURED_BLOCKS: usize = 48_000 * 2 / BLOCK_FRAMES;
        let audio = AudioConfig {
            sample_rate_hz: 48_000,
            frame_size: BLOCK_FRAMES as i32,
        };
        let descriptors = (0..source_count)
            .map(|index| {
                crate::MultiSourceDescriptor::at(ApiEnuVector3::new(
                    index as f32 * 0.5 - 4.0,
                    4.0 + index as f32 * 0.125,
                    1.5,
                ))
                .with_reference_level(ReferenceLevel::CreativeDb { db: index as f32 })
            })
            .collect::<Vec<_>>();
        let (mut simulation, render) = crate::build_multi_source_session_for_tier(
            mesh,
            baked,
            audio,
            test_config(),
            &descriptors,
            QualityTier::Desktop,
        )
        .unwrap();

        let mut motions = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for (source, descriptor) in motions.iter_mut().zip(&descriptors) {
            *source = SourceMotion {
                active: true,
                pose: descriptor.initial_pose(),
                linear_velocity_mps: ApiEnuVector3::default(),
            };
        }
        simulation.update_inputs(&SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 1.5)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources: motions,
        });
        simulation.run_direct().unwrap();
        simulation.run_pathing().unwrap();
        simulation.run_reflections().unwrap();
        let governor = simulation.quality_governor_telemetry().unwrap();
        let steam_tracked_payload_bytes = governor.memory.tracked_current_bytes;
        let detailed_sources = governor.sources[..source_count]
            .iter()
            .filter(|source| source.quality == SourceQualityLevel::Full)
            .count();

        let (mut propagation_writer, propagation_reader) =
            SnapshotPublication::new(PropagationSnapshot::default());
        propagation_writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < source_count,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let config = EngineConfig {
            sample_rate_hz: 48_000,
            block_size_frames: BLOCK_FRAMES as u32,
            max_active_sources: source_count as u8,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_backend(config, propagation_reader, Box::new(render)).unwrap();
        let runtime_memory = graph.persistent_memory();
        let reported_total_payload_bytes =
            steam_tracked_payload_bytes.saturating_add(runtime_memory.total_payload_bytes);
        graph.set_listener_state(fightbox_api::ListenerState {
            pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        });
        for source_index in 0..source_count {
            graph
                .set_source(
                    source_index,
                    &capacity_source_profile(source_index),
                    SceneCalibration::default(),
                )
                .unwrap();
        }

        let inputs = (0..source_count)
            .map(|source_index| {
                (0..BLOCK_FRAMES)
                    .map(|frame| {
                        ((frame as f32 * 0.05) + source_index as f32 * 0.17).sin() * 0.000_1
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        let source_blocks = inputs
            .iter()
            .enumerate()
            .map(|(source_index, decoded_mono)| SourceBlock {
                source_index,
                decoded_mono,
            })
            .collect::<Vec<_>>();
        let mut left = [0.0_f32; BLOCK_FRAMES];
        let mut right = [0.0_f32; BLOCK_FRAMES];
        for warmup_block in 0..64 {
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            assert!(
                left.iter().chain(&right).all(|sample| sample.is_finite()),
                "non-finite output during {source_count}-source warmup block {warmup_block}"
            );
        }

        let mut timings = Vec::with_capacity(MEASURED_BLOCKS);
        for measured_block in 0..MEASURED_BLOCKS {
            let started = Instant::now();
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            let elapsed_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            simulation.observe_render_timing(elapsed_ns);
            timings.push(elapsed_ns);
            assert!(
                left.iter().chain(&right).all(|sample| sample.is_finite()),
                "non-finite output during {source_count}-source measured block {measured_block}"
            );
        }
        assert!(left.iter().chain(&right).any(|sample| *sample != 0.0));
        timings.sort_unstable();
        CapacitySoakObservation {
            p50_ns: capacity_percentile(&timings, 0.50),
            p99_ns: capacity_percentile(&timings, 0.99),
            p99_9_ns: capacity_percentile(&timings, 0.999),
            steam_tracked_payload_bytes,
            runtime_delay_payload_bytes: runtime_memory.propagation_delay_payload_bytes,
            runtime_scratch_payload_bytes: runtime_memory.block_scratch_payload_bytes,
            runtime_total_payload_bytes: runtime_memory.total_payload_bytes,
            reported_total_payload_bytes,
            deadline_misses: graph.fault_counters().deadline_miss,
            detailed_sources,
        }
    }

    fn capacity_source_profile(source_index: usize) -> SourceProfile {
        SourceProfile {
            id: SourceId::new(format!("capacity-source-{source_index}")),
            pose: default_api_pose(ApiEnuVector3::default()),
            reference_level: ReferenceLevel::CreativeDb { db: 0.0 },
            asset_analysis: AssetAnalysis::new(
                -20.0,
                -1.0,
                AssetMeasurementProvenance::new("wave0-capacity-short-soak/v1").unwrap(),
            )
            .unwrap(),
            extent: ExtentDescriptor::Point,
            directivity: Directivity::OMNIDIRECTIONAL,
            max_speed_mps: 200.0,
        }
    }

    fn capacity_percentile(sorted: &[u64], fraction: f64) -> u64 {
        let rank = ((sorted.len() as f64 * fraction).ceil() as usize)
            .max(1)
            .min(sorted.len())
            - 1;
        sorted[rank]
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
        // Boot policy is intentionally allowed to start this audible source at
        // Full. Drive the governor to the quality state this integration test
        // names instead of coupling the path invariant to startup policy.
        for _ in 0..16 {
            if simulation.quality_governor_telemetry().sources[0].quality
                == SourceQualityLevel::DirectOnly
            {
                break;
            }
            simulation.observe_render_timing(10_000_000);
            simulation.observe_render_timing(100_000);
            simulation.observe_render_timing(100_000);
        }
        assert_eq!(
            simulation.quality_governor_telemetry().sources[0].quality,
            SourceQualityLevel::DirectOnly
        );

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
