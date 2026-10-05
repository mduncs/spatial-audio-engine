use super::*;
use crate::propagation_delay_stereo_tests::count_allocations;
use fightbox_api::{EnuVector3 as ApiEnuVector3, Pose};
use fightbox_runtime::backend::{
    SpatialBackendSourceBlock, SpatialFeedPlacement, SpatialPresentationFeedMetadata,
};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

thread_local! {
    static INSIDE_NEUTRAL_CALLBACK: Cell<bool> = const { Cell::new(false) };
}

const ALL_PRESENTATION_FEEDS: u64 = (1_u64 << MAX_SPATIAL_PRESENTATION_FEEDS) - 1;

#[test]
fn city_update_translation_keeps_one_control_pose_across_retained_worlds() {
    use fightbox_runtime::backend::{SimulationUpdate, SourceMotion};

    let pose = |position| Pose {
        position,
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    };
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: pose(ApiEnuVector3::new(-243.0, 242.0, 1.5)),
        linear_velocity_mps: ApiEnuVector3::new(0.125, 0.0, 0.0),
    };
    let east = SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: pose(ApiEnuVector3::new(-241.5, 192.5, 1.5)),
            linear_velocity_mps: ApiEnuVector3::new(0.125, 0.0, 0.0),
        },
        sources,
    };
    let west = translated_simulation_update(
        &east,
        ApiEnuVector3::new(485.0, 0.0, 0.0),
        ApiEnuVector3::default(),
    );
    assert_eq!(
        west.listener.pose.position,
        ApiEnuVector3::new(243.5, 192.5, 1.5)
    );
    assert_eq!(
        west.sources[0].pose.position,
        ApiEnuVector3::new(242.0, 242.0, 1.5)
    );
    assert_eq!(
        west.sources[0].linear_velocity_mps,
        east.sources[0].linear_velocity_mps
    );
}

#[derive(Default)]
struct MockObservations {
    prepare_calls: AtomicUsize,
    render_calls: AtomicUsize,
    drops: AtomicUsize,
    callback_drops: AtomicUsize,
}

struct MockNeutralGraph {
    generation: u64,
    reported_route: NeutralSwapRouteIdentity,
    presentation_values: [f32; MAX_SPATIAL_PRESENTATION_FEEDS],
    environmental_values: [f32; MAX_SPATIAL_ENVIRONMENT_PLANES],
    feeds: [SpatialPresentationFeedMetadata; MAX_SPATIAL_PRESENTATION_FEEDS],
    preparation_error: Option<SpatialBackendRenderError>,
    backend_error: Option<SpatialBackendRenderError>,
    discontinuity_offset: u64,
    retiring_tail_blocks: usize,
    retiring_tail_value: f32,
    warmup_blocks: u8,
    observations: Arc<MockObservations>,
}

impl MockNeutralGraph {
    fn new(
        generation: u64,
        route: NeutralSwapRouteIdentity,
        presentation_seed: f32,
        environmental_seed: f32,
        feed_mask: u64,
        pose_bias: f32,
    ) -> (Self, Arc<MockObservations>) {
        let observations = Arc::new(MockObservations::default());
        (
            Self {
                generation,
                reported_route: route,
                presentation_values: std::array::from_fn(|plane| presentation_seed + plane as f32),
                environmental_values: std::array::from_fn(|plane| {
                    environmental_seed + plane as f32
                }),
                feeds: presentation_feeds(feed_mask, pose_bias),
                preparation_error: None,
                backend_error: None,
                discontinuity_offset: 0,
                retiring_tail_blocks: 0,
                retiring_tail_value: 0.0,
                warmup_blocks: 0,
                observations: Arc::clone(&observations),
            },
            observations,
        )
    }

    fn with_warmup(mut self, warmup_blocks: u8) -> Self {
        self.warmup_blocks = warmup_blocks;
        self
    }
}

impl Drop for MockNeutralGraph {
    fn drop(&mut self) {
        self.observations
            .drops
            .fetch_add(1, AtomicOrdering::Relaxed);
        INSIDE_NEUTRAL_CALLBACK.with(|inside| {
            if inside.get() {
                self.observations
                    .callback_drops
                    .fetch_add(1, AtomicOrdering::Relaxed);
            }
        });
    }
}

impl SpatialBackendRenderGraph for MockNeutralGraph {
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        self.observations
            .prepare_calls
            .fetch_add(1, AtomicOrdering::Relaxed);
        self.preparation_error.map_or(Ok(()), Err)
    }

    fn pre_crossfade_warmup_blocks(&self) -> u8 {
        self.warmup_blocks
    }

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        self.observations
            .render_calls
            .fetch_add(1, AtomicOrdering::Relaxed);
        if let Some(error) = self.backend_error {
            return Err(error);
        }

        let block_size = self.reported_route.block_size_frames as usize;
        if block.presentation_bank.len()
            != MAX_SPATIAL_PRESENTATION_FEEDS.saturating_mul(block_size)
            || block.environmental_bank.len()
                != MAX_SPATIAL_ENVIRONMENT_PLANES.saturating_mul(block_size)
        {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }
        for plane in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
            let start = plane * block_size;
            block.presentation_bank[start..start + block_size]
                .fill(self.presentation_values[plane]);
        }
        for plane in 0..MAX_SPATIAL_ENVIRONMENT_PLANES {
            let start = plane * block_size;
            block.environmental_bank[start..start + block_size]
                .fill(self.environmental_values[plane]);
        }

        let discontinuity_sequence = block
            .metadata
            .discontinuity_sequence
            .saturating_add(self.discontinuity_offset);
        let active_presentation_feed_count = self.feeds.iter().filter(|feed| feed.valid).count();
        *block.metadata = SpatialOutputMetadata {
            sample_rate_hz: self.reported_route.sample_rate_hz,
            block_size_frames: self.reported_route.block_size_frames,
            block_start_frame: block.block_start_frame,
            validity: SpatialOutputValidity::Valid,
            generation: self.generation,
            discontinuity_sequence,
            active_presentation_feed_count,
            active_environmental_order: self.reported_route.active_environmental_order,
            active_environmental_plane_count: self
                .reported_route
                .active_environmental_plane_count(),
            environmental_latency_frames: self.reported_route.environmental_latency_frames,
            environmental_channel_order: self
                .reported_route
                .bank_layout
                .environmental_channel_order,
            environmental_normalization: self
                .reported_route
                .bank_layout
                .environmental_normalization,
            environmental_basis: self.reported_route.environmental_basis,
            world_space_unrotated: self.reported_route.stage_contract.world_space_unrotated,
            source_drive_applied: self.reported_route.stage_contract.source_drive_applied,
            source_safety_gain_applied: self
                .reported_route
                .stage_contract
                .source_safety_gain_applied,
            monitor_gain_applied: self.reported_route.stage_contract.monitor_gain_applied,
            final_hrtf_applied: self.reported_route.stage_contract.final_hrtf_applied,
            output_limiter_applied: self.reported_route.stage_contract.output_limiter_applied,
            presentation_feeds: self.feeds,
        };
        Ok(())
    }

    fn tail_retirement_state(&self) -> SpatialTailRetirementState {
        if self.retiring_tail_blocks == 0 {
            SpatialTailRetirementState::TailComplete
        } else {
            SpatialTailRetirementState::TailRemaining
        }
    }

    fn render_retiring_environmental_tail(
        &mut self,
        environmental_bank: &mut [f32],
    ) -> Result<SpatialTailRetirementState, SpatialBackendRenderError> {
        environmental_bank.fill(self.retiring_tail_value);
        self.retiring_tail_blocks = self.retiring_tail_blocks.saturating_sub(1);
        Ok(self.tail_retirement_state())
    }
}

#[test]
fn offered_generation_is_prepared_before_publish_and_prepare_failure_is_not_adopted() {
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (active, _) = MockNeutralGraph::new(1, route, 1.0, 2.0, 1, 0.0);
    let active = prepare_initial_active(active);
    let (mut control, mut render) = build_neutral_swap_pair(active, route).unwrap();

    let (mut rejected, rejected_observations) = MockNeutralGraph::new(2, route, 3.0, 4.0, 1, 0.0);
    rejected.preparation_error = Some(SpatialBackendRenderError::InactiveGraph);
    assert_eq!(
        control.offer_prepared(rejected, route),
        Err(NeutralSwapError::PreparationFailed(
            SpatialBackendRenderError::InactiveGraph
        ))
    );
    assert_eq!(
        rejected_observations
            .prepare_calls
            .load(AtomicOrdering::Relaxed),
        1
    );
    assert_eq!(
        rejected_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        0
    );
    assert_eq!(rejected_observations.drops.load(AtomicOrdering::Relaxed), 1);

    let (candidate, candidate_observations) = MockNeutralGraph::new(3, route, 5.0, 6.0, 1, 0.0);
    control.offer_prepared(candidate, route).unwrap();
    assert_eq!(
        candidate_observations
            .prepare_calls
            .load(AtomicOrdering::Relaxed),
        1
    );
    assert_eq!(
        candidate_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        0
    );

    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut metadata = caller_metadata(route, 0, 0);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.generation, 3);
    assert_eq!(
        candidate_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        1
    );
}

#[test]
fn eight_block_transition_has_exact_endpoint_gains_across_every_fixed_plane() {
    let route = test_route(4, SpatialAmbisonicOrder::Two, SpatialAmbisonicOrder::Two);
    let (old, old_observations) =
        MockNeutralGraph::new(11, route, 10.0, 100.0, ALL_PRESENTATION_FEEDS, 0.0);
    let (new, new_observations) =
        MockNeutralGraph::new(12, route, 50.0, 500.0, ALL_PRESENTATION_FEEDS, 0.0);
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    assert_preallocated_bank_shapes(&render, route);
    control.offer_prepared(new, route).unwrap();

    let block_size = route.block_size_frames as usize;
    let fade_frames = block_size * usize::from(NEUTRAL_SWAP_FADE_BLOCKS);
    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut first_transition_sample = f32::NAN;
    for block_index in 0..usize::from(NEUTRAL_SWAP_FADE_BLOCKS) {
        presentation.fill(-91.0);
        environment.fill(-73.0);
        let mut metadata = caller_metadata(route, (block_index * block_size) as u64, 77);
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        if block_index == 0 {
            first_transition_sample = presentation[0];
        }

        for plane in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
            for frame in 0..block_size {
                let fade_frame = block_index * block_size + frame;
                let new_gain = fade_frame as f32 / (fade_frames - 1) as f32;
                let old_gain = 1.0 - new_gain;
                let expected = (10.0 + plane as f32) * old_gain + (50.0 + plane as f32) * new_gain;
                assert_eq!(
                    presentation[plane * block_size + frame].to_bits(),
                    expected.to_bits(),
                    "presentation plane {plane}, fade frame {fade_frame}"
                );
            }
        }
        for plane in 0..MAX_SPATIAL_ENVIRONMENT_PLANES {
            for frame in 0..block_size {
                let fade_frame = block_index * block_size + frame;
                let new_gain = fade_frame as f32 / (fade_frames - 1) as f32;
                let old_gain = 1.0 - new_gain;
                let expected =
                    (100.0 + plane as f32) * old_gain + (500.0 + plane as f32) * new_gain;
                assert_eq!(
                    environment[plane * block_size + frame].to_bits(),
                    expected.to_bits(),
                    "ACN plane {plane}, fade frame {fade_frame}"
                );
            }
        }
        assert_eq!(metadata.generation, 12);
        assert_eq!(metadata.discontinuity_sequence, 77);
        assert_eq!(
            metadata.active_presentation_feed_count,
            MAX_SPATIAL_PRESENTATION_FEEDS
        );
        assert_eq!(
            metadata.active_environmental_plane_count,
            MAX_SPATIAL_ENVIRONMENT_PLANES
        );
    }

    assert_eq!(first_transition_sample.to_bits(), 10.0_f32.to_bits());
    assert_eq!(
        presentation[MAX_SPATIAL_PRESENTATION_FEEDS * block_size - 1].to_bits(),
        (50.0 + (MAX_SPATIAL_PRESENTATION_FEEDS - 1) as f32).to_bits()
    );
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 0);
    assert_eq!(
        old_observations
            .callback_drops
            .load(AtomicOrdering::Relaxed),
        0
    );
    assert!(control.collect_retired());
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(new_observations.drops.load(AtomicOrdering::Relaxed), 0);
}

#[test]
fn union_metadata_keeps_old_only_slots_through_the_last_fade_block() {
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let old_mask = (1_u64 << 0) | (1_u64 << 1);
    let new_mask = (1_u64 << 1) | (1_u64 << 2);
    let (old, _) = MockNeutralGraph::new(21, route, 10.0, 100.0, old_mask, 0.0);
    let (new, _) = MockNeutralGraph::new(22, route, 50.0, 500.0, new_mask, 0.0);
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    control.offer_prepared(new, route).unwrap();
    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];

    for block_index in 0..usize::from(NEUTRAL_SWAP_FADE_BLOCKS) {
        let mut metadata = caller_metadata(route, (block_index * 4) as u64, 91);
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert_eq!(metadata.generation, 22);
        assert_eq!(metadata.discontinuity_sequence, 91);
        assert_eq!(metadata.active_presentation_feed_count, 3);
        for plane in 0..3 {
            assert!(metadata.presentation_feeds[plane].valid);
            assert_eq!(
                metadata.presentation_feeds[plane].source_index
                    * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE
                    + metadata.presentation_feeds[plane]
                        .component
                        .presentation_slot()
                        .unwrap(),
                plane
            );
        }
        for plane in 3..MAX_SPATIAL_PRESENTATION_FEEDS {
            assert_eq!(metadata.presentation_feeds[plane], Default::default());
            assert!(
                presentation[plane * 4..plane * 4 + 4]
                    .iter()
                    .all(|sample| sample.to_bits() == 0)
            );
        }
        for plane in 4..MAX_SPATIAL_ENVIRONMENT_PLANES {
            assert!(
                environment[plane * 4..plane * 4 + 4]
                    .iter()
                    .all(|sample| sample.to_bits() == 0)
            );
        }
    }

    let mut metadata = caller_metadata(route, 32, 91);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.active_presentation_feed_count, 2);
    assert_eq!(metadata.presentation_feeds[0], Default::default());
    assert!(presentation[..4].iter().all(|sample| sample.to_bits() == 0));
    assert!(metadata.presentation_feeds[1].valid);
    assert!(metadata.presentation_feeds[2].valid);
}

#[test]
fn incompatible_or_invalid_routes_are_rejected_before_the_callback() {
    let route = test_route(8, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (active, active_observations) = MockNeutralGraph::new(31, route, 1.0, 2.0, 1, 0.0);
    let (mut control, _render) = build_neutral_swap_pair(active, route).unwrap();

    let mut incompatible = Vec::new();
    let mut changed = route;
    changed.sample_rate_hz += 1;
    incompatible.push(changed);
    changed = route;
    changed.block_size_frames += 1;
    incompatible.push(changed);
    changed = route;
    changed.environmental_basis = SpatialEnvironmentalBasis::RightHandedEnu;
    incompatible.push(changed);
    changed = route;
    changed.active_environmental_order = SpatialAmbisonicOrder::Zero;
    incompatible.push(changed);
    changed = route;
    changed.requested_environmental_order = SpatialAmbisonicOrder::One;
    incompatible.push(changed);
    changed = route;
    changed.environmental_latency_frames += 1;
    incompatible.push(changed);

    for (index, incompatible_route) in incompatible.into_iter().enumerate() {
        let (candidate, observations) =
            MockNeutralGraph::new(40 + index as u64, route, 3.0, 4.0, 1, 0.0);
        assert_eq!(
            control.offer_prepared(candidate, incompatible_route),
            Err(NeutralSwapError::IncompatibleRouteIdentity)
        );
        assert_eq!(observations.render_calls.load(AtomicOrdering::Relaxed), 0);
        assert_eq!(observations.drops.load(AtomicOrdering::Relaxed), 1);
        assert_eq!(observations.callback_drops.load(AtomicOrdering::Relaxed), 0);
    }

    let mut invalid = route;
    invalid.bank_layout.presentation_plane_count -= 1;
    let (candidate, observations) = MockNeutralGraph::new(50, route, 3.0, 4.0, 1, 0.0);
    assert_eq!(
        control.offer_prepared(candidate, invalid),
        Err(NeutralSwapError::InvalidRouteIdentity)
    );
    assert_eq!(observations.render_calls.load(AtomicOrdering::Relaxed), 0);
    assert_eq!(observations.drops.load(AtomicOrdering::Relaxed), 1);
    assert_eq!(
        active_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        0
    );
}

#[test]
fn one_prepared_maximum_and_no_third_graph_while_retiring() {
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (old, old_observations) = MockNeutralGraph::new(61, route, 1.0, 2.0, 1, 0.0);
    let (prepared, _) = MockNeutralGraph::new(62, route, 3.0, 4.0, 1, 0.0);
    let (queued_too_early, queued_observations) =
        MockNeutralGraph::new(63, route, 5.0, 6.0, 1, 0.0);
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    control.offer_prepared(prepared, route).unwrap();
    assert_eq!(
        control.offer_prepared(queued_too_early, route),
        Err(NeutralSwapError::AdoptionPending)
    );
    assert_eq!(queued_observations.drops.load(AtomicOrdering::Relaxed), 1);

    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut metadata = caller_metadata(route, 0, 7);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    let (third, third_observations) = MockNeutralGraph::new(64, route, 7.0, 8.0, 1, 0.0);
    assert_eq!(
        control.offer_prepared(third, route),
        Err(NeutralSwapError::AdoptionPending)
    );
    assert_eq!(third_observations.drops.load(AtomicOrdering::Relaxed), 1);

    for block_index in 1..usize::from(NEUTRAL_SWAP_FADE_BLOCKS) {
        metadata = caller_metadata(route, (block_index * 4) as u64, 7);
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
    }
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 0);

    let (next, _) = MockNeutralGraph::new(65, route, 9.0, 10.0, 1, 0.0);
    control.offer_prepared(next, route).unwrap();
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 1);
    metadata = caller_metadata(route, 32, 7);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.generation, 65);
}

#[test]
fn admitted_environmental_tail_survives_after_direct_path_crossfade() {
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (mut old, old_observations) = MockNeutralGraph::new(66, route, 1.0, 10.0, 1, 0.0);
    old.retiring_tail_blocks = 3;
    old.retiring_tail_value = 0.25;
    let (new, _) = MockNeutralGraph::new(67, route, 5.0, 20.0, 1, 0.0);
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    control.offer_prepared(new, route).unwrap();
    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];

    for block_index in 0..usize::from(NEUTRAL_SWAP_FADE_BLOCKS) {
        let mut metadata = caller_metadata(route, (block_index * 4) as u64, 9);
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
    }
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 0);
    let (blocked, blocked_observations) = MockNeutralGraph::new(68, route, 7.0, 30.0, 1, 0.0);
    assert_eq!(
        control.offer_prepared(blocked, route),
        Err(NeutralSwapError::AdoptionPending)
    );
    assert_eq!(blocked_observations.drops.load(AtomicOrdering::Relaxed), 1);

    for tail_block in 0..3 {
        presentation.fill(0.0);
        environment.fill(0.0);
        let mut metadata = caller_metadata(
            route,
            (usize::from(NEUTRAL_SWAP_FADE_BLOCKS) * 4 + tail_block * 4) as u64,
            9,
        );
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert_eq!(presentation[0].to_bits(), 5.0_f32.to_bits());
        assert_eq!(environment[0].to_bits(), 20.25_f32.to_bits());
        assert_eq!(metadata.generation, 67);
    }
    assert!(control.collect_retired());
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 1);
}

#[test]
fn backend_and_metadata_errors_propagate_without_advancing_the_fade() {
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (old, old_observations) = MockNeutralGraph::new(71, route, 1.0, 2.0, 1, 0.0);
    let (mut new, new_observations) = MockNeutralGraph::new(72, route, 3.0, 4.0, 1, 0.0);
    new.backend_error = Some(SpatialBackendRenderError::InactiveGraph);
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    control.offer_prepared(new, route).unwrap();
    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut metadata = caller_metadata(route, 0, 19);
    assert_eq!(
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        ),
        Err(SpatialBackendRenderError::InactiveGraph)
    );
    assert_eq!(render.retiring.as_ref().unwrap().completed_blocks, 0);
    assert_eq!(
        old_observations.render_calls.load(AtomicOrdering::Relaxed),
        1
    );
    assert_eq!(
        new_observations.render_calls.load(AtomicOrdering::Relaxed),
        1
    );

    render.active.graph.backend_error = None;
    presentation.fill(0.0);
    environment.fill(0.0);
    metadata = caller_metadata(route, 4, 19);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(presentation[0].to_bits(), 1.0_f32.to_bits());
    assert_eq!(render.retiring.as_ref().unwrap().completed_blocks, 1);

    render.active.graph.feeds[0].pose_enu.position.east_m += 1.0;
    metadata = caller_metadata(route, 8, 19);
    assert_eq!(
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        ),
        Err(SpatialBackendRenderError::InvalidOutputMetadata)
    );
    assert_eq!(render.retiring.as_ref().unwrap().completed_blocks, 1);

    render.active.graph.feeds[0] = render.retiring.as_ref().unwrap().generation.graph.feeds[0];
    render.active.graph.discontinuity_offset = 1;
    metadata = caller_metadata(route, 12, 19);
    assert_eq!(
        render_with_callback_marker(
            &mut render,
            &mut presentation,
            &mut environment,
            &mut metadata,
        ),
        Err(SpatialBackendRenderError::InvalidOutputMetadata)
    );
    assert_eq!(render.retiring.as_ref().unwrap().completed_blocks, 1);
}

#[test]
fn repeated_swaps_render_with_zero_allocations_and_stable_storage() {
    let route = test_route(8, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (old, old_observations) = MockNeutralGraph::new(80, route, 1.0, 2.0, 0b111, 0.0);
    let mut prepared = (81..85)
        .map(|generation| {
            MockNeutralGraph::new(
                generation,
                route,
                generation as f32,
                generation as f32 + 100.0,
                0b111,
                0.0,
            )
            .0
        })
        .collect::<Vec<_>>()
        .into_iter();
    let old = prepare_initial_active(old);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    let initial_pointers = scratch_pointers(&render);
    let initial_capacities = scratch_capacities(&render);
    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut metadata = caller_metadata(route, 0, 101);

    // Initialize the callback marker's TLS before allocation tracking begins.
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    let allocations = count_allocations(|| {
        let mut block_start = route.block_size_frames as u64;
        for _ in 0..4 {
            control
                .offer_prepared(prepared.next().unwrap(), route)
                .unwrap();
            for _ in 0..NEUTRAL_SWAP_FADE_BLOCKS {
                presentation.fill(0.0);
                environment.fill(0.0);
                metadata = caller_metadata(route, block_start, 101);
                render_with_callback_marker(
                    &mut render,
                    &mut presentation,
                    &mut environment,
                    &mut metadata,
                )
                .unwrap();
                block_start += route.block_size_frames as u64;
            }
            assert!(control.collect_retired());
        }
    });

    assert_eq!(allocations, 0);
    assert_eq!(scratch_pointers(&render), initial_pointers);
    assert_eq!(scratch_capacities(&render), initial_capacities);
    assert_eq!(
        old_observations
            .callback_drops
            .load(AtomicOrdering::Relaxed),
        0
    );
    assert_eq!(old_observations.drops.load(AtomicOrdering::Relaxed), 1);
}

#[test]
fn backend_warmup_keeps_old_output_then_starts_fade_without_callback_storage_changes() {
    const WARMUP_BLOCKS: usize = 3;
    let route = test_route(4, SpatialAmbisonicOrder::One, SpatialAmbisonicOrder::Two);
    let (old, old_observations) = MockNeutralGraph::new(90, route, 1.0, 2.0, 1, 0.0);
    let old = prepare_initial_active(old);
    let (candidate, candidate_observations) = MockNeutralGraph::new(91, route, 5.0, 6.0, 1, 0.0);
    let candidate = candidate.with_warmup(WARMUP_BLOCKS as u8);
    let (mut control, mut render) = build_neutral_swap_pair(old, route).unwrap();
    let initial_pointers = scratch_pointers(&render);
    let initial_capacities = scratch_capacities(&render);
    control.offer_prepared(candidate, route).unwrap();

    let mut presentation = vec![0.0; route.presentation_bank_samples().unwrap()];
    let mut environment = vec![0.0; route.environmental_bank_samples().unwrap()];
    let mut metadata = caller_metadata(route, 0, 23);
    let allocations = count_allocations(|| {
        for block in 0..WARMUP_BLOCKS {
            presentation.fill(0.0);
            environment.fill(0.0);
            metadata = caller_metadata(route, (block * 4) as u64, 23);
            render_with_callback_marker(
                &mut render,
                &mut presentation,
                &mut environment,
                &mut metadata,
            )
            .unwrap();
            assert_eq!(control.lifecycle(), NeutralSwapLifecycle::Prepared);
            assert!(render.retiring.is_none());
            assert_eq!(presentation[0].to_bits(), 1.0_f32.to_bits());
            assert_eq!(environment[0].to_bits(), 2.0_f32.to_bits());
        }
    });
    assert_eq!(allocations, 0, "warmup callback allocated");
    assert_eq!(
        candidate_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        WARMUP_BLOCKS
    );
    assert_eq!(
        old_observations.render_calls.load(AtomicOrdering::Relaxed),
        WARMUP_BLOCKS
    );
    assert_eq!(render.new_presentation_bank[0].to_bits(), 5.0_f32.to_bits());
    assert_eq!(scratch_pointers(&render), initial_pointers);
    assert_eq!(scratch_capacities(&render), initial_capacities);

    presentation.fill(0.0);
    environment.fill(0.0);
    metadata = caller_metadata(route, (WARMUP_BLOCKS * 4) as u64, 23);
    render_with_callback_marker(
        &mut render,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(control.lifecycle(), NeutralSwapLifecycle::Crossfading);
    assert_eq!(render.retiring.as_ref().unwrap().completed_blocks, 1);
    assert_eq!(
        candidate_observations
            .render_calls
            .load(AtomicOrdering::Relaxed),
        WARMUP_BLOCKS + 1
    );
    assert!(presentation[route.block_size_frames as usize - 1] > 1.0);
}

fn prepare_initial_active(mut graph: MockNeutralGraph) -> MockNeutralGraph {
    graph.prepare_for_realtime().unwrap();
    graph
}

fn test_route(
    block_size_frames: u32,
    active_order: SpatialAmbisonicOrder,
    requested_order: SpatialAmbisonicOrder,
) -> NeutralSwapRouteIdentity {
    NeutralSwapRouteIdentity::new(
        48_000,
        block_size_frames,
        SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
        active_order,
        requested_order,
        7,
    )
}

fn presentation_feeds(
    valid_mask: u64,
    pose_bias: f32,
) -> [SpatialPresentationFeedMetadata; MAX_SPATIAL_PRESENTATION_FEEDS] {
    std::array::from_fn(|plane| {
        if valid_mask & (1_u64 << plane) == 0 {
            return Default::default();
        }
        let source_index = plane / MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE;
        let component = match plane % MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE {
            0 => SpatialPresentationComponent::DirectCenter,
            1 => SpatialPresentationComponent::WidthPositive,
            _ => SpatialPresentationComponent::WidthNegative,
        };
        SpatialPresentationFeedMetadata {
            valid: true,
            source_index,
            component,
            placement: SpatialFeedPlacement::Pose,
            pose_enu: Pose {
                position: ApiEnuVector3::new(source_index as f32 + pose_bias, 2.0, 3.0),
                forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                up: ApiEnuVector3::new(0.0, 0.0, 1.0),
            },
            direction_enu: ApiEnuVector3::default(),
            latency_frames: component.presentation_slot().unwrap() as u32,
        }
    })
}

fn caller_metadata(
    route: NeutralSwapRouteIdentity,
    block_start_frame: u64,
    discontinuity_sequence: u64,
) -> SpatialOutputMetadata {
    SpatialOutputMetadata {
        sample_rate_hz: route.sample_rate_hz,
        block_size_frames: route.block_size_frames,
        block_start_frame,
        discontinuity_sequence,
        ..SpatialOutputMetadata::default()
    }
}

fn render_with_callback_marker(
    render: &mut NeutralSwapRenderGraph<MockNeutralGraph>,
    presentation_bank: &mut [f32],
    environmental_bank: &mut [f32],
    metadata: &mut SpatialOutputMetadata,
) -> Result<(), SpatialBackendRenderError> {
    INSIDE_NEUTRAL_CALLBACK.with(|inside| inside.set(true));
    let result = render.render_spatial_block(SpatialPropagationRenderBlock {
        block_start_frame: metadata.block_start_frame,
        propagation_sequence: 0,
        sources: &[] as &[SpatialBackendSourceBlock<'_>],
        presentation_bank,
        environmental_bank,
        metadata,
    });
    INSIDE_NEUTRAL_CALLBACK.with(|inside| inside.set(false));
    result
}

fn assert_preallocated_bank_shapes(
    render: &NeutralSwapRenderGraph<MockNeutralGraph>,
    route: NeutralSwapRouteIdentity,
) {
    assert_eq!(
        render.old_presentation_bank.len(),
        route.presentation_bank_samples().unwrap()
    );
    assert_eq!(
        render.new_presentation_bank.len(),
        route.presentation_bank_samples().unwrap()
    );
    assert_eq!(
        render.old_environmental_bank.len(),
        route.environmental_bank_samples().unwrap()
    );
    assert_eq!(
        render.new_environmental_bank.len(),
        route.environmental_bank_samples().unwrap()
    );
}

fn scratch_pointers(render: &NeutralSwapRenderGraph<MockNeutralGraph>) -> [*const f32; 4] {
    [
        render.old_presentation_bank.as_ptr(),
        render.new_presentation_bank.as_ptr(),
        render.old_environmental_bank.as_ptr(),
        render.new_environmental_bank.as_ptr(),
    ]
}

fn scratch_capacities(render: &NeutralSwapRenderGraph<MockNeutralGraph>) -> [usize; 4] {
    [
        render.old_presentation_bank.capacity(),
        render.new_presentation_bank.capacity(),
        render.old_environmental_bank.capacity(),
        render.new_environmental_bank.capacity(),
    ]
}

/// Production-linked γ10 continuity proof.
///
/// Unlike the mock swap tests above, this keeps one real Steam neutral graph
/// alive, feeds a globally indexed (seekable) program through the callback
/// path, admits a second in-flight event before the first swap, and then walks
/// three more complete world generations.  The route controller intentionally
/// predicts e1, receives e2, and refuses the next neighbor until the old
/// generation's tail is complete.  No offline process restart is involved.
#[test]
fn linked_production_gamma10_continuity_keeps_seek_and_event_through_four_cell_route() {
    use fightbox_runtime::backend::{
        SimulationUpdate, SourceMotion, SpatialBackendSourceBlock, SpatialOutputMetadata,
    };
    use fightbox_runtime::{
        CellIdentity, CellPrepareEstimate, CellStreamManager, FreshMemorySample, PrepareAdmission,
        PrepareRefusalReason, RouteCellCandidate, RouteDirection,
    };

    const SAMPLE_RATE: f32 = 48_000.0;
    const BLOCK: usize = 128;
    const RAW_BYTES: u64 = 40 * 1024 * 1024;
    const RESIDENT_BYTES: u64 = 100 * 1024 * 1024;
    const EVENT_ONSET: u64 = (BLOCK as u64) * 2 + (BLOCK as u64 / 2);
    const FIRST_SWAP_BLOCK: usize = 32;
    const EVENT_TAU_FRAMES: f32 = SAMPLE_RATE * 0.35;

    fn cell(name: &str) -> CellIdentity {
        CellIdentity::new("wave17-locality", name)
    }

    fn estimate() -> CellPrepareEstimate {
        CellPrepareEstimate {
            raw_cell_bytes: RAW_BYTES,
            prepared_resident_bytes: RESIDENT_BYTES,
            preparation_scratch_bytes: 80 * 1024 * 1024,
        }
    }

    fn memory() -> FreshMemorySample {
        FreshMemorySample {
            advisory_reserve_bytes: 900 * 1024 * 1024,
            process_resident_bytes: 180 * 1024 * 1024,
        }
    }

    fn route_update() -> SimulationUpdate {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        let pose = |position| fightbox_api::Pose {
            position,
            forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        };
        sources[0] = SourceMotion {
            active: true,
            pose: pose(ApiEnuVector3::new(-3.0, 2.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        sources[1] = SourceMotion {
            active: true,
            pose: pose(ApiEnuVector3::new(-3.0, 3.0, 1.5)),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
        SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: pose(ApiEnuVector3::new(2.0, 2.0, 1.5)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        }
    }

    fn block_programs(start: u64) -> (Vec<f32>, Vec<f32>) {
        let continuous = (0..BLOCK)
            .map(|offset| {
                let frame = start + offset as u64;
                (2.0 * core::f32::consts::PI * 375.0 * frame as f32 / SAMPLE_RATE).sin() * 0.08
            })
            .collect();
        let event = (0..BLOCK)
            .map(|offset| {
                let frame = start + offset as u64;
                if frame < EVENT_ONSET {
                    return 0.0;
                }
                let elapsed = (frame - EVENT_ONSET) as f32;
                let envelope = (-elapsed / EVENT_TAU_FRAMES).exp();
                // Deterministic broadband event energy keeps each block's
                // measured onset/decay monotonic instead of making the
                // duplicate-onset check depend on a sine phase.
                let noise = frame
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let sign = if noise & (1_u64 << 63) == 0 {
                    1.0
                } else {
                    -1.0
                };
                sign * 0.24 * envelope
            })
            .collect();
        (continuous, event)
    }

    fn rms(values: &[f32]) -> f64 {
        let energy = values
            .iter()
            .copied()
            .map(f64::from)
            .map(|value| value * value)
            .sum::<f64>();
        (energy / values.len().max(1) as f64).sqrt()
    }

    fn db_delta(a: f64, b: f64) -> f64 {
        if a <= f64::MIN_POSITIVE || b <= f64::MIN_POSITIVE {
            0.0
        } else {
            20.0 * (a / b).log10().abs()
        }
    }

    // The production cell selector is exercised on the same four-cell
    // estimator-bound topology as the Wave 17 fixture.  The intentionally
    // wrong e1 prediction is replaced before any world is built.
    let route = [cell("e0:n0"), cell("e1:n0"), cell("e1:n1"), cell("e0:n1")];
    let candidates = [
        RouteCellCandidate {
            identity: route[1].clone(),
            route_offset_mm: 485_000,
        },
        RouteCellCandidate {
            identity: route[2].clone(),
            route_offset_mm: 485_000,
        },
    ];
    assert_eq!(
        fightbox_runtime::choose_route_candidate(&route[0], RouteDirection::Forward, &candidates),
        Some(route[1].clone())
    );

    let mut residency = CellStreamManager::new(
        route[0].clone(),
        route[0].clone(),
        RESIDENT_BYTES,
        Default::default(),
    );
    let stale_ticket = match residency.request_prepare(route[1].clone(), estimate(), memory()) {
        PrepareAdmission::Queued(ticket) => ticket,
        other => panic!("stale prediction was not queued: {other:?}"),
    };
    let selected_ticket = match residency.request_prepare(route[2].clone(), estimate(), memory()) {
        PrepareAdmission::ReplacedUnstarted { cancelled, queued } => {
            assert_eq!(cancelled, route[1]);
            queued
        }
        other => panic!("prediction miss did not replace stale neighbor: {other:?}"),
    };
    assert!(matches!(
        residency.start_prepare(selected_ticket, std::time::Duration::from_millis(1)),
        Ok(_)
    ));
    assert!(matches!(
        residency.complete_prepare(
            selected_ticket,
            route[2].clone(),
            RESIDENT_BYTES,
            std::time::Duration::from_millis(2)
        ),
        Ok(fightbox_runtime::CompletePreparation::Prepared)
    ));
    assert_eq!(stale_ticket.serial(), 1);

    let mesh = SceneMesh::controlled_s3_corner();
    // Keep the real bake small enough for a host gate while retaining a real
    // serialized path layer. Every generation below loads this same verified
    // batch through the production builder; there is no mock backend.
    let mut bake_request = S3BakeRequest::default();
    bake_request.mesh = mesh.clone();
    bake_request.probes = ProbeVolume {
        min_enu_m: EnuVector3::new(-4.0, -4.0, 0.0),
        max_enu_m: EnuVector3::new(4.0, 4.0, 3.0),
        spacing_m: 4.0,
        height_above_floor_m: 1.5,
    };
    bake_request.pathing = PathBakeConfig {
        num_visibility_samples: 1,
        probe_visibility_radius_m: 1.0,
        visibility_threshold: 0.1,
        visibility_range_m: 100.0,
        path_range_m: 100.0,
        num_threads: 1,
    };
    let baked = bake_s3(&bake_request).expect("host gate must create the real path bake");

    let descriptors = [
        MultiSourceDescriptor::at(ApiEnuVector3::new(-3.0, 2.0, 1.5))
            .with_initially_active(true)
            .with_reflection_send(false),
        MultiSourceDescriptor::at(ApiEnuVector3::new(-3.0, 3.0, 1.5))
            .with_initially_active(true)
            .with_reflection_send(true),
    ];
    let mut simulation_config = S3SimulationConfig::default();
    simulation_config.reflection_rays = 64;
    simulation_config.diffuse_samples = 8;
    simulation_config.reflection_bounces = 1;
    simulation_config.reflection_duration_s = 0.10;
    simulation_config.reflection_order = 1;
    simulation_config.pathing_order = 1;
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK as i32,
    };
    let (mut simulation, mut render) = build_spatial_multi_source_session(
        &mesh,
        &baked,
        audio,
        simulation_config,
        &descriptors,
        &[1, 1],
        1,
        QualityTier::Desktop,
    )
    .expect("construct production neutral session");
    let update = route_update();
    simulation
        .prepare_simulation_for_realtime(&update)
        .expect("initial production snapshot");
    render
        .prepare_for_realtime()
        .expect("initial production graph prepare");
    let one_world_memory = simulation.quality_governor_telemetry().memory;
    let tracked_category_sum = |memory: &crate::SessionMemoryTelemetry| {
        memory
            .snapshot_ring_payload_bytes
            .saturating_add(memory.reflection_ir_payload_capacity_bytes)
            .saturating_add(memory.audio_buffer_payload_bytes)
            .saturating_add(memory.render_scratch_bytes)
            .saturating_add(memory.propagation_delay_line_bytes)
            .saturating_add(memory.retained_bake_bytes)
    };
    assert_eq!(
        one_world_memory.tracked_current_bytes,
        tracked_category_sum(&one_world_memory)
    );

    let mut frame = 0_u64;
    let mut previous_level = None;
    let mut levels = Vec::new();
    let mut seam_levels = Vec::new();
    let mut event_levels = Vec::new();
    let mut directions: Vec<ApiEnuVector3> = Vec::new();
    let mut saw_tail = false;
    let mut saw_tail_energy = false;
    let mut saw_warmup = false;
    let mut saw_crossfade = false;
    let mut swap_count = 0;

    // Render enough pre-roll for the event to be genuinely in flight before
    // the first adoption. Input frame numbering never resets at a cell seam.
    let mut render_one = |frame: u64,
                          simulation: &mut NeutralMultiSourceSimulation,
                          render: &mut NeutralMultiSourceRenderGraph| {
        let (continuous, event) = block_programs(frame);
        let sources = [
            SpatialBackendSourceBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [continuous.as_slice(), &[]],
            },
            SpatialBackendSourceBlock {
                source_index: 1,
                program_plane_count: 1,
                program_planes: [event.as_slice(), &[]],
            },
        ];
        let mut presentation = vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK];
        let mut environment = vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK];
        let mut metadata = SpatialOutputMetadata {
            block_start_frame: frame,
            ..SpatialOutputMetadata::default()
        };
        render
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: frame,
                propagation_sequence: simulation.latest_direct_sequence(),
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .expect("production callback block must render");
        // The seam excursion gate is for the continuous seekable source;
        // the in-flight event has its own onset/restart gate below.
        let level = rms(&presentation[..BLOCK]);
        let event_level = rms(&presentation[3 * BLOCK..4 * BLOCK]);
        let block_delta = previous_level.map(|previous| db_delta(level, previous));
        if let Some(delta) = block_delta {
            levels.push(delta);
        }
        previous_level = Some(level);
        event_levels.push(event_level);
        if let Some(previous) = directions.last().copied() {
            let dot = previous.east_m * metadata.presentation_feeds[0].direction_enu.east_m
                + previous.north_m * metadata.presentation_feeds[0].direction_enu.north_m
                + previous.up_m * metadata.presentation_feeds[0].direction_enu.up_m;
            assert!(
                dot >= -1.0e-3,
                "production seam flipped source direction: {dot}"
            );
        }
        let direction = metadata.presentation_feeds[0].direction_enu;
        let direction_norm = (direction.east_m * direction.east_m
            + direction.north_m * direction.north_m
            + direction.up_m * direction.up_m)
            .sqrt();
        assert!(
            direction_norm > 0.9,
            "production callback published no direction authority: {direction:?}"
        );
        directions.push(direction);
        let lifecycle = simulation.cell_stream_lifecycle();
        if lifecycle == NeutralSwapLifecycle::Prepared {
            saw_warmup = true;
        }
        if lifecycle == NeutralSwapLifecycle::Crossfading {
            saw_crossfade = true;
        }
        if matches!(
            lifecycle,
            NeutralSwapLifecycle::Crossfading | NeutralSwapLifecycle::TailRetiring
        ) {
            if let Some(delta) = block_delta {
                seam_levels.push(delta);
            }
        }
        if lifecycle == NeutralSwapLifecycle::TailRetiring {
            saw_tail = true;
            if environment.iter().any(|sample| sample.abs() > 1.0e-8) {
                saw_tail_energy = true;
            }
        }
        (metadata, lifecycle)
    };

    for _ in 0..FIRST_SWAP_BLOCK {
        let _ = render_one(frame, &mut simulation, &mut render);
        frame += BLOCK as u64;
    }

    let mut next_cells = [route[2].clone(), route[1].clone(), route[3].clone()].into_iter();
    let mut predicted_ticket = Some(selected_ticket);
    while let Some(next) = next_cells.next() {
        // Manager-level admission is the route authority. During the real
        // callback crossfade, requesting another neighbor must be refused;
        // importantly, we do not build that third world.
        let estimate = estimate();
        let ticket = if let Some(ticket) = predicted_ticket.take() {
            assert_eq!(next, route[2]);
            ticket
        } else {
            match residency.request_prepare(next.clone(), estimate, memory()) {
                PrepareAdmission::Queued(ticket) => ticket,
                other => panic!("next route cell admission failed: {other:?}"),
            }
        };
        if next == route[2] {
            // This is the selected completion from the deliberate e1 -> e2
            // prediction miss; it was already started and completed above.
            residency.adopt_prepared(ticket).unwrap();
        } else {
            residency
                .start_prepare(ticket, std::time::Duration::from_secs(1))
                .unwrap();
            residency
                .complete_prepare(
                    ticket,
                    next.clone(),
                    RESIDENT_BYTES,
                    std::time::Duration::from_secs(2),
                )
                .unwrap();
            residency.adopt_prepared(ticket).unwrap();
        }

        let mut prepared = simulation
            .prepare_world(&mesh, &baked)
            .expect("prepare one production neighbor");
        prepared
            .prepare_simulation_for_realtime(&update)
            .expect("prime one production neighbor");
        simulation
            .swap_prepared_world(prepared)
            .expect("publish one production neighbor");
        assert_eq!(
            simulation.cell_stream_lifecycle(),
            NeutralSwapLifecycle::Prepared
        );
        assert!(matches!(
            simulation.prepare_world(&mesh, &baked),
            Err(BackendError::InvalidInput(
                "neutral locality preparation is refused until terminal tail retirement"
            ))
        ));
        let two_world_memory = simulation.quality_governor_telemetry().memory;
        assert!(
            two_world_memory.tracked_current_bytes > one_world_memory.tracked_current_bytes,
            "two retained worlds must increase tracked current memory"
        );
        assert!(
            two_world_memory.retained_bake_bytes > one_world_memory.retained_bake_bytes,
            "two retained worlds must include both baked payloads"
        );
        assert!(
            two_world_memory.render_scratch_bytes > one_world_memory.render_scratch_bytes,
            "two retained render generations must both be counted"
        );
        assert_eq!(
            two_world_memory.propagation_delay_line_bytes,
            one_world_memory
                .propagation_delay_line_bytes
                .saturating_mul(2)
                .saturating_sub(crate::propagation_delay::bandlimited_kernel_payload_bytes()),
            "two worlds must count both histories and the shared kernel exactly once"
        );
        assert_eq!(
            two_world_memory.tracked_current_bytes,
            tracked_category_sum(&two_world_memory),
            "two-world tracked total must equal its exact category sum"
        );
        swap_count += 1;

        // The one-slot production swap and the manager's tail state jointly
        // enforce the two-world ceiling. No candidate graph is constructed for
        // this refusal path.
        let refusal = residency.request_prepare(cell("unrequested-third"), estimate, memory());
        assert!(matches!(
            refusal,
            PrepareAdmission::Refused(PrepareRefusalReason::TailRetiring)
        ));

        let mut complete = false;
        let mut tail_control_checked = false;
        while !complete {
            let (_, _lifecycle) = render_one(frame, &mut simulation, &mut render);
            frame += BLOCK as u64;
            if simulation.cell_stream_lifecycle() == NeutralSwapLifecycle::TailRetiring
                && !tail_control_checked
            {
                let retiring_before = simulation
                    .retiring
                    .as_ref()
                    .map(|(retiring, _)| retiring.latest_direct_sequence())
                    .expect("tail retains old simulation ownership");
                simulation.update_inputs(&update);
                simulation.run_direct().expect("active tail control update");
                let retiring_after = simulation
                    .retiring
                    .as_ref()
                    .map(|(retiring, _)| retiring.latest_direct_sequence())
                    .expect("tail retains old simulation ownership");
                assert_eq!(
                    retiring_after, retiring_before,
                    "terminal tail must not admit new old-world simulation truth"
                );
                tail_control_checked = true;
            }
            if simulation.cell_stream_lifecycle() == NeutralSwapLifecycle::TailComplete {
                complete = simulation.collect_retired_world();
            }
            assert!(
                frame < SAMPLE_RATE as u64 * 8,
                "production tail did not retire"
            );
        }
        assert!(
            tail_control_checked,
            "retiring control freeze was not exercised"
        );
        residency.finish_tail_retirement();
        assert_eq!(
            simulation.cell_stream_lifecycle(),
            NeutralSwapLifecycle::Idle
        );
        let single_world_memory = simulation.quality_governor_telemetry().memory;
        assert_eq!(
            single_world_memory.tracked_current_bytes,
            one_world_memory.tracked_current_bytes
        );
        assert_eq!(
            single_world_memory.retained_bake_bytes,
            one_world_memory.retained_bake_bytes
        );
        assert_eq!(
            single_world_memory.tracked_current_bytes,
            tracked_category_sum(&single_world_memory)
        );
        assert_eq!(
            single_world_memory.tracked_peak_bytes, two_world_memory.tracked_peak_bytes,
            "terminal collection must retain the actual two-world historical peak"
        );
        assert!(single_world_memory.tracked_peak_bytes > single_world_memory.tracked_current_bytes);
    }

    // The first sample after the onset is the only attack. A world adoption
    // later in the same event cannot create a second attack or restart seek.
    let onset_block = (EVENT_ONSET / BLOCK as u64) as usize;
    assert!(
        event_levels[onset_block] > 0.0,
        "in-flight event never reached callback"
    );
    // Steam's real effect has a few callback blocks of startup latency. Use
    // the complete first fade as the attack reference, then reject a second
    // attack after the old generation is retired. A duplicated onset would be
    // approximately +6 dB here and is intentionally far outside this bound.
    let first_swap_end = FIRST_SWAP_BLOCK + usize::from(NEUTRAL_SWAP_FADE_BLOCKS);
    let first_attack_peak = event_levels[onset_block..first_swap_end]
        .iter()
        .copied()
        .fold(0.0_f64, f64::max);
    let post_adoption_peak = event_levels[first_swap_end..]
        .iter()
        .copied()
        .fold(0.0_f64, f64::max);
    assert!(
        post_adoption_peak <= first_attack_peak * 1.50,
        "event onset duplicated/restarted after adoption: attack={first_attack_peak} later={post_adoption_peak}"
    );
    assert_eq!(
        swap_count, 3,
        "four-cell route must perform three adoptions"
    );
    assert!(saw_tail, "real reflection tail never entered TailRetiring");
    assert!(
        saw_tail_energy,
        "retiring production tail was truncated to silence"
    );
    assert!(
        saw_warmup,
        "production candidate warmup lifecycle was never observed"
    );
    assert!(
        saw_crossfade,
        "production crossfade lifecycle was never observed"
    );
    assert!(
        !seam_levels.is_empty(),
        "production seam metric was vacuous"
    );
    eprintln!("gamma10 seam_levels={seam_levels:?}");
    assert!(
        seam_levels.iter().copied().fold(0.0_f64, f64::max) <= 1.0,
        "production seam excursion exceeded unchanged 1 dB gate: {seam_levels:?}"
    );
    assert!(
        directions.len() > 8,
        "production route produced no direction samples"
    );
}
