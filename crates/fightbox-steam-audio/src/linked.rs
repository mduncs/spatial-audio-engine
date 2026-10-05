//! Safe RAII and owned Phase A operations over the private 4.8.1 FFI module.

use core::{marker::PhantomData, mem::size_of, ptr::NonNull};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::elevated_probes;
use crate::probe_mask::ProbeMask;
use crate::ffi;
use crate::{
    AudioConfig, BackendError, BakedProbeBatch, DirectOcclusionMode, DirectSnapshot,
    ElevatedProbeLayer, EnuVector3, ExplicitProbe, ExplicitProbeBakeRequest, ListenerPose,
    OwnedStereoPcm, PROBE_BATCH_METADATA_SCHEMA, PathBakeConfig, PathSnapshot,
    PathValidationSegment, ProbeBatchMetadata, ProbeVolume, ReflectionEffectType,
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
#[cfg(test)]
pub(crate) use multi_source::build_multi_source_generation as build_roof_evidence_generation;
pub(crate) use multi_source::EchoPlanWriter;
use multi_source::{
    MultiSourceRenderGraph as GenerationRenderGraph, MultiSourceSimulation as GenerationSimulation,
    NeutralMultiSourceRenderGraph as GenerationNeutralRenderGraph,
    build_anomaly_query_simulation as build_generation_anomaly_query_simulation,
    build_multi_source_generation, build_neutral_multi_source_generation,
};

use crate::world_swap;
use crate::{
    DeliveredWorldState, MultiSourceDescriptor, PreparedWorldCapabilities, PreparedWorldSwapError,
    QualityTier, StageOutputGains, WorldReflectionState,
};
use fightbox_runtime::backend::{
    BackendRenderError, MAX_ACTIVE_SOURCES, MAX_SPATIAL_ENVIRONMENT_PLANES,
    MAX_SPATIAL_PRESENTATION_FEEDS, MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE,
    PropagationRenderBlock, SimulationError, SimulationUpdate, SpatialAmbisonicChannelOrder,
    SpatialAmbisonicNormalization, SpatialAmbisonicOrder, SpatialBackendRenderError,
    SpatialBackendRenderGraph, SpatialEnvironmentalBasis, SpatialOutputMetadata,
    SpatialOutputValidity, SpatialPresentationComponent, SpatialPropagationRenderBlock,
    SpatialTailRetirementState,
};

const WORLD_SWAP_FADE_BLOCKS: u8 = 8;
const NEUTRAL_SWAP_FADE_BLOCKS: u8 = 8;
// A backend may request warmup, but never beyond this callback-bounded cap.
// Real Steam neutral graphs request the full 64-block runway: the measured
// four-cell route needs 56 blocks to cover the longest boundary propagation
// delay, leaving eight blocks of bounded margin. The default hook remains zero
// for existing/mock backends.
const NEUTRAL_SWAP_MAX_WARMUP_BLOCKS: u8 = 64;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NeutralSwapBankLayout {
    presentation_plane_count: usize,
    presentation_components_per_source: usize,
    presentation_component_slots: [SpatialPresentationComponent; 3],
    environmental_plane_count: usize,
    environmental_channel_order: SpatialAmbisonicChannelOrder,
    environmental_normalization: SpatialAmbisonicNormalization,
}

const WAVE0_NEUTRAL_SWAP_BANK_LAYOUT: NeutralSwapBankLayout = NeutralSwapBankLayout {
    presentation_plane_count: MAX_SPATIAL_PRESENTATION_FEEDS,
    presentation_components_per_source: MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE,
    presentation_component_slots: [
        SpatialPresentationComponent::DirectCenter,
        SpatialPresentationComponent::WidthPositive,
        SpatialPresentationComponent::WidthNegative,
    ],
    environmental_plane_count: MAX_SPATIAL_ENVIRONMENT_PLANES,
    environmental_channel_order: SpatialAmbisonicChannelOrder::Acn,
    environmental_normalization: SpatialAmbisonicNormalization::N3d,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NeutralSwapStageContract {
    world_space_unrotated: bool,
    source_drive_applied: bool,
    source_safety_gain_applied: bool,
    monitor_gain_applied: bool,
    final_hrtf_applied: bool,
    output_limiter_applied: bool,
}

const WAVE0_NEUTRAL_SWAP_STAGE_CONTRACT: NeutralSwapStageContract = NeutralSwapStageContract {
    world_space_unrotated: true,
    source_drive_applied: true,
    source_safety_gain_applied: true,
    monitor_gain_applied: false,
    final_hrtf_applied: false,
    output_limiter_applied: false,
};

/// Immutable format and route identity shared by every swappable neutral graph.
///
/// The active order describes emitted planes. The requested order records the
/// graph's configured ceiling even when quality policy currently emits a lower
/// prefix. Both are part of compatibility so a swap never changes the field
/// contract inside the callback.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NeutralSwapRouteIdentity {
    sample_rate_hz: u32,
    block_size_frames: u32,
    environmental_basis: SpatialEnvironmentalBasis,
    active_environmental_order: SpatialAmbisonicOrder,
    requested_environmental_order: SpatialAmbisonicOrder,
    environmental_latency_frames: u32,
    bank_layout: NeutralSwapBankLayout,
    stage_contract: NeutralSwapStageContract,
}

#[allow(dead_code)]
impl NeutralSwapRouteIdentity {
    pub(crate) const fn new(
        sample_rate_hz: u32,
        block_size_frames: u32,
        environmental_basis: SpatialEnvironmentalBasis,
        active_environmental_order: SpatialAmbisonicOrder,
        requested_environmental_order: SpatialAmbisonicOrder,
        environmental_latency_frames: u32,
    ) -> Self {
        Self {
            sample_rate_hz,
            block_size_frames,
            environmental_basis,
            active_environmental_order,
            requested_environmental_order,
            environmental_latency_frames,
            bank_layout: WAVE0_NEUTRAL_SWAP_BANK_LAYOUT,
            stage_contract: WAVE0_NEUTRAL_SWAP_STAGE_CONTRACT,
        }
    }

    const fn active_environmental_plane_count(self) -> usize {
        self.active_environmental_order.channel_count()
    }

    fn is_valid(self) -> bool {
        if self.sample_rate_hz == 0
            || self.block_size_frames == 0
            || self.bank_layout != WAVE0_NEUTRAL_SWAP_BANK_LAYOUT
            || self.stage_contract != WAVE0_NEUTRAL_SWAP_STAGE_CONTRACT
        {
            return false;
        }
        match (
            self.active_environmental_order.order(),
            self.requested_environmental_order.order(),
        ) {
            (Some(_), None) => false,
            (Some(active), Some(requested)) => active <= requested,
            _ => true,
        }
    }

    fn presentation_bank_samples(self) -> Option<usize> {
        self.bank_layout
            .presentation_plane_count
            .checked_mul(self.block_size_frames as usize)
    }

    fn environmental_bank_samples(self) -> Option<usize> {
        self.bank_layout
            .environmental_plane_count
            .checked_mul(self.block_size_frames as usize)
    }
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralSwapError {
    InvalidRouteIdentity,
    IncompatibleRouteIdentity,
    AdoptionPending,
    PreparationFailed(SpatialBackendRenderError),
}

const NEUTRAL_SWAP_IDLE: u8 = 0;
const NEUTRAL_SWAP_PUBLISHING: u8 = 1;
const NEUTRAL_SWAP_PREPARED: u8 = 2;
const NEUTRAL_SWAP_TRANSITION: u8 = 3;
const NEUTRAL_SWAP_RETIREMENT_BACKLOG: u8 = 4;
const NEUTRAL_SWAP_RETIRED_PENDING: u8 = 5;
const NEUTRAL_SWAP_TAIL_RETIRING: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NeutralSwapLifecycle {
    Idle,
    Publishing,
    Prepared,
    Crossfading,
    TailRetiring,
    TailComplete,
}

struct NeutralSwapGeneration<G> {
    graph: G,
    route: NeutralSwapRouteIdentity,
    warmup_blocks: u8,
}

/// Control-thread endpoint for admitting and destroying neutral generations.
#[allow(dead_code)]
pub(crate) struct NeutralSwapControl<G> {
    route: NeutralSwapRouteIdentity,
    prepared: world_swap::Producer<NeutralSwapGeneration<G>>,
    retired: world_swap::Consumer<NeutralSwapGeneration<G>>,
    lifecycle: Arc<AtomicU8>,
}

#[allow(dead_code)]
impl<G: SpatialBackendRenderGraph> NeutralSwapControl<G> {
    pub(crate) fn lifecycle(&self) -> NeutralSwapLifecycle {
        match self.lifecycle.load(Ordering::Acquire) {
            NEUTRAL_SWAP_PUBLISHING => NeutralSwapLifecycle::Publishing,
            NEUTRAL_SWAP_PREPARED => NeutralSwapLifecycle::Prepared,
            NEUTRAL_SWAP_TRANSITION => NeutralSwapLifecycle::Crossfading,
            NEUTRAL_SWAP_RETIREMENT_BACKLOG | NEUTRAL_SWAP_TAIL_RETIRING => {
                NeutralSwapLifecycle::TailRetiring
            }
            NEUTRAL_SWAP_RETIRED_PENDING => NeutralSwapLifecycle::TailComplete,
            _ => NeutralSwapLifecycle::Idle,
        }
    }

    /// Drops one retired graph on the calling control thread, if available.
    pub(crate) fn collect_retired(&mut self) -> bool {
        if self.lifecycle.load(Ordering::Acquire) != NEUTRAL_SWAP_RETIRED_PENDING {
            return false;
        }
        let Some(retired) = self.retired.try_pop() else {
            return false;
        };
        drop(retired);
        self.lifecycle.store(NEUTRAL_SWAP_IDLE, Ordering::Release);
        true
    }

    /// Prepares and offers one fully constructed graph to the next audio block
    /// boundary.
    ///
    /// Format mismatch, vendor preparation, preparation failure, and graph
    /// destruction all occur on this control-side call. A graph is never
    /// admitted while another is prepared, fading, or waiting to be collected
    /// after retirement.
    pub(crate) fn offer_prepared(
        &mut self,
        graph: G,
        route: NeutralSwapRouteIdentity,
    ) -> Result<(), NeutralSwapError> {
        self.offer_prepared_recoverable(graph, route)
            .map_err(|(error, _graph)| error)
    }

    pub(crate) fn offer_prepared_recoverable(
        &mut self,
        mut graph: G,
        route: NeutralSwapRouteIdentity,
    ) -> Result<(), (NeutralSwapError, G)> {
        if !route.is_valid() {
            return Err((NeutralSwapError::InvalidRouteIdentity, graph));
        }
        if route != self.route {
            return Err((NeutralSwapError::IncompatibleRouteIdentity, graph));
        }
        self.collect_retired();
        if self
            .lifecycle
            .compare_exchange(
                NEUTRAL_SWAP_IDLE,
                NEUTRAL_SWAP_PUBLISHING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Err((NeutralSwapError::AdoptionPending, graph));
        }

        if let Err(error) = graph.prepare_for_realtime() {
            self.lifecycle.store(NEUTRAL_SWAP_IDLE, Ordering::Release);
            return Err((NeutralSwapError::PreparationFailed(error), graph));
        }

        let warmup_blocks = graph
            .pre_crossfade_warmup_blocks()
            .min(NEUTRAL_SWAP_MAX_WARMUP_BLOCKS);
        let generation = NeutralSwapGeneration {
            graph,
            route,
            warmup_blocks,
        };
        if let Err(generation) = self.prepared.try_push(generation) {
            self.lifecycle.store(NEUTRAL_SWAP_IDLE, Ordering::Release);
            return Err((NeutralSwapError::AdoptionPending, generation.graph));
        }
        self.lifecycle
            .store(NEUTRAL_SWAP_PREPARED, Ordering::Release);
        Ok(())
    }
}

struct NeutralRetiringGeneration<G> {
    generation: NeutralSwapGeneration<G>,
    completed_blocks: u8,
}

struct NeutralWarmingGeneration<G> {
    generation: NeutralSwapGeneration<G>,
    remaining_blocks: u8,
}

/// Audio-thread endpoint for a bounded neutral generation warmup and the
/// existing eight-block transition.
#[allow(dead_code)]
pub(crate) struct NeutralSwapRenderGraph<G> {
    active: NeutralSwapGeneration<G>,
    prepared: world_swap::Consumer<NeutralSwapGeneration<G>>,
    retired: world_swap::Producer<NeutralSwapGeneration<G>>,
    lifecycle: Arc<AtomicU8>,
    warming: Option<NeutralWarmingGeneration<G>>,
    retiring: Option<NeutralRetiringGeneration<G>>,
    retirement_backlog: Option<NeutralSwapGeneration<G>>,
    old_presentation_bank: Vec<f32>,
    new_presentation_bank: Vec<f32>,
    old_environmental_bank: Vec<f32>,
    new_environmental_bank: Vec<f32>,
    old_metadata: SpatialOutputMetadata,
    new_metadata: SpatialOutputMetadata,
}

#[allow(dead_code)]
impl<G: SpatialBackendRenderGraph> NeutralSwapRenderGraph<G> {
    fn flush_retirement(&mut self) {
        let Some(retired) = self.retirement_backlog.take() else {
            return;
        };
        if let Err(retired) = self.retired.try_push(retired) {
            self.retirement_backlog = Some(retired);
            return;
        }
        self.lifecycle
            .store(NEUTRAL_SWAP_RETIRED_PENDING, Ordering::Release);
    }

    fn begin_crossfade(&mut self, prepared: NeutralSwapGeneration<G>) {
        self.active.graph.begin_tail_retirement();
        let old = core::mem::replace(&mut self.active, prepared);
        self.retiring = Some(NeutralRetiringGeneration {
            generation: old,
            completed_blocks: 0,
        });
        self.lifecycle
            .store(NEUTRAL_SWAP_TRANSITION, Ordering::Release);
    }

    fn adopt_at_block_boundary(&mut self) {
        self.flush_retirement();
        if self.retiring.is_some() || self.retirement_backlog.is_some() {
            return;
        }
        if let Some(warming) = self.warming.as_ref() {
            if warming.remaining_blocks == 0 {
                let warming = self
                    .warming
                    .take()
                    .expect("warming generation exists at boundary");
                self.begin_crossfade(warming.generation);
            }
            return;
        }
        if self.lifecycle.load(Ordering::Acquire) != NEUTRAL_SWAP_PREPARED {
            return;
        }
        let Some(prepared) = self.prepared.try_pop() else {
            return;
        };
        if prepared.warmup_blocks == 0 {
            self.begin_crossfade(prepared);
        } else {
            self.warming = Some(NeutralWarmingGeneration {
                remaining_blocks: prepared.warmup_blocks,
                generation: prepared,
            });
        }
    }

    fn render_warmup(
        &mut self,
        block_start_frame: u64,
        propagation_sequence: u64,
        sources: &[fightbox_runtime::backend::SpatialBackendSourceBlock<'_>],
        presentation_bank: &mut [f32],
        environmental_bank: &mut [f32],
        metadata: &mut SpatialOutputMetadata,
    ) -> Result<(), SpatialBackendRenderError> {
        let expected_discontinuity_sequence = metadata.discontinuity_sequence;
        let route = self.active.route;
        self.new_presentation_bank.fill(0.0);
        self.new_environmental_bank.fill(0.0);
        self.new_metadata = *metadata;

        // Keep the old graph audible while advancing both graphs with the same
        // globally indexed program block. Candidate output is confined to the
        // preallocated swap scratch and is never exposed before the fade.
        self.active
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank,
                environmental_bank,
                metadata,
            })?;
        if !neutral_metadata_matches_route(route, block_start_frame, metadata)
            || metadata.discontinuity_sequence != expected_discontinuity_sequence
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        normalize_inactive_neutral_banks(route, presentation_bank, environmental_bank, metadata);

        let warming = self
            .warming
            .as_mut()
            .ok_or(SpatialBackendRenderError::InactiveGraph)?;
        let candidate_route = warming.generation.route;
        warming
            .generation
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank: &mut self.new_presentation_bank,
                environmental_bank: &mut self.new_environmental_bank,
                metadata: &mut self.new_metadata,
            })?;
        if !neutral_metadata_matches_route(candidate_route, block_start_frame, &self.new_metadata)
            || self.new_metadata.discontinuity_sequence != expected_discontinuity_sequence
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        normalize_inactive_neutral_banks(
            candidate_route,
            &mut self.new_presentation_bank,
            &mut self.new_environmental_bank,
            &mut self.new_metadata,
        );
        warming.remaining_blocks = warming.remaining_blocks.saturating_sub(1);
        Ok(())
    }

    fn finish_transition_block(&mut self) {
        let Some(retiring) = self.retiring.as_mut() else {
            return;
        };
        retiring.completed_blocks += 1;
        if retiring.completed_blocks < NEUTRAL_SWAP_FADE_BLOCKS {
            return;
        }
        if retiring.generation.graph.tail_retirement_state()
            == SpatialTailRetirementState::TailRemaining
        {
            self.lifecycle
                .store(NEUTRAL_SWAP_TAIL_RETIRING, Ordering::Release);
            return;
        }
        self.finish_retirement();
    }

    fn finish_retirement(&mut self) {
        let Some(retiring) = self.retiring.take() else {
            return;
        };
        self.lifecycle
            .store(NEUTRAL_SWAP_RETIREMENT_BACKLOG, Ordering::Release);
        if let Err(retired) = self.retired.try_push(retiring.generation) {
            self.retirement_backlog = Some(retired);
            return;
        }
        self.lifecycle
            .store(NEUTRAL_SWAP_RETIRED_PENDING, Ordering::Release);
    }

    fn render_tail_only(
        &mut self,
        block_start_frame: u64,
        propagation_sequence: u64,
        sources: &[fightbox_runtime::backend::SpatialBackendSourceBlock<'_>],
        presentation_bank: &mut [f32],
        environmental_bank: &mut [f32],
        metadata: &mut SpatialOutputMetadata,
    ) -> Result<(), SpatialBackendRenderError> {
        let route = self.active.route;
        let expected_discontinuity_sequence = metadata.discontinuity_sequence;
        self.active
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank,
                environmental_bank,
                metadata,
            })?;
        if !neutral_metadata_matches_route(route, block_start_frame, metadata)
            || metadata.discontinuity_sequence != expected_discontinuity_sequence
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        normalize_inactive_neutral_banks(route, presentation_bank, environmental_bank, metadata);

        self.old_environmental_bank.fill(0.0);
        let tail_state = self
            .retiring
            .as_mut()
            .ok_or(SpatialBackendRenderError::InactiveGraph)?
            .generation
            .graph
            .render_retiring_environmental_tail(&mut self.old_environmental_bank)?;
        let block_size = route.block_size_frames as usize;
        for plane_index in 0..route.active_environmental_plane_count() {
            let start = plane_index * block_size;
            for frame in 0..block_size {
                environmental_bank[start + frame] += self.old_environmental_bank[start + frame];
            }
        }
        if tail_state == SpatialTailRetirementState::TailComplete {
            self.finish_retirement();
        }
        Ok(())
    }
}

impl<G: SpatialBackendRenderGraph> SpatialBackendRenderGraph for NeutralSwapRenderGraph<G> {
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        // Prepared swap candidates are control-thread-owned and must be
        // prepared before `offer_prepared`. This hook prepares only the
        // immutable active generation before its first audio block.
        self.active.graph.prepare_for_realtime()
    }

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        let SpatialPropagationRenderBlock {
            block_start_frame,
            propagation_sequence,
            sources,
            presentation_bank,
            environmental_bank,
            metadata,
        } = block;
        let route = self.active.route;
        if presentation_bank.len() != route.presentation_bank_samples().unwrap_or(0)
            || environmental_bank.len() != route.environmental_bank_samples().unwrap_or(0)
        {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }

        self.adopt_at_block_boundary();
        if self.warming.is_some() {
            return self.render_warmup(
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank,
                environmental_bank,
                metadata,
            );
        }
        let route = self.active.route;
        let Some(completed_blocks) = self
            .retiring
            .as_ref()
            .map(|retiring| retiring.completed_blocks)
        else {
            let expected_discontinuity_sequence = metadata.discontinuity_sequence;
            self.active
                .graph
                .render_spatial_block(SpatialPropagationRenderBlock {
                    block_start_frame,
                    propagation_sequence,
                    sources,
                    presentation_bank,
                    environmental_bank,
                    metadata,
                })?;
            if !neutral_metadata_matches_route(route, block_start_frame, metadata)
                || metadata.discontinuity_sequence != expected_discontinuity_sequence
            {
                return Err(SpatialBackendRenderError::InvalidOutputMetadata);
            }
            normalize_inactive_neutral_banks(
                route,
                presentation_bank,
                environmental_bank,
                metadata,
            );
            return Ok(());
        };

        if completed_blocks >= NEUTRAL_SWAP_FADE_BLOCKS {
            return self.render_tail_only(
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank,
                environmental_bank,
                metadata,
            );
        }

        self.old_presentation_bank.fill(0.0);
        self.new_presentation_bank.fill(0.0);
        self.old_environmental_bank.fill(0.0);
        self.new_environmental_bank.fill(0.0);
        self.old_metadata = *metadata;
        self.new_metadata = *metadata;

        let Some(retiring) = self.retiring.as_mut() else {
            return Err(SpatialBackendRenderError::InactiveGraph);
        };
        retiring
            .generation
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank: &mut self.old_presentation_bank,
                environmental_bank: &mut self.old_environmental_bank,
                metadata: &mut self.old_metadata,
            })?;
        self.active
            .graph
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame,
                propagation_sequence,
                sources,
                presentation_bank: &mut self.new_presentation_bank,
                environmental_bank: &mut self.new_environmental_bank,
                metadata: &mut self.new_metadata,
            })?;

        let expected_discontinuity_sequence = metadata.discontinuity_sequence;
        if !neutral_metadata_matches_route(
            retiring.generation.route,
            block_start_frame,
            &self.old_metadata,
        ) || !neutral_metadata_matches_route(route, block_start_frame, &self.new_metadata)
            || self.old_metadata.discontinuity_sequence != expected_discontinuity_sequence
            || self.new_metadata.discontinuity_sequence != expected_discontinuity_sequence
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        normalize_inactive_neutral_banks(
            route,
            &mut self.old_presentation_bank,
            &mut self.old_environmental_bank,
            &mut self.old_metadata,
        );
        normalize_inactive_neutral_banks(
            route,
            &mut self.new_presentation_bank,
            &mut self.new_environmental_bank,
            &mut self.new_metadata,
        );
        write_neutral_union_metadata(&self.old_metadata, &self.new_metadata, metadata)?;

        presentation_bank.fill(0.0);
        environmental_bank.fill(0.0);
        let block_size = route.block_size_frames as usize;
        let fade_frames = block_size * usize::from(NEUTRAL_SWAP_FADE_BLOCKS);
        let fade_start_frame = block_size * usize::from(completed_blocks);
        blend_neutral_planes(
            &self.old_presentation_bank,
            &self.new_presentation_bank,
            presentation_bank,
            route.bank_layout.presentation_plane_count,
            block_size,
            fade_start_frame,
            fade_frames,
        );
        blend_neutral_planes(
            &self.old_environmental_bank,
            &self.new_environmental_bank,
            environmental_bank,
            route.active_environmental_plane_count(),
            block_size,
            fade_start_frame,
            fade_frames,
        );
        self.finish_transition_block();
        Ok(())
    }
}

fn neutral_metadata_matches_route(
    route: NeutralSwapRouteIdentity,
    block_start_frame: u64,
    metadata: &SpatialOutputMetadata,
) -> bool {
    if metadata.sample_rate_hz != route.sample_rate_hz
        || metadata.block_size_frames != route.block_size_frames
        || metadata.block_start_frame != block_start_frame
        || metadata.validity != SpatialOutputValidity::Valid
        || metadata.active_environmental_order != route.active_environmental_order
        || metadata.active_environmental_plane_count != route.active_environmental_plane_count()
        || metadata.environmental_latency_frames != route.environmental_latency_frames
        || metadata.environmental_channel_order != route.bank_layout.environmental_channel_order
        || metadata.environmental_normalization != route.bank_layout.environmental_normalization
        || metadata.environmental_basis != route.environmental_basis
        || metadata.world_space_unrotated != route.stage_contract.world_space_unrotated
        || metadata.source_drive_applied != route.stage_contract.source_drive_applied
        || metadata.source_safety_gain_applied != route.stage_contract.source_safety_gain_applied
        || metadata.monitor_gain_applied != route.stage_contract.monitor_gain_applied
        || metadata.final_hrtf_applied != route.stage_contract.final_hrtf_applied
        || metadata.output_limiter_applied != route.stage_contract.output_limiter_applied
    {
        return false;
    }

    let mut active_feed_count = 0;
    for (plane_index, feed) in metadata.presentation_feeds.iter().enumerate() {
        if !feed.valid {
            continue;
        }
        let Some(component_slot) = feed.component.presentation_slot() else {
            return false;
        };
        if feed.source_index >= MAX_ACTIVE_SOURCES
            || component_slot >= route.bank_layout.presentation_components_per_source
            || route.bank_layout.presentation_component_slots[component_slot] != feed.component
            || feed.source_index * route.bank_layout.presentation_components_per_source
                + component_slot
                != plane_index
        {
            return false;
        }
        active_feed_count += 1;
    }
    active_feed_count == metadata.active_presentation_feed_count
}

fn normalize_inactive_neutral_banks(
    route: NeutralSwapRouteIdentity,
    presentation_bank: &mut [f32],
    environmental_bank: &mut [f32],
    metadata: &mut SpatialOutputMetadata,
) {
    let block_size = route.block_size_frames as usize;
    for (plane_index, feed) in metadata.presentation_feeds.iter_mut().enumerate() {
        if feed.valid {
            continue;
        }
        let start = plane_index * block_size;
        presentation_bank[start..start + block_size].fill(0.0);
        *feed = Default::default();
    }
    for plane_index in
        route.active_environmental_plane_count()..route.bank_layout.environmental_plane_count
    {
        let start = plane_index * block_size;
        environmental_bank[start..start + block_size].fill(0.0);
    }
}

fn write_neutral_union_metadata(
    old: &SpatialOutputMetadata,
    new: &SpatialOutputMetadata,
    output: &mut SpatialOutputMetadata,
) -> Result<(), SpatialBackendRenderError> {
    *output = *new;
    output.active_presentation_feed_count = 0;
    for plane_index in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
        let old_feed = old.presentation_feeds[plane_index];
        let new_feed = new.presentation_feeds[plane_index];
        output.presentation_feeds[plane_index] = match (old_feed.valid, new_feed.valid) {
            (false, false) => Default::default(),
            (true, false) => old_feed,
            (false, true) => new_feed,
            (true, true) if old_feed == new_feed => new_feed,
            (true, true) => return Err(SpatialBackendRenderError::InvalidOutputMetadata),
        };
        if output.presentation_feeds[plane_index].valid {
            output.active_presentation_feed_count += 1;
        }
    }
    Ok(())
}

fn blend_neutral_planes(
    old: &[f32],
    new: &[f32],
    output: &mut [f32],
    plane_count: usize,
    block_size: usize,
    fade_start_frame: usize,
    fade_frames: usize,
) {
    for plane_index in 0..plane_count {
        let plane_start = plane_index * block_size;
        for frame in 0..block_size {
            let new_gain = (fade_start_frame + frame) as f32 / (fade_frames - 1) as f32;
            let old_gain = 1.0 - new_gain;
            let sample_index = plane_start + frame;
            output[sample_index] = old[sample_index] * old_gain + new[sample_index] * new_gain;
        }
    }
}

/// Constructs the bounded control/audio pair around one active neutral graph.
#[allow(dead_code)]
pub(crate) fn build_neutral_swap_pair<G: SpatialBackendRenderGraph>(
    active_graph: G,
    route: NeutralSwapRouteIdentity,
) -> Result<(NeutralSwapControl<G>, NeutralSwapRenderGraph<G>), NeutralSwapError> {
    if !route.is_valid()
        || route.presentation_bank_samples().is_none()
        || route.environmental_bank_samples().is_none()
    {
        return Err(NeutralSwapError::InvalidRouteIdentity);
    }
    let presentation_samples = route.presentation_bank_samples().unwrap_or(0);
    let environmental_samples = route.environmental_bank_samples().unwrap_or(0);
    let (prepared_tx, prepared_rx) = world_swap::channel();
    let (retired_tx, retired_rx) = world_swap::channel();
    let lifecycle = Arc::new(AtomicU8::new(NEUTRAL_SWAP_IDLE));
    Ok((
        NeutralSwapControl {
            route,
            prepared: prepared_tx,
            retired: retired_rx,
            lifecycle: Arc::clone(&lifecycle),
        },
        NeutralSwapRenderGraph {
            active: NeutralSwapGeneration {
                graph: active_graph,
                route,
                warmup_blocks: 0,
            },
            prepared: prepared_rx,
            retired: retired_tx,
            lifecycle,
            warming: None,
            retiring: None,
            retirement_backlog: None,
            old_presentation_bank: vec![0.0; presentation_samples],
            new_presentation_bank: vec![0.0; presentation_samples],
            old_environmental_bank: vec![0.0; environmental_samples],
            new_environmental_bank: vec![0.0; environmental_samples],
            old_metadata: SpatialOutputMetadata::default(),
            new_metadata: SpatialOutputMetadata::default(),
        },
    ))
}

/// Control-side owner for a neutral spatial Steam generation and its bounded
/// render-thread swap channel.
pub(crate) struct NeutralMultiSourceSimulation {
    active: GenerationSimulation,
    active_world_offset_enu: fightbox_api::EnuVector3,
    retiring: Option<(GenerationSimulation, fightbox_api::EnuVector3)>,
    swap: NeutralSwapControl<GenerationNeutralRenderGraph>,
    route: NeutralSwapRouteIdentity,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: Vec<MultiSourceDescriptor>,
    program_channel_counts: Vec<usize>,
    environmental_order: usize,
    quality_tier: QualityTier,
    render_scratch_bytes: u64,
    audio_buffer_payload_bytes: u64,
    propagation_delay_line_bytes: u64,
    tracked_memory_adjustment_bytes: i64,
    swap_bank_payload_bytes: u64,
    tracked_memory_peak_bytes: AtomicU64,
}

pub(crate) type NeutralMultiSourceRenderGraph =
    NeutralSwapRenderGraph<GenerationNeutralRenderGraph>;

impl NeutralSwapRenderGraph<GenerationNeutralRenderGraph> {
    pub(crate) fn persistent_memory(&self) -> crate::SpatialRenderMemoryTelemetry {
        let mut memory = self.active.graph.persistent_memory();
        let swap_bank_bytes = self
            .old_presentation_bank
            .capacity()
            .saturating_add(self.new_presentation_bank.capacity())
            .saturating_add(self.old_environmental_bank.capacity())
            .saturating_add(self.new_environmental_bank.capacity())
            .saturating_mul(size_of::<f32>()) as u64;
        memory.rust_scratch_payload_bytes = memory
            .rust_scratch_payload_bytes
            .saturating_add(swap_bank_bytes);
        memory.total_tracked_payload_bytes = memory
            .total_tracked_payload_bytes
            .saturating_add(swap_bank_bytes);
        memory
    }
}

pub(crate) struct PreparedNeutralMultiSourceWorld {
    simulation: GenerationSimulation,
    render: GenerationNeutralRenderGraph,
    route: NeutralSwapRouteIdentity,
    descriptors: Vec<MultiSourceDescriptor>,
    world_offset_enu: fightbox_api::EnuVector3,
}

impl PreparedNeutralMultiSourceWorld {
    pub(crate) fn generation(&self) -> u64 {
        self.simulation.capabilities().generation
    }

    pub(crate) fn diagnostics(&self) -> crate::WorldGenerationDiagnostics {
        self.simulation.diagnostics()
    }

    pub(crate) fn prepare_simulation_for_realtime(
        &mut self,
        update: &SimulationUpdate,
    ) -> Result<(), SimulationError> {
        self.simulation.prepare_simulation_for_realtime(update)
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

fn neutral_memory_truth(
    simulation: &GenerationSimulation,
    memory: crate::SpatialRenderMemoryTelemetry,
) -> (u64, u64, u64, i64) {
    let render_scratch_bytes = memory
        .rust_scratch_payload_bytes
        .saturating_add(memory.outer_vec_payload_bytes);
    let audio_buffer_payload_bytes = memory.steam_audio_buffer_payload_bytes;
    let propagation_delay_line_bytes = memory
        .program_delay_audio_history_payload_bytes
        .saturating_add(memory.program_delay_geometry_history_payload_bytes)
        .saturating_add(crate::propagation_delay::bandlimited_kernel_payload_bytes());
    let estimated = simulation.quality_governor_telemetry().memory;
    let exact_total = render_scratch_bytes
        .saturating_add(audio_buffer_payload_bytes)
        .saturating_add(propagation_delay_line_bytes);
    let estimated_total = estimated
        .render_scratch_bytes
        .saturating_add(estimated.audio_buffer_payload_bytes)
        .saturating_add(estimated.propagation_delay_line_bytes);
    let delta = (i128::from(exact_total) - i128::from(estimated_total))
        .clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64;
    (
        render_scratch_bytes,
        audio_buffer_payload_bytes,
        propagation_delay_line_bytes,
        delta,
    )
}

fn apply_memory_delta(value: u64, delta: i64) -> u64 {
    if delta >= 0 {
        value.saturating_add(delta as u64)
    } else {
        value.saturating_sub(delta.unsigned_abs())
    }
}

fn neutral_world_offset(descriptors: &[MultiSourceDescriptor]) -> fightbox_api::EnuVector3 {
    descriptors
        .first()
        .copied()
        .map(MultiSourceDescriptor::metadata_city_offset)
        .unwrap_or_default()
}

fn translated_simulation_update(
    update: &SimulationUpdate,
    from_world_offset: fightbox_api::EnuVector3,
    to_world_offset: fightbox_api::EnuVector3,
) -> SimulationUpdate {
    let delta = fightbox_api::EnuVector3::new(
        from_world_offset.east_m - to_world_offset.east_m,
        from_world_offset.north_m - to_world_offset.north_m,
        from_world_offset.up_m - to_world_offset.up_m,
    );
    let mut translated = *update;
    translated.listener.pose.position.east_m += delta.east_m;
    translated.listener.pose.position.north_m += delta.north_m;
    translated.listener.pose.position.up_m += delta.up_m;
    for source in &mut translated.sources {
        source.pose.position.east_m += delta.east_m;
        source.pose.position.north_m += delta.north_m;
        source.pose.position.up_m += delta.up_m;
    }
    translated
}

impl NeutralMultiSourceSimulation {
    fn collect_retired(&mut self) {
        if self.swap.collect_retired() {
            self.retiring = None;
        }
    }

    pub(crate) fn cell_stream_lifecycle(&self) -> NeutralSwapLifecycle {
        self.swap.lifecycle()
    }

    fn retiring_simulation_accepts_control(&self) -> bool {
        matches!(
            self.swap.lifecycle(),
            NeutralSwapLifecycle::Publishing
                | NeutralSwapLifecycle::Prepared
                | NeutralSwapLifecycle::Crossfading
        )
    }

    pub(crate) fn collect_retired_world(&mut self) -> bool {
        let collected = self.swap.collect_retired();
        if collected {
            self.retiring = None;
        }
        collected
    }

    pub(crate) fn latest_direct_sequence(&self) -> u64 {
        self.active.latest_direct_sequence()
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

    pub(crate) fn quality_governor_telemetry(&self) -> crate::QualityGovernorTelemetry {
        let mut telemetry = self.active.quality_governor_telemetry();
        telemetry.memory.render_scratch_bytes = self.render_scratch_bytes;
        telemetry.memory.audio_buffer_payload_bytes = self.audio_buffer_payload_bytes;
        telemetry.memory.propagation_delay_line_bytes = self.propagation_delay_line_bytes;
        telemetry.memory.tracked_at_create_bytes = apply_memory_delta(
            telemetry.memory.tracked_at_create_bytes,
            self.tracked_memory_adjustment_bytes,
        );
        telemetry.memory.tracked_current_bytes = apply_memory_delta(
            telemetry.memory.tracked_current_bytes,
            self.tracked_memory_adjustment_bytes,
        );
        telemetry.memory.tracked_peak_bytes = apply_memory_delta(
            telemetry.memory.tracked_peak_bytes,
            self.tracked_memory_adjustment_bytes,
        );

        if let Some((retiring, _)) = &self.retiring {
            let retiring_telemetry = retiring.quality_governor_telemetry();
            let retiring_shared_bytes = self
                .swap_bank_payload_bytes
                .saturating_add(crate::propagation_delay::bandlimited_kernel_payload_bytes());
            let retiring_adjustment = self
                .tracked_memory_adjustment_bytes
                .saturating_sub(retiring_shared_bytes.min(i64::MAX as u64) as i64);
            let retiring_current = apply_memory_delta(
                retiring_telemetry.memory.tracked_current_bytes,
                retiring_adjustment,
            );
            let retiring_peak = apply_memory_delta(
                retiring_telemetry.memory.tracked_peak_bytes,
                retiring_adjustment,
            );
            telemetry.memory.render_scratch_bytes =
                telemetry.memory.render_scratch_bytes.saturating_add(
                    self.render_scratch_bytes
                        .saturating_sub(self.swap_bank_payload_bytes),
                );
            telemetry.memory.audio_buffer_payload_bytes = telemetry
                .memory
                .audio_buffer_payload_bytes
                .saturating_add(self.audio_buffer_payload_bytes);
            telemetry.memory.propagation_delay_line_bytes =
                telemetry
                    .memory
                    .propagation_delay_line_bytes
                    .saturating_add(self.propagation_delay_line_bytes.saturating_sub(
                        crate::propagation_delay::bandlimited_kernel_payload_bytes(),
                    ));
            telemetry.memory.snapshot_ring_payload_bytes = telemetry
                .memory
                .snapshot_ring_payload_bytes
                .saturating_add(retiring_telemetry.memory.snapshot_ring_payload_bytes);
            telemetry.memory.reflection_ir_payload_capacity_bytes = telemetry
                .memory
                .reflection_ir_payload_capacity_bytes
                .saturating_add(
                    retiring_telemetry
                        .memory
                        .reflection_ir_payload_capacity_bytes,
                );
            telemetry.memory.retained_bake_bytes = telemetry
                .memory
                .retained_bake_bytes
                .saturating_add(retiring_telemetry.memory.retained_bake_bytes);
            telemetry.memory.tracked_current_bytes = telemetry
                .memory
                .tracked_current_bytes
                .saturating_add(retiring_current);
            telemetry.memory.tracked_peak_bytes = telemetry
                .memory
                .tracked_peak_bytes
                .saturating_add(retiring_peak)
                .max(telemetry.memory.tracked_current_bytes);
        }
        let observed_peak = telemetry
            .memory
            .tracked_peak_bytes
            .max(telemetry.memory.tracked_current_bytes);
        let historical_peak = self
            .tracked_memory_peak_bytes
            .fetch_max(observed_peak, Ordering::Relaxed)
            .max(observed_peak);
        telemetry.memory.tracked_peak_bytes = historical_peak;
        telemetry
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

    pub(crate) fn update_inputs(&mut self, update: &SimulationUpdate) {
        self.collect_retired();
        self.active.update_inputs(update);
        let drive_retiring = self.retiring_simulation_accepts_control();
        if let Some((retiring, retiring_offset)) = &mut self.retiring
            && drive_retiring
        {
            let translated = translated_simulation_update(
                update,
                self.active_world_offset_enu,
                *retiring_offset,
            );
            retiring.update_inputs(&translated);
        }
    }

    pub(crate) fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        let active = self.active.run_direct();
        let retiring = if self.retiring_simulation_accepts_control() {
            self.retiring
                .as_mut()
                .map_or(Ok(()), |(simulation, _)| simulation.run_direct())
        } else {
            Ok(())
        };
        active.and(retiring)
    }

    pub(crate) fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        let active = self.active.run_pathing();
        let retiring = if self.retiring_simulation_accepts_control() {
            self.retiring
                .as_mut()
                .map_or(Ok(()), |(simulation, _)| simulation.run_pathing())
        } else {
            Ok(())
        };
        active.and(retiring)
    }

    pub(crate) fn run_reflections(&mut self) -> Result<(), SimulationError> {
        self.collect_retired();
        let active = self.active.run_reflections();
        let retiring = if self.retiring_simulation_accepts_control() {
            self.retiring
                .as_mut()
                .map_or(Ok(()), |(simulation, _)| simulation.run_reflections())
        } else {
            Ok(())
        };
        active.and(retiring)
    }

    pub(crate) fn prepare_simulation_for_realtime(
        &mut self,
        update: &SimulationUpdate,
    ) -> Result<(), SimulationError> {
        self.collect_retired();
        self.active.prepare_simulation_for_realtime(update)
    }

    pub(crate) fn prepare_world(
        &mut self,
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
    ) -> Result<PreparedNeutralMultiSourceWorld, BackendError> {
        self.prepare_world_with_descriptors(mesh, baked, self.descriptors.clone())
    }

    pub(crate) fn prepare_world_with_metadata_city_offset(
        &mut self,
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
        offset_enu: fightbox_api::EnuVector3,
    ) -> Result<PreparedNeutralMultiSourceWorld, BackendError> {
        let descriptors = self
            .descriptors
            .iter()
            .copied()
            .map(|descriptor| descriptor.with_metadata_city_offset(offset_enu))
            .collect();
        self.prepare_world_with_descriptors(mesh, baked, descriptors)
    }

    fn prepare_world_with_descriptors(
        &mut self,
        mesh: &SceneMesh,
        baked: &BakedProbeBatch,
        descriptors: Vec<MultiSourceDescriptor>,
    ) -> Result<PreparedNeutralMultiSourceWorld, BackendError> {
        self.collect_retired();
        if self.retiring.is_some() {
            return Err(BackendError::InvalidInput(
                "neutral locality preparation is refused until terminal tail retirement",
            ));
        }
        let (simulation, render) = build_neutral_multi_source_generation(
            mesh,
            Some(baked),
            self.audio,
            self.config,
            &descriptors,
            &self.program_channel_counts,
            self.environmental_order,
            next_world_generation(),
            self.quality_tier,
        )?;
        let world_offset_enu = neutral_world_offset(&descriptors);
        Ok(PreparedNeutralMultiSourceWorld {
            simulation,
            render,
            route: self.route,
            descriptors,
            world_offset_enu,
        })
    }

    pub(crate) fn swap_prepared_world(
        &mut self,
        prepared: PreparedNeutralMultiSourceWorld,
    ) -> Result<u64, NeutralSwapError> {
        self.swap_prepared_world_recoverable(prepared)
            .map_err(|(error, _prepared)| error)
    }

    pub(crate) fn swap_prepared_world_recoverable(
        &mut self,
        prepared: PreparedNeutralMultiSourceWorld,
    ) -> Result<u64, (NeutralSwapError, PreparedNeutralMultiSourceWorld)> {
        self.collect_retired();
        if self.retiring.is_some() {
            return Err((NeutralSwapError::AdoptionPending, prepared));
        }
        let PreparedNeutralMultiSourceWorld {
            mut simulation,
            render,
            route,
            descriptors,
            world_offset_enu,
        } = prepared;
        simulation.align_direct_sequence_for_world_swap(self.active.latest_direct_sequence());
        let generation = simulation.capabilities().generation;
        if let Err((error, render)) = self.swap.offer_prepared_recoverable(render, route) {
            return Err((
                error,
                PreparedNeutralMultiSourceWorld {
                    simulation,
                    render,
                    route,
                    descriptors,
                    world_offset_enu,
                },
            ));
        }
        let previous = core::mem::replace(&mut self.active, simulation);
        self.retiring = Some((previous, self.active_world_offset_enu));
        self.active_world_offset_enu = world_offset_enu;
        self.descriptors = descriptors;
        // Record the actual two-generation peak even if no host telemetry
        // sample lands inside the bounded residency interval.
        let _ = self.quality_governor_telemetry();
        Ok(generation)
    }
}

fn neutral_route_identity(
    audio: AudioConfig,
    environmental_order: usize,
) -> Result<NeutralSwapRouteIdentity, BackendError> {
    let order = match environmental_order {
        0 => SpatialAmbisonicOrder::Zero,
        1 => SpatialAmbisonicOrder::One,
        2 => SpatialAmbisonicOrder::Two,
        _ => {
            return Err(BackendError::InvalidInput(
                "neutral environmental order must be 0, 1, or 2",
            ));
        }
    };
    Ok(NeutralSwapRouteIdentity::new(
        audio.sample_rate_hz as u32,
        audio.frame_size as u32,
        SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
        order,
        order,
        0,
    ))
}

#[cfg(test)]
pub(crate) fn wrap_neutral_generation_for_test(
    simulation: GenerationSimulation,
    render: GenerationNeutralRenderGraph,
    config: S3SimulationConfig,
    descriptors: &[MultiSourceDescriptor],
    program_channel_counts: &[usize],
    environmental_order: usize,
    quality_tier: QualityTier,
) -> NeutralMultiSourceSimulation {
    let audio = simulation.audio_config();
    let route = neutral_route_identity(audio, environmental_order).expect("valid test route");
    let generation_memory = render.persistent_memory();
    let (swap, wrapped_render) =
        build_neutral_swap_pair(render, route).expect("valid test swap pair");
    let wrapped_memory = wrapped_render.persistent_memory();
    let swap_bank_payload_bytes = wrapped_memory
        .total_tracked_payload_bytes
        .saturating_sub(generation_memory.total_tracked_payload_bytes);
    let (
        render_scratch_bytes,
        audio_buffer_payload_bytes,
        propagation_delay_line_bytes,
        tracked_memory_adjustment_bytes,
    ) = neutral_memory_truth(&simulation, wrapped_memory);
    let tracked_memory_peak_bytes = apply_memory_delta(
        simulation
            .quality_governor_telemetry()
            .memory
            .tracked_peak_bytes,
        tracked_memory_adjustment_bytes,
    );
    NeutralMultiSourceSimulation {
        active: simulation,
        active_world_offset_enu: neutral_world_offset(descriptors),
        retiring: None,
        swap,
        route,
        audio,
        config,
        descriptors: descriptors.to_vec(),
        program_channel_counts: program_channel_counts.to_vec(),
        environmental_order,
        quality_tier,
        render_scratch_bytes,
        audio_buffer_payload_bytes,
        propagation_delay_line_bytes,
        tracked_memory_adjustment_bytes,
        swap_bank_payload_bytes,
        tracked_memory_peak_bytes: AtomicU64::new(tracked_memory_peak_bytes),
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

    pub(crate) fn observe_simulation_lateness(
        &mut self,
        pass: crate::GovernorSimulationPass,
        lateness_ns: u64,
    ) {
        self.simulation
            .observe_simulation_lateness(pass, lateness_ns);
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
    pub(crate) fn pin_replay_full_quality(&mut self) {
        self.active.pin_replay_full_quality();
    }

    pub(crate) fn enable_reflection_worker(&mut self, minimum_interval_ns: u64) -> Result<(), SimulationError> {
        self.active.enable_reflection_worker(minimum_interval_ns)
    }

    pub(crate) fn take_scene_air_writer(&mut self) -> Option<fightbox_runtime::SnapshotWriter<[f32; 3]>> {
        self.active.take_scene_air_writer()
    }

    pub(crate) fn prepare_simulation_for_realtime(
        &mut self,
        update: &SimulationUpdate,
    ) -> Result<(), SimulationError> {
        self.collect_retired();
        self.active.prepare_simulation_for_realtime(update)
    }

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
        let (mut simulation, render) = build_multi_source_generation(
            mesh,
            baked,
            self.active.audio_config(),
            config,
            descriptors,
            generation,
            self.active.quality_governor_telemetry().quality_tier,
        )?;
        if let Some(interval_ns) = self.active.reflection_worker_interval() {
            simulation.enable_reflection_worker(interval_ns)
                .map_err(|_| BackendError::InvalidInput("cannot start prepared reflection worker"))?;
        }
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
    pub(crate) fn into_spatial_export(
        self,
    ) -> Result<Box<dyn SpatialBackendRenderGraph>, BackendError> {
        // A file render owns one scene generation; prepared-world replacement
        // belongs to the live output session rather than an export stem.
        Ok(Box::new(multi_source::FullSpatialExportGraph::new(self.active)?))
    }

    pub(crate) fn scene_reset_control(&self) -> crate::SceneResetControl {
        self.active.scene_reset_control()
    }

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

    pub(crate) fn take_echo_trigger_control(
        &mut self,
    ) -> Option<(crate::EchoTrigger, EchoPlanWriter)> {
        self.active.take_echo_trigger_control()
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
        self.active.begin_tail_retirement();
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

    fn finish_retirement(&mut self) {
        let Some(retired) = self.retiring.take() else {
            return;
        };
        if let Err(retired) = self.retired.try_push(retired.graph) {
            self.retirement_backlog = Some(retired);
        }
    }

    pub(crate) fn render_block(
        &mut self,
        block: PropagationRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        if block.sources.len() > MAX_ACTIVE_SOURCES {
            return Err(BackendRenderError::InvalidSourceIndex);
        }
        let mut sources = [fightbox_runtime::backend::SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&[], &[]],
        }; MAX_ACTIVE_SOURCES];
        for (program, source) in sources.iter_mut().zip(block.sources) {
            *program = fightbox_runtime::backend::SpatialBackendSourceBlock {
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
        self.adopt_at_block_boundary();
        let Some(completed_blocks) = self
            .retiring
            .as_ref()
            .map(|retiring| retiring.completed_blocks)
        else {
            return self.active.render_program_block(block);
        };

        self.old_left.fill(0.0);
        self.old_right.fill(0.0);
        self.new_left.fill(0.0);
        self.new_right.fill(0.0);

        if completed_blocks >= WORLD_SWAP_FADE_BLOCKS {
            let tail_state = self
                .retiring
                .as_mut()
                .expect("retiring generation exists")
                .graph
                .render_retiring_tail(
                    block.listener_orientation,
                    &mut self.old_left,
                    &mut self.old_right,
                )?;
            self.active.render_program_block(fightbox_runtime::ProgramRenderBlock {
                listener_orientation: block.listener_orientation,
                sources: block.sources,
                output_left: &mut self.new_left,
                output_right: &mut self.new_right,
            })?;
            for frame in 0..self.old_left.len() {
                block.output_left[frame] += self.old_left[frame] + self.new_left[frame];
                block.output_right[frame] += self.old_right[frame] + self.new_right[frame];
            }
            self.delivered.store(
                encode_delivery(self.active.capabilities(), 0),
                Ordering::Release,
            );
            if tail_state == SpatialTailRetirementState::TailComplete {
                self.finish_retirement();
            }
            return Ok(());
        }

        self.retiring
            .as_mut()
            .expect("retiring generation exists")
            .graph
            .render_program_block(fightbox_runtime::ProgramRenderBlock {
                listener_orientation: block.listener_orientation,
                sources: block.sources,
                output_left: &mut self.old_left,
                output_right: &mut self.old_right,
            })?;
        self.active.render_program_block(fightbox_runtime::ProgramRenderBlock {
            listener_orientation: block.listener_orientation,
            sources: block.sources,
            output_left: &mut self.new_left,
            output_right: &mut self.new_right,
        })?;

        let frames = self.old_left.len();
        let fade_frames = frames * usize::from(WORLD_SWAP_FADE_BLOCKS);
        let start_frame = frames * usize::from(completed_blocks);
        for frame in 0..frames {
            let new_gain = (start_frame + frame) as f32 / (fade_frames - 1) as f32;
            let old_gain = 1.0 - new_gain;
            block.output_left[frame] +=
                self.old_left[frame] * old_gain + self.new_left[frame] * new_gain;
            block.output_right[frame] +=
                self.old_right[frame] * old_gain + self.new_right[frame] * new_gain;
        }

        let retiring = self.retiring.as_mut().expect("retiring generation exists");
        retiring.completed_blocks += 1;
        let remaining = WORLD_SWAP_FADE_BLOCKS - retiring.completed_blocks;
        let tail_state = retiring.graph.tail_retirement_state();
        self.delivered.store(
            encode_delivery(self.active.capabilities(), remaining),
            Ordering::Release,
        );
        if remaining == 0 && tail_state == SpatialTailRetirementState::TailComplete {
            self.finish_retirement();
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_spatial_multi_source_session(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    audio: AudioConfig,
    config: S3SimulationConfig,
    descriptors: &[MultiSourceDescriptor],
    program_channel_counts: &[usize],
    environmental_order: usize,
    quality_tier: QualityTier,
) -> Result<(NeutralMultiSourceSimulation, NeutralMultiSourceRenderGraph), BackendError> {
    let route = neutral_route_identity(audio, environmental_order)?;
    let (simulation, render) = build_neutral_multi_source_generation(
        mesh,
        Some(baked),
        audio,
        config,
        descriptors,
        program_channel_counts,
        environmental_order,
        next_world_generation(),
        quality_tier,
    )?;
    let generation_memory = render.persistent_memory();
    let (swap, render) = build_neutral_swap_pair(render, route).map_err(|_| {
        BackendError::InvalidInput("neutral swap route identity is incompatible with the graph")
    })?;
    let wrapped_memory = render.persistent_memory();
    let swap_bank_payload_bytes = wrapped_memory
        .total_tracked_payload_bytes
        .saturating_sub(generation_memory.total_tracked_payload_bytes);
    let (
        render_scratch_bytes,
        audio_buffer_payload_bytes,
        propagation_delay_line_bytes,
        tracked_memory_adjustment_bytes,
    ) = neutral_memory_truth(&simulation, wrapped_memory);
    let tracked_memory_peak_bytes = apply_memory_delta(
        simulation
            .quality_governor_telemetry()
            .memory
            .tracked_peak_bytes,
        tracked_memory_adjustment_bytes,
    );
    Ok((
        NeutralMultiSourceSimulation {
            active: simulation,
            active_world_offset_enu: neutral_world_offset(descriptors),
            retiring: None,
            swap,
            route,
            audio,
            config,
            descriptors: descriptors.to_vec(),
            program_channel_counts: program_channel_counts.to_vec(),
            environmental_order,
            quality_tier,
            render_scratch_bytes,
            audio_buffer_payload_bytes,
            propagation_delay_line_bytes,
            tracked_memory_adjustment_bytes,
            swap_bank_payload_bytes,
            tracked_memory_peak_bytes: AtomicU64::new(tracked_memory_peak_bytes),
        },
        render,
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

pub(crate) fn decode_ambix_binaural(samples: &[f32], audio: AudioConfig) -> Result<Vec<f32>, BackendError> {
    validate_audio(audio)?;
    if samples.len() % 9 != 0 || !samples.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::InvalidInput("AmbiX decode needs finite interleaved nine-channel PCM"));
    }
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate", status,
    })?;
    let mut audio_settings = raw_audio_settings(audio);
    let hrtf = Hrtf::create(&context, &mut audio_settings)?;
    let mut effect = AmbisonicsBinauralEffect::create(&context, &mut audio_settings, &hrtf, 2)?;
    let mut input = AudioBuffer::allocate(&context, 9, audio.frame_size)?;
    let mut output = AudioBuffer::allocate(&context, 2, audio.frame_size)?;
    let frames = audio.frame_size as usize;
    let total_frames = samples.len() / 9;
    let mut native = vec![0.0; frames * 9];
    let mut stereo = vec![0.0; frames * 2];
    let mut result = Vec::with_capacity(total_frames * 2);
    let mut params = ffi::IPLAmbisonicsBinauralEffectParams { hrtf: hrtf.raw(), order: 2 };
    let inverse_sphere_gain = 1.0 / (4.0 * std::f32::consts::PI).sqrt();
    let gains = [inverse_sphere_gain, inverse_sphere_gain * 3.0_f32.sqrt(), inverse_sphere_gain * 5.0_f32.sqrt()];
    for block in samples.chunks(frames * 9) {
        native.fill(0.0);
        for (source, target) in block.chunks_exact(9).zip(native.chunks_exact_mut(9)) {
            for channel in 0..9 {
                let order = if channel == 0 { 0 } else if channel < 4 { 1 } else { 2 };
                // Native Steam/Google SH includes (-1)^|m|; AmbiX omits it.
                let phase = if matches!(channel, 1 | 3 | 5 | 7) { -1.0 } else { 1.0 };
                target[channel] = source[channel] * gains[order] * phase;
            }
        }
        input.write_interleaved(&mut native);
        effect.apply(&mut params, &mut input, &mut output);
        output.read_interleaved(&mut stereo);
        result.extend_from_slice(&stereo[..block.len() / 9 * 2]);
    }
    if !result.iter().all(|sample| sample.is_finite()) {
        return Err(BackendError::NonFiniteOutput { output: "AmbiX binaural decode" });
    }
    Ok(result)
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

    fn probe(&self, index: u32) -> ffi::IPLSphere {
        ffi::probe_array_get_probe(self.raw(), index as i32)
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

    /// Builds one committed batch from generated spheres already in Steam
    /// coordinates, in their generated order.
    fn from_spheres(
        context: &'context Context,
        spheres: &[ffi::IPLSphere],
    ) -> Result<Self, BackendError> {
        let mut raw = core::ptr::null_mut();
        let status = ffi::probe_batch_create(context.raw(), &mut raw);
        sdk_status("iplProbeBatchCreate", status)?;
        let batch = Self {
            raw: non_null("iplProbeBatchCreate", status, raw)?,
            _context: PhantomData,
        };
        for sphere in spheres {
            ffi::probe_batch_add_probe(batch.raw(), *sphere);
        }
        ffi::probe_batch_commit(batch.raw());
        if batch.probe_count() != spheres.len() as i32 {
            return Err(BackendError::InvalidSdkOutput(
                "committed masked probe count differs from the kept count",
            ));
        }
        Ok(batch)
    }

    /// Builds one committed batch from the caller's ordered probe spheres.
    /// No generator, sorting, snapping, or deduplication occurs here.
    fn from_explicit(
        context: &'context Context,
        probes: &[ExplicitProbe],
    ) -> Result<Self, BackendError> {
        let mut raw = core::ptr::null_mut();
        let status = ffi::probe_batch_create(context.raw(), &mut raw);
        sdk_status("iplProbeBatchCreate", status)?;
        let batch = Self {
            raw: non_null("iplProbeBatchCreate", status, raw)?,
            _context: PhantomData,
        };
        for probe in probes {
            let center = enu_to_steam(probe.center_enu_m);
            ffi::probe_batch_add_probe(
                batch.raw(),
                ffi::IPLSphere {
                    center: ffi::IPLVector3 {
                        x: center.x,
                        y: center.y,
                        z: center.z,
                    },
                    radius: probe.radius_m,
                },
            );
        }
        ffi::probe_batch_commit(batch.raw());
        if batch.probe_count() != probes.len() as i32 {
            return Err(BackendError::InvalidSdkOutput(
                "committed explicit probe count differs from the submitted count",
            ));
        }
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
    bake_s3_inner(request, &ProbeMask::default(), None, None)
}

pub fn bake_s3_with_progress(
    request: &S3BakeRequest,
    on_progress: &(dyn Fn(f32) + Send + Sync),
) -> Result<BakedProbeBatch, BackendError> {
    bake_s3_inner(request, &ProbeMask::default(), Some(on_progress), None)
}

pub fn bake_s3_masked(
    request: &S3BakeRequest,
    mask: &ProbeMask,
    on_progress: Option<&(dyn Fn(f32) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<BakedProbeBatch, BackendError> {
    bake_s3_inner(request, mask, on_progress, should_cancel)
}

/// How often the cancel watcher polls. Unit tests poll fast so a cancel lands
/// inside a bake that lasts milliseconds.
const CANCEL_POLL: Duration = if cfg!(test) {
    Duration::from_millis(1)
} else {
    Duration::from_millis(200)
};

/// Runs the path bake, polling `should_cancel` on a scoped watcher thread and
/// cancelling the bake while it returns `true`. Returns the progress record
/// and whether a cancel was requested.
fn path_bake_watched(
    context: ffi::IPLContext,
    params: &mut ffi::IPLPathBakeParams,
    on_progress: Option<&(dyn Fn(f32) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Sync)>,
) -> (ffi::BakeProgress, bool) {
    let Some(should_cancel) = should_cancel else {
        return (ffi::path_baker_bake_with_progress(context, params, on_progress), false);
    };
    let cancelled = AtomicBool::new(false);
    let canceller = ffi::PathBakeCanceller::new(context);
    let progress = std::thread::scope(|scope| {
        let (done, finished) = std::sync::mpsc::channel::<()>();
        let (canceller, cancelled) = (&canceller, &cancelled);
        scope.spawn(move || {
            loop {
                // Steam ignores a cancel that arrives before its bake starts,
                // so repeat it on every positive poll until the bake returns.
                if should_cancel() {
                    cancelled.store(true, Ordering::SeqCst);
                    canceller.cancel();
                }
                if !matches!(
                    finished.recv_timeout(CANCEL_POLL),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    break;
                }
            }
        });
        let progress = ffi::path_baker_bake_with_progress(context, params, on_progress);
        drop(done);
        progress
    });
    (progress, cancelled.load(Ordering::SeqCst))
}

fn bake_s3_inner(
    request: &S3BakeRequest,
    mask: &ProbeMask,
    on_progress: Option<&(dyn Fn(f32) + Send + Sync)>,
    should_cancel: Option<&(dyn Fn() -> bool + Sync)>,
) -> Result<BakedProbeBatch, BackendError> {
    validate_bake_config(request)?;
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.mesh)?;
    let (probe_array, floor_probe_count) =
        ProbeArray::generate_uniform_floor(&context, &scene, request.probes)?;
    // An empty mask keeps the array route, so unmasked bakes stay byte-identical.
    let (probe_batch, floor_probe_count) = if mask.is_empty() {
        (ProbeBatch::from_array(&context, &probe_array)?, floor_probe_count)
    } else {
        let generated: Vec<ffi::IPLSphere> = (0..floor_probe_count)
            .map(|index| probe_array.probe(index))
            .collect();
        let centres: Vec<EnuVector3> = generated
            .iter()
            .map(|sphere| {
                steam_to_enu(SteamVector3 {
                    x: sphere.center.x,
                    y: sphere.center.y,
                    z: sphere.center.z,
                })
            })
            .collect();
        let selection = mask.compile(&request.mesh, request.probes).select(&centres);
        let kept: Vec<ffi::IPLSphere> = generated
            .iter()
            .zip(selection)
            .filter_map(|(sphere, scale)| {
                scale.map(|scale| ffi::IPLSphere {
                    center: sphere.center,
                    radius: sphere.radius * scale,
                })
            })
            .collect();
        if kept.is_empty() {
            return Err(BackendError::ProbeGenerationProducedNoProbes);
        }
        let count = kept.len() as u32;
        (ProbeBatch::from_spheres(&context, &kept)?, count)
    };
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
    let (progress, cancelled) =
        path_bake_watched(context.raw(), &mut bake_params, on_progress, should_cancel);
    // A cancelled bake leaves Steam's path data half-built: even
    // iplProbeBatchGetDataSize crashes on it. Return before touching it; the
    // batch still releases safely.
    if cancelled {
        return Err(BackendError::PathBakeCancelled);
    }
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

pub(crate) fn bake_explicit_probe_batch(
    request: &ExplicitProbeBakeRequest,
) -> Result<BakedProbeBatch, BackendError> {
    validate_mesh(&request.mesh)?;
    validate_path_bake_config(request.pathing)?;
    let context = Context::create().map_err(|status| BackendError::SdkCall {
        function: "iplContextCreate",
        status,
    })?;
    let scene = Scene::create_default(&context)?;
    let _static_mesh = StaticMesh::create_and_add(&scene, &request.mesh)?;
    let probe_batch = ProbeBatch::from_explicit(&context, &request.probes)?;

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
    let metadata = ProbeBatchMetadata {
        schema_version: PROBE_BATCH_METADATA_SCHEMA,
        steam_audio_version: STEAM_AUDIO_VERSION,
        upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
        probe_count: request.probes.len() as u32,
        path_data_size_bytes: path_data_size as u64,
        serialized_size_bytes: bytes.len() as u64,
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
    // A real operation can complete below the host clock's observable
    // resolution. Keep the sample present at the representable one-nanosecond
    // floor instead of intermittently reporting a false zero-duration call.
    u64::try_from(started.elapsed().as_nanos())
        .unwrap_or(u64::MAX)
        .max(1)
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

/// Steam's exponential air model carrying the shared ISO 9613-1 three-band
/// law, so direct, routed, and echo air share one timbre.
fn iso_air_absorption_model(exponents: [f32; 3]) -> ffi::IPLAirAbsorptionModel {
    ffi::IPLAirAbsorptionModel {
        type_: ffi::IPL_AIRABSORPTIONTYPE_EXPONENTIAL,
        coefficients: exponents,
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
    validate_path_bake_config(request.pathing)
}

fn validate_path_bake_config(config: PathBakeConfig) -> Result<(), BackendError> {
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

#[cfg(test)]
#[path = "neutral_swap_tests.rs"]
mod neutral_swap_tests;

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU32;

    /// A cancel issued once the visibility sweep reports 100% lands while
    /// Steam's bake is in progress, just before its path search. The bake
    /// returns without finishing the search (its path data is left unusable,
    /// so the test never queries it) and the batch still releases cleanly.
    #[test]
    fn a_cancel_at_the_path_search_stops_a_tiny_bake_and_releases_cleanly() {
        let request = S3BakeRequest {
            // Twelve probes in the open half of the corner.
            probes: ProbeVolume {
                min_enu_m: EnuVector3::new(-6.0, 2.0, 0.0),
                max_enu_m: EnuVector3::new(3.0, 8.0, 3.0),
                spacing_m: 3.0,
                height_above_floor_m: 1.5,
            },
            ..S3BakeRequest::default()
        };
        let context = Context::create().expect("context");
        let scene = Scene::create_default(&context).expect("scene");
        let _mesh = StaticMesh::create_and_add(&scene, &request.mesh).expect("mesh");
        let (probe_array, probe_count) =
            ProbeArray::generate_uniform_floor(&context, &scene, request.probes).expect("probes");
        let probe_batch = ProbeBatch::from_array(&context, &probe_array).expect("batch");
        let mut params = ffi::IPLPathBakeParams {
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
        let fractions = Mutex::new(Vec::new());
        let swept = AtomicBool::new(false);
        let polls = AtomicU32::new(0);
        // Hold Steam's thread at the end of the sweep long enough for the
        // 1 ms watcher to cancel the bake that is still in progress.
        let observer = |fraction: f32| {
            fractions.lock().unwrap().push(fraction);
            if fraction >= 1.0 && !swept.swap(true, Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(50));
            }
        };
        let gate = || {
            polls.fetch_add(1, Ordering::Relaxed);
            swept.load(Ordering::SeqCst)
        };
        let started = Instant::now();
        let (_, cancelled) =
            path_bake_watched(context.raw(), &mut params, Some(&observer), Some(&gate));
        let elapsed = started.elapsed();
        let fractions = fractions.into_inner().unwrap();
        eprintln!(
            "{probe_count} probes, cancelled {cancelled} after {elapsed:?} and {} poll(s); progress {fractions:?}",
            polls.load(Ordering::Relaxed)
        );
        assert!(cancelled && swept.load(Ordering::SeqCst));
        drop(probe_batch);
        eprintln!("released the cancelled batch");
    }
}
