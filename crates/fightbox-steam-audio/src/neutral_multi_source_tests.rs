use super::*;
use fightbox_api::{EnuVector3, ExtentDescriptor, ListenerState, Pose};
use fightbox_runtime::backend::{
    MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS, SimulationError,
    SimulationUpdate, SourceMotion, SpatialAmbisonicChannelOrder, SpatialAmbisonicNormalization,
    SpatialBackendRenderError, SpatialBackendRenderGraph, SpatialBackendSourceBlock,
    SpatialEnvironmentalBasis, SpatialFeedPlacement, SpatialOutputMetadata, SpatialOutputValidity,
    SpatialPresentationComponent, SpatialPropagationRenderBlock,
};

const SAMPLE_RATE_HZ: i32 = 48_000;
const BLOCK_FRAMES: usize = 128;

#[path = "neutral_mobile_soak_tests.rs"]
mod mobile_soak;

#[path = "neutral_stereo_motion_tests.rs"]
mod stereo_motion;

fn audio() -> AudioConfig {
    AudioConfig {
        sample_rate_hz: SAMPLE_RATE_HZ,
        frame_size: BLOCK_FRAMES as i32,
    }
}

fn config(order: i32) -> S3SimulationConfig {
    S3SimulationConfig {
        reflection_rays: 16,
        diffuse_samples: 4,
        reflection_bounces: 0,
        reflection_duration_s: 0.01,
        reflection_order: order,
        pathing_order: order,
        ..S3SimulationConfig::default()
    }
}

fn descriptor(extent: ExtentDescriptor) -> crate::MultiSourceDescriptor {
    crate::MultiSourceDescriptor::at(EnuVector3::default())
        .with_extent(extent)
        .with_reflection_send(false)
}

fn point() -> crate::MultiSourceDescriptor {
    descriptor(ExtentDescriptor::Point)
}

fn stereo(width_m: f32) -> crate::MultiSourceDescriptor {
    descriptor(ExtentDescriptor::StereoImage { width_m })
}

fn build(
    descriptors: &[crate::MultiSourceDescriptor],
    channels: &[usize],
    order: usize,
) -> (MultiSourceSimulation, NeutralMultiSourceRenderGraph) {
    build_neutral_multi_source_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        config(order as i32),
        descriptors,
        channels,
        order,
        17,
        QualityTier::Desktop,
    )
    .expect("construct neutral linked graph")
}

fn one_active_source_update() -> SimulationUpdate {
    let mut update = SimulationUpdate {
        listener: ListenerState {
            pose: default_api_pose(EnuVector3::new(0.0, 0.0, 1.5)),
            linear_velocity_mps: EnuVector3::default(),
        },
        sources: [SourceMotion::default(); MAX_ACTIVE_SOURCES],
    };
    update.sources[0] = SourceMotion {
        active: true,
        pose: default_api_pose(EnuVector3::new(2.0, 3.0, 1.5)),
        linear_velocity_mps: EnuVector3::default(),
    };
    update
}

fn output_banks() -> (Vec<f32>, Vec<f32>, SpatialOutputMetadata) {
    (
        vec![f32::NAN; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES],
        vec![f32::NAN; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES],
        SpatialOutputMetadata::default(),
    )
}

fn graph_from_current_simulation(
    simulation: &MultiSourceSimulation,
    cfg: S3SimulationConfig,
    descriptors: &[crate::MultiSourceDescriptor],
    channels: &[usize],
    order: usize,
) -> NeutralMultiSourceRenderGraph {
    let (_propagation_writer, propagation_reader) = SnapshotPublication::new(simulation.snapshot);
    let (_governor_writer, governor_reader) =
        SnapshotPublication::new(simulation.governor.render_quality());
    create_neutral_render_graph(
        Arc::clone(&simulation.world),
        audio(),
        cfg,
        propagation_reader,
        governor_reader,
        descriptors,
        channels,
        order as i32,
    )
    .expect("construct graph from exact current simulation snapshot")
}

fn assert_owned_audio_buffer_is_zero(buffer: &mut OwnedAudioBuffer) {
    let sample_count =
        usize::try_from(buffer.samples).unwrap() * usize::try_from(buffer.channels).unwrap();
    let mut observed = vec![f32::NAN; sample_count];
    buffer.read_interleaved(&mut observed);
    assert!(
        observed.iter().all(|sample| sample.to_bits() == 0),
        "prepared SDK buffer retained non-zero history"
    );
}

#[test]
fn realtime_prepare_is_transparent_primes_dormant_and_direct_only_effects_and_resets_tails() {
    let cfg = config(1);
    let descriptors = (0..6)
        .map(|index| {
            crate::MultiSourceDescriptor::at(EnuVector3::new(index as f32, 2.0, 1.5))
                .with_extent(ExtentDescriptor::LineSegment { length_m: 2.0 })
        })
        .collect::<Vec<_>>();
    let channels = vec![1; descriptors.len()];
    let (mut simulation, _construction_graph) = build_neutral_multi_source_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        cfg,
        &descriptors,
        &channels,
        1,
        41,
        QualityTier::Mobile,
    )
    .unwrap();

    let mut update = one_active_source_update();
    for index in 0..5 {
        update.sources[index] = SourceMotion {
            active: true,
            pose: default_api_pose(EnuVector3::new(
                if index < 4 { 2.0 + index as f32 } else { 80.0 },
                2.0,
                1.5,
            )),
            linear_velocity_mps: EnuVector3::default(),
        };
    }
    update.sources[5] = SourceMotion {
        active: false,
        pose: default_api_pose(EnuVector3::new(100.0, 2.0, 1.5)),
        linear_velocity_mps: EnuVector3::default(),
    };
    simulation.update_inputs(&update);
    simulation.run_direct().unwrap();
    simulation.run_reflections_for_realtime_prepare().unwrap();

    let quality = simulation.quality_governor_telemetry();
    assert_eq!(quality.sources[4].quality, SourceQualityLevel::DirectOnly);
    assert_eq!(quality.sources[5].quality, SourceQualityLevel::DirectOnly);
    assert!(simulation.snapshot.sources[4].active);
    assert!(!simulation.snapshot.sources[5].active);
    for source in &simulation.snapshot.sources[..descriptors.len()] {
        assert_ne!(source.reflections.ir, 0);
    }

    let mut cold = graph_from_current_simulation(&simulation, cfg, &descriptors, &channels, 1);
    let mut prepared = graph_from_current_simulation(&simulation, cfg, &descriptors, &channels, 1);
    let mut zero_tail = graph_from_current_simulation(&simulation, cfg, &descriptors, &channels, 1);
    let memory_before = prepared.persistent_memory();
    let correlation_before = prepared.correlation_diagnostics();
    let applied_quality_before = prepared.applied_governor_quality;
    let reflection_gain_before = prepared.reflection_output_gain;

    prepared.prepare_for_realtime().unwrap();
    prepared.prepare_for_realtime().unwrap();
    assert!(prepared.prepared_for_realtime);
    assert_eq!(prepared.prepared_reflection_effect_count, descriptors.len());
    assert_eq!(prepared.persistent_memory(), memory_before);
    assert_eq!(prepared.correlation_diagnostics(), correlation_before);
    assert_eq!(prepared.applied_governor_quality, applied_quality_before);
    assert_eq!(
        prepared.reflection_output_gain.to_bits(),
        reflection_gain_before.to_bits()
    );
    for state in &mut prepared.sources {
        assert_owned_audio_buffer_is_zero(state.indirect_input.as_mut().unwrap());
        assert_owned_audio_buffer_is_zero(state.reflection_scratch.as_mut().unwrap());
    }
    assert_owned_audio_buffer_is_zero(prepared.reflection_mix.as_mut().unwrap());
    assert!(
        prepared
            .program_interleaved_work
            .iter()
            .all(|sample| *sample == 0.0)
    );
    assert!(
        prepared
            .effect_interleaved_work
            .iter()
            .all(|sample| *sample == 0.0)
    );
    assert!(prepared.line_work.iter().all(|sample| *sample == 0.0));
    assert!(
        prepared
            .steam_environment_bank
            .iter()
            .all(|sample| *sample == 0.0)
    );

    // Compare the first real render against an otherwise identical graph whose
    // guard alone is opened, leaving Steam's lazy state genuinely cold.
    cold.prepared_for_realtime = true;
    let program = (0..BLOCK_FRAMES)
        .map(|frame| {
            if frame == 0 {
                0.5
            } else {
                frame as f32 * 0.000_1
            }
        })
        .collect::<Vec<_>>();
    let sources = (0..5)
        .map(|source_index| SpatialBackendSourceBlock {
            source_index,
            program_plane_count: 1,
            program_planes: [program.as_slice(), &[]],
        })
        .collect::<Vec<_>>();
    let sequence = simulation.latest_direct_sequence();
    let (mut cold_presentation, mut cold_environment, mut cold_metadata) = output_banks();
    let (mut warm_presentation, mut warm_environment, mut warm_metadata) = output_banks();
    render_with_sequence(
        &mut cold,
        &sources,
        0,
        sequence,
        &mut cold_presentation,
        &mut cold_environment,
        &mut cold_metadata,
    )
    .unwrap();
    render_with_sequence(
        &mut prepared,
        &sources,
        0,
        sequence,
        &mut warm_presentation,
        &mut warm_environment,
        &mut warm_metadata,
    )
    .unwrap();
    assert_eq!(
        cold_presentation
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        warm_presentation
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        cold_environment
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>(),
        warm_environment
            .iter()
            .map(|sample| sample.to_bits())
            .collect::<Vec<_>>()
    );
    assert_eq!(cold_metadata, warm_metadata);
    assert_eq!(warm_metadata.generation, simulation.world.generation);
    assert_eq!(warm_metadata.block_start_frame, 0);

    zero_tail.prepare_for_realtime().unwrap();
    let silence = vec![0.0; BLOCK_FRAMES];
    let silent_sources = (0..5)
        .map(|source_index| SpatialBackendSourceBlock {
            source_index,
            program_plane_count: 1,
            program_planes: [silence.as_slice(), &[]],
        })
        .collect::<Vec<_>>();
    for block_index in 0..2 {
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render_with_sequence(
            &mut zero_tail,
            &silent_sources,
            (block_index * BLOCK_FRAMES) as u64,
            sequence,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert!(presentation.iter().all(|sample| sample.to_bits() == 0));
        assert!(environment.iter().all(|sample| sample.to_bits() == 0));
        assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
        assert_eq!(metadata.generation, simulation.world.generation);
    }
}

#[test]
fn unprepared_render_rejects_before_block_validation_and_no_reflection_prepare_is_a_bypass() {
    let (mut simulation, mut graph) = build(&[point()], &[1], 0);
    let memory_before = graph.persistent_memory();
    let mut presentation = vec![17.0; 1];
    let mut environment = vec![23.0; 1];
    let mut metadata = SpatialOutputMetadata {
        generation: 99,
        ..SpatialOutputMetadata::default()
    };
    let metadata_before = metadata;
    assert_eq!(
        graph.render_spatial_block(SpatialPropagationRenderBlock {
            block_start_frame: 0,
            propagation_sequence: 0,
            sources: &[],
            presentation_bank: &mut presentation,
            environmental_bank: &mut environment,
            metadata: &mut metadata,
        }),
        Err(SpatialBackendRenderError::InactiveGraph)
    );
    assert_eq!(presentation, [17.0]);
    assert_eq!(environment, [23.0]);
    assert_eq!(metadata, metadata_before);

    let valid_snapshot = simulation.snapshot;
    let mut mismatched_snapshot = valid_snapshot;
    mismatched_snapshot.world_generation = valid_snapshot.world_generation.wrapping_add(1);
    simulation.publication.publish(mismatched_snapshot);
    assert_eq!(
        graph.prepare_for_realtime(),
        Err(SpatialBackendRenderError::InactiveGraph)
    );
    assert!(!graph.prepared_for_realtime);
    assert_eq!(graph.prepared_reflection_effect_count, 0);

    simulation.publication.publish(valid_snapshot);
    graph.prepare_for_realtime().unwrap();
    assert!(graph.prepared_for_realtime);
    assert_eq!(graph.prepared_reflection_effect_count, 0);
    assert_eq!(graph.persistent_memory(), memory_before);
}

#[test]
fn parametric_realtime_prepare_accepts_finite_reverb_with_no_ir_shape() {
    let mut cfg = config(1);
    cfg.reflection_effect = crate::ReflectionEffectConfig::PARAMETRIC;
    let descriptors = [crate::MultiSourceDescriptor::at(EnuVector3::new(
        2.0, 3.0, 1.5,
    ))];
    // The public Wave 0 neutral constructor remains convolution-only. Exercise
    // the internal graph preparer directly so its type-specific validation is
    // already protected when the contract admits parametric reverb later.
    let (mut simulation, publication, governor_quality) = build_simulation_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        cfg,
        &descriptors,
        45,
        QualityTier::Desktop,
        None,
    )
    .unwrap();
    let mut graph = create_neutral_render_graph(
        Arc::clone(&simulation.world),
        audio(),
        cfg,
        publication,
        governor_quality,
        &descriptors,
        &[1],
        1,
    )
    .unwrap();
    let mut snapshot = simulation.snapshot;
    snapshot.sources[0].reflections = SteamReflectionParams {
        ir: 0,
        reverb_times: [0.25, 0.5, 0.75],
        eq: [0.8, 0.9, 1.0],
        delay: 0,
        num_channels: 0,
        ir_size: 0,
        tan_slot: 0,
    };
    simulation.publication.publish(snapshot);

    graph.prepare_for_realtime().unwrap();

    assert!(graph.prepared_for_realtime);
    assert_eq!(graph.prepared_reflection_effect_count, 1);
}

#[test]
fn failed_repeat_prepare_revokes_the_previous_prepared_state() {
    let descriptor = crate::MultiSourceDescriptor::at(EnuVector3::new(2.0, 3.0, 1.5));
    let (mut simulation, mut graph) = build_neutral_multi_source_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        config(1),
        &[descriptor],
        &[1],
        1,
        44,
        QualityTier::Desktop,
    )
    .unwrap();
    simulation.update_inputs(&one_active_source_update());
    simulation.run_reflections_for_realtime_prepare().unwrap();
    graph.prepare_for_realtime().unwrap();
    assert!(graph.prepared_for_realtime);

    let mut invalid = simulation.snapshot;
    invalid.sources[0].reflections.ir = 0;
    simulation.publication.publish(invalid);
    assert_eq!(
        graph.prepare_for_realtime(),
        Err(SpatialBackendRenderError::InactiveGraph)
    );
    assert!(!graph.prepared_for_realtime);
    assert_eq!(graph.prepared_reflection_effect_count, 0);

    simulation.publication.publish(simulation.snapshot);
    graph.prepare_for_realtime().unwrap();
    assert!(graph.prepared_for_realtime);
    assert_eq!(graph.prepared_reflection_effect_count, 1);
}

#[test]
fn forced_reflection_prepare_bypasses_skip_without_consuming_cadence_or_governor_evidence() {
    let descriptor = crate::MultiSourceDescriptor::at(EnuVector3::new(2.0, 3.0, 1.5));
    let (mut simulation, _graph) = build_neutral_multi_source_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        config(1),
        &[descriptor],
        &[1],
        1,
        42,
        QualityTier::Mobile,
    )
    .unwrap();
    simulation.update_inputs(&one_active_source_update());
    assert_eq!(
        simulation
            .governor
            .render_quality()
            .reflections
            .cadence_divisor,
        2
    );
    simulation.run_reflections().unwrap();
    let after_scheduled = simulation.diagnostics();
    simulation.run_reflections().unwrap();
    let after_skipped = simulation.diagnostics();
    assert_eq!(
        after_skipped.vendor_pass_runs[2],
        after_scheduled.vendor_pass_runs[2]
    );
    assert_eq!(simulation.reflection_cadence_tick, 2);

    let cadence_before = simulation.pass_cadences;
    let telemetry_before = simulation.quality_governor_telemetry();
    let snapshot_sequence_before = simulation.snapshot.sequence;
    simulation.run_reflections_for_realtime_prepare().unwrap();
    let telemetry_after = simulation.quality_governor_telemetry();
    assert_eq!(simulation.reflection_cadence_tick, 2);
    assert_eq!(simulation.pass_cadences, cadence_before);
    assert_eq!(simulation.snapshot.sequence, snapshot_sequence_before + 1);
    assert_eq!(
        simulation.diagnostics().vendor_pass_runs[2],
        after_skipped.vendor_pass_runs[2] + 1
    );
    assert_eq!(
        telemetry_after.simulation_lateness_ns,
        telemetry_before.simulation_lateness_ns
    );
    assert_eq!(telemetry_after.reason, telemetry_before.reason);
    assert_eq!(
        telemetry_after.ladder_position,
        telemetry_before.ladder_position
    );
    assert_eq!(telemetry_after.p50_ns, telemetry_before.p50_ns);
    assert_eq!(telemetry_after.p95_ns, telemetry_before.p95_ns);
    assert_eq!(telemetry_after.p99_ns, telemetry_before.p99_ns);
    assert_eq!(telemetry_after.p99_9_ns, telemetry_before.p99_9_ns);

    // The forced pass did not consume tick 2: the next ordinary call is due.
    let vendor_runs_before = simulation.diagnostics().vendor_pass_runs[2];
    simulation.run_reflections().unwrap();
    assert_eq!(
        simulation.diagnostics().vendor_pass_runs[2],
        vendor_runs_before + 1
    );
}

#[test]
fn consolidated_simulation_prepare_publishes_all_passes_without_scheduler_evidence() {
    let descriptor = stereo(2.0);
    let (mut simulation, _graph) = build_neutral_multi_source_generation(
        &SceneMesh::controlled_s3_corner(),
        None,
        audio(),
        config(1),
        &[descriptor],
        &[2],
        1,
        43,
        QualityTier::Mobile,
    )
    .unwrap();
    let update = one_active_source_update();
    let cadence_before = simulation.pass_cadences;
    let reflection_tick_before = simulation.reflection_cadence_tick;
    let telemetry_before = simulation.quality_governor_telemetry();

    simulation.prepare_simulation_for_realtime(&update).unwrap();

    let telemetry_after = simulation.quality_governor_telemetry();
    let diagnostics = simulation.diagnostics();
    assert_eq!(simulation.pass_cadences, cadence_before);
    assert_eq!(simulation.reflection_cadence_tick, reflection_tick_before);
    assert_eq!(diagnostics.vendor_pass_runs, [1, 0, 0]);
    assert_eq!(diagnostics.skipped_vendor_passes, [0, 1, 1]);
    assert_eq!(simulation.latest_direct_sequence(), 1);
    assert_eq!(
        telemetry_after.simulation_lateness_ns,
        telemetry_before.simulation_lateness_ns
    );
    assert_eq!(telemetry_after.reason, telemetry_before.reason);
    assert_eq!(
        telemetry_after.ladder_position,
        telemetry_before.ladder_position
    );
    assert_eq!(telemetry_after.p50_ns, telemetry_before.p50_ns);
    assert_eq!(telemetry_after.p95_ns, telemetry_before.p95_ns);
    assert_eq!(telemetry_after.p99_ns, telemetry_before.p99_ns);
    assert_eq!(telemetry_after.p99_9_ns, telemetry_before.p99_9_ns);
}

#[test]
fn runtime_simulation_lateness_maps_slots_and_only_reflections_degrade() {
    let descriptors = [point()];
    let channels = [1];
    let (simulation, graph) = build(&descriptors, &channels, 1);
    let inner = crate::linked::wrap_neutral_generation_for_test(
        simulation,
        graph,
        config(1),
        &descriptors,
        &channels,
        1,
        QualityTier::Desktop,
    );
    let mut runner = crate::SteamAudioSpatialSimulationRunner { inner };
    let initial = runner
        .quality_governor_telemetry()
        .expect("linked spatial runner exposes governor telemetry");

    // Since round 5, pathing no longer shares a thread with reflections, so
    // its lateness is telemetry only.
    fightbox_runtime::backend::SimulationRunner::observe_simulation_lateness(
        &mut runner,
        fightbox_runtime::backend::SimulationPass::Pathing,
        fightbox_runtime::backend::SIMULATION_LATENESS_TRIGGER_NS,
    );
    for _ in 0..16 {
        runner.observe_render_timing(100_000);
    }
    let after_pathing = runner
        .quality_governor_telemetry()
        .expect("linked spatial runner exposes governor telemetry");
    assert_eq!(after_pathing.ladder_position, initial.ladder_position);
    assert_eq!(
        after_pathing.simulation_lateness_ns[1],
        fightbox_runtime::backend::SIMULATION_LATENESS_TRIGGER_NS,
        "the runtime Pathing lane must map to the governor's pathing telemetry slot"
    );

    fightbox_runtime::backend::SimulationRunner::observe_simulation_lateness(
        &mut runner,
        fightbox_runtime::backend::SimulationPass::Reflections,
        fightbox_runtime::backend::SIMULATION_LATENESS_TRIGGER_NS,
    );
    for _ in 0..16 {
        runner.observe_render_timing(100_000);
    }
    let degraded = runner
        .quality_governor_telemetry()
        .expect("linked spatial runner exposes governor telemetry");
    assert_eq!(degraded.ladder_position, initial.ladder_position + 1);
    assert_eq!(
        degraded.reason,
        crate::GovernorTransitionReason::SimulationLate
    );
    assert_eq!(
        degraded.simulation_lateness_ns[2],
        fightbox_runtime::backend::SIMULATION_LATENESS_TRIGGER_NS,
        "the runtime Reflections lane must map to the governor's reflections slot"
    );
    assert_eq!(degraded.reflection_output_gain, 1.0);

    for _ in 0..16 {
        runner.observe_render_timing(100_000);
    }
    let healthy = runner
        .quality_governor_telemetry()
        .expect("linked spatial runner exposes governor telemetry");
    assert_eq!(
        healthy.ladder_position, degraded.ladder_position,
        "one scheduler observation must be consumed by one evaluation and never replay"
    );
    assert_eq!(healthy.reflection_output_gain, 1.0);
}

#[test]
fn simulation_cadence_change_rebases_before_measuring_lateness() {
    let mut cadence = SimulationPassCadence::default();
    assert_eq!(cadence.observe_start(1_000_000, 200_000_000), 0);
    assert_eq!(cadence.observe_start(207_000_000, 200_000_000), 6_000_000);

    // A quality recovery may shorten reflection cadence. The interval ending
    // at that transition was scheduled under the preceding cadence and cannot
    // be compared with the new target without inventing a large late event.
    assert_eq!(cadence.observe_start(407_000_000, 100_000_000), 0);
    assert_eq!(cadence.observe_start(512_000_000, 100_000_000), 5_000_000);
}

fn render(
    graph: &mut NeutralMultiSourceRenderGraph,
    sources: &[SpatialBackendSourceBlock<'_>],
    block_start_frame: u64,
    presentation: &mut [f32],
    environment: &mut [f32],
    metadata: &mut SpatialOutputMetadata,
) -> Result<(), SpatialBackendRenderError> {
    if !graph.prepared_for_realtime {
        graph.prepare_for_realtime()?;
    }
    let propagation_sequence = graph.publication.read().direct_sequence;
    render_with_sequence(
        graph,
        sources,
        block_start_frame,
        propagation_sequence,
        presentation,
        environment,
        metadata,
    )
}

#[allow(clippy::too_many_arguments)]
fn render_with_sequence(
    graph: &mut NeutralMultiSourceRenderGraph,
    sources: &[SpatialBackendSourceBlock<'_>],
    block_start_frame: u64,
    propagation_sequence: u64,
    presentation: &mut [f32],
    environment: &mut [f32],
    metadata: &mut SpatialOutputMetadata,
) -> Result<(), SpatialBackendRenderError> {
    graph.render_spatial_block(SpatialPropagationRenderBlock {
        block_start_frame,
        propagation_sequence,
        sources,
        presentation_bank: presentation,
        environmental_bank: environment,
        metadata,
    })
}

fn plane(bank: &[f32], index: usize) -> &[f32] {
    &bank[index * BLOCK_FRAMES..(index + 1) * BLOCK_FRAMES]
}

fn mono_source_block(program: &[f32]) -> [SpatialBackendSourceBlock<'_>; 1] {
    [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 1,
        program_planes: [program, &[]],
    }]
}

fn live_delay_totals(graph: &NeutralMultiSourceRenderGraph) -> NeutralProgramDelayMemory {
    graph
        .program_delays
        .iter()
        .map(NeutralProgramDelay::memory)
        .fold(NeutralProgramDelayMemory::default(), |mut total, memory| {
            total.audio_history_payload_bytes = total
                .audio_history_payload_bytes
                .saturating_add(memory.audio_history_payload_bytes);
            total.geometry_history_payload_bytes = total
                .geometry_history_payload_bytes
                .saturating_add(memory.geometry_history_payload_bytes);
            total.additional_channel_payload_bytes = total
                .additional_channel_payload_bytes
                .saturating_add(memory.additional_channel_payload_bytes);
            total
        })
}

fn assert_memory_matches_live_capacities(graph: &NeutralMultiSourceRenderGraph) {
    let telemetry = graph.persistent_memory();
    let live = live_delay_totals(graph);
    assert_eq!(
        telemetry.program_delay_audio_history_payload_bytes,
        live.audio_history_payload_bytes
    );
    assert_eq!(
        telemetry.program_delay_geometry_history_payload_bytes,
        live.geometry_history_payload_bytes
    );
    assert_eq!(
        telemetry.additional_program_channel_payload_bytes,
        live.additional_channel_payload_bytes
    );
    let delayed_scratch_samples = graph
        .delayed_program
        .iter()
        .flat_map(|planes| planes.iter())
        .map(Vec::capacity)
        .sum::<usize>();
    let scratch_samples = delayed_scratch_samples
        + graph.program_interleaved_work.capacity()
        + graph.roof_program_work.iter().map(Vec::capacity).sum::<usize>()
        + graph.effect_interleaved_work.capacity()
        + graph.line_work.capacity()
        + graph.steam_environment_bank.capacity();
    assert_eq!(
        telemetry.delayed_program_scratch_payload_bytes,
        (delayed_scratch_samples * size_of::<f32>()) as u64
    );
    assert_eq!(
        telemetry.rust_scratch_payload_bytes,
        (scratch_samples * size_of::<f32>()) as u64
    );
    let outer_vec_payload_bytes = graph.sources.capacity() * size_of::<NeutralSourceRenderState>()
        + graph.metadata_city_offsets.capacity() * size_of::<ApiEnuVector3>()
        + graph.metadata_city_frame_enabled.capacity() * size_of::<bool>()
        + graph.program_delays.capacity() * size_of::<NeutralProgramDelay>()
        + graph.delayed_program.capacity() * size_of::<[Vec<f32>; 2]>();
    assert_eq!(
        telemetry.outer_vec_payload_bytes,
        outer_vec_payload_bytes as u64
    );
    let source_audio_payload = graph.sources.iter().fold(0_u64, |total, source| {
        total
            + source.program_input.payload_bytes()
            + source.direct_output.payload_bytes()
            + source
                .indirect_input
                .as_ref()
                .map_or(0, OwnedAudioBuffer::payload_bytes)
            + source
                .path_field
                .as_ref()
                .map_or(0, OwnedAudioBuffer::payload_bytes)
            + source
                .reflection_scratch
                .as_ref()
                .map_or(0, OwnedAudioBuffer::payload_bytes)
            + source
                .line
                .as_ref()
                .map_or(0, |line| line.presentation.payload_bytes())
    });
    let reflection_mix_payload = graph
        .reflection_mix
        .as_ref()
        .map_or(0, OwnedAudioBuffer::payload_bytes);
    assert_eq!(
        telemetry.steam_audio_buffer_payload_bytes,
        source_audio_payload + reflection_mix_payload
    );
    assert_eq!(
        telemetry.total_tracked_payload_bytes,
        telemetry
            .program_delay_audio_history_payload_bytes
            .saturating_add(telemetry.program_delay_geometry_history_payload_bytes)
            .saturating_add(telemetry.steam_audio_buffer_payload_bytes)
            .saturating_add(telemetry.rust_scratch_payload_bytes)
            .saturating_add(telemetry.outer_vec_payload_bytes)
    );
}

fn assert_governor_memory_matches_neutral_graph(
    simulation: &MultiSourceSimulation,
    graph: &NeutralMultiSourceRenderGraph,
) {
    let graph_memory = graph.persistent_memory();
    let memory = simulation.quality_governor_telemetry().memory;
    let expected_snapshot_bytes = fightbox_runtime::SnapshotPublication::shared_payload_bytes::<
        SteamPropagationSnapshot,
    >()
    .saturating_add(fightbox_runtime::SnapshotPublication::shared_payload_bytes::<[f32; 3]>())
    .saturating_add(size_of::<[f32; 3]>() as u64)
    .saturating_add(size_of::<SteamPropagationSnapshot>() as u64)
    .saturating_add(size_of::<SteamPropagationSnapshot>() as u64)
    .saturating_add(size_of::<Option<SteamPropagationSnapshot>>() as u64)
    .saturating_add(
        fightbox_runtime::SnapshotPublication::shared_payload_bytes::<GovernorRenderSnapshot>(),
    )
    .saturating_add(size_of::<GovernorRenderSnapshot>() as u64);
    let reflection_sources = simulation.world.source_simulation_flags
        [..simulation.world.source_count]
        .iter()
        .filter(|flags| **flags & ffi::IPL_SIMULATIONFLAGS_REFLECTIONS != 0)
        .count() as u64;
    let expected_ir_bytes = reflection_sources
        * ambisonics_channel_count(simulation.config.reflection_order).unwrap() as u64
        * reflection_ir_size(
            simulation.config.reflection_duration_s,
            simulation.audio.sample_rate_hz,
        )
        .unwrap() as u64
        * size_of::<f32>() as u64;
    let expected_delay_bytes = graph_memory
        .program_delay_audio_history_payload_bytes
        .saturating_add(graph_memory.program_delay_geometry_history_payload_bytes)
        .saturating_add(bandlimited_kernel_payload_bytes());
    let expected_total = expected_snapshot_bytes
        .saturating_add(expected_ir_bytes)
        .saturating_add(graph_memory.steam_audio_buffer_payload_bytes)
        .saturating_add(graph_memory.rust_scratch_payload_bytes)
        .saturating_add(graph_memory.outer_vec_payload_bytes)
        .saturating_add(expected_delay_bytes)
        .saturating_add(simulation.world.serialized_bytes.capacity() as u64)
        .saturating_add(simulation.world.probe_grid_payload_capacity_bytes());

    assert_eq!(memory.snapshot_ring_payload_bytes, expected_snapshot_bytes);
    assert_eq!(
        memory.reflection_ir_payload_capacity_bytes,
        expected_ir_bytes
    );
    assert_eq!(
        memory.audio_buffer_payload_bytes,
        graph_memory.steam_audio_buffer_payload_bytes
    );
    assert_eq!(
        memory.render_scratch_bytes,
        graph_memory
            .rust_scratch_payload_bytes
            .saturating_add(graph_memory.outer_vec_payload_bytes)
    );
    assert_eq!(memory.propagation_delay_line_bytes, expected_delay_bytes);
    // retained_bake_bytes now covers the serialized bake plus the derived
    // probe influence grid, matching the production accounting.
    assert_eq!(
        memory.retained_bake_bytes,
        (simulation.world.serialized_bytes.capacity() as u64)
            .saturating_add(simulation.world.probe_grid_payload_capacity_bytes())
    );
    assert_eq!(memory.tracked_at_create_bytes, expected_total);
    assert_eq!(memory.tracked_current_bytes, expected_total);
    assert_eq!(memory.tracked_peak_bytes, expected_total);
}

#[test]
fn neutral_contract_rejects_invalid_shapes_orders_counts_and_geometry() {
    let cfg = config(0);
    let point_descriptor = point();
    assert!(matches!(
        validate_neutral_source_contract(cfg, &[point_descriptor], &[], 0),
        Err(BackendError::InvalidInput(_))
    ));
    assert!(matches!(
        validate_neutral_source_contract(cfg, &[point_descriptor], &[1], 3),
        Err(BackendError::InvalidInput(_))
    ));
    let mut high_reflection_order = cfg;
    high_reflection_order.reflection_order = 3;
    assert!(matches!(
        validate_neutral_source_contract(high_reflection_order, &[point_descriptor], &[1], 0),
        Err(BackendError::InvalidInput(_))
    ));
    for reflection_effect in [
        crate::ReflectionEffectConfig::PARAMETRIC,
        crate::ReflectionEffectConfig::hybrid(0.1, 0.25),
        crate::ReflectionEffectConfig::TRUE_AUDIO_NEXT_UNSUPPORTED,
    ] {
        let mut unsupported = cfg;
        unsupported.reflection_effect = reflection_effect;
        assert!(matches!(
            validate_neutral_source_contract(unsupported, &[point_descriptor], &[1], 0),
            Err(BackendError::InvalidInput(_))
        ));
    }

    let invalid_shapes = [
        (point_descriptor, 2),
        (descriptor(ExtentDescriptor::MultiPoint { count: 2 }), 2),
        (
            descriptor(ExtentDescriptor::LineSegment { length_m: 2.0 }),
            2,
        ),
        (stereo(2.0), 1),
    ];
    for (descriptor, channels) in invalid_shapes {
        assert!(matches!(
            validate_neutral_source_contract(cfg, &[descriptor], &[channels], 0),
            Err(BackendError::InvalidInput(_))
        ));
    }
    for (descriptor, channels) in [
        (point_descriptor, 1),
        (descriptor(ExtentDescriptor::MultiPoint { count: 2 }), 1),
        (
            descriptor(ExtentDescriptor::LineSegment { length_m: 2.0 }),
            1,
        ),
        (stereo(2.0), 2),
    ] {
        validate_neutral_source_contract(cfg, &[descriptor], &[channels], 0)
            .expect("valid neutral shape");
    }

    let mesh = SceneMesh::controlled_s3_corner();
    for bad_extent in [
        ExtentDescriptor::MultiPoint { count: 0 },
        ExtentDescriptor::LineSegment { length_m: 0.0 },
        ExtentDescriptor::LineSegment { length_m: f32::NAN },
        ExtentDescriptor::StereoImage { width_m: 0.0 },
        ExtentDescriptor::StereoImage {
            width_m: f32::INFINITY,
        },
    ] {
        assert!(matches!(
            validate_multi_source_config(
                &mesh,
                None,
                audio(),
                cfg,
                &[descriptor(bad_extent)],
                QualityTier::Desktop,
            ),
            Err(BackendError::InvalidInput(_))
        ));
    }
    let degenerate_pose = point_descriptor.with_initial_pose(Pose {
        position: EnuVector3::default(),
        forward: EnuVector3::default(),
        up: EnuVector3::new(0.0, 0.0, 1.0),
    });
    assert!(matches!(
        validate_multi_source_config(
            &mesh,
            None,
            audio(),
            cfg,
            &[degenerate_pose],
            QualityTier::Desktop,
        ),
        Err(BackendError::InvalidInput(_))
    ));
}

#[test]
fn guaranteed_unused_stereo_and_disabled_reflection_state_is_not_constructed() {
    let (stereo_simulation, stereo_graph) = build(&[stereo(2.0)], &[2], 0);
    let stereo_state = &stereo_graph.sources[0];
    assert!(stereo_state.path_effect.is_none());
    assert!(stereo_state.reflection_effect.is_none());
    assert!(stereo_state.indirect_input.is_none());
    assert!(stereo_state.path_field.is_none());
    assert!(stereo_state.reflection_scratch.is_none());
    assert!(stereo_graph.reflection_mixer.is_none());
    assert!(stereo_graph.reflection_mix.is_none());
    let stereo_policy = stereo_simulation.neutral_indirect_policy.unwrap();
    assert!(!stereo_policy.pathing[0]);
    assert!(!stereo_policy.reflections[0]);
    assert_eq!(
        stereo_simulation.world.source_simulation_flags[0],
        ffi::IPL_SIMULATIONFLAGS_DIRECT
    );

    let (point_simulation, point_graph) = build(&[point()], &[1], 0);
    let point_state = &point_graph.sources[0];
    assert!(point_state.path_effect.is_some());
    assert!(point_state.indirect_input.is_some());
    assert!(point_state.path_field.is_some());
    assert!(point_state.reflection_effect.is_none());
    assert!(point_state.reflection_scratch.is_none());
    assert!(point_graph.reflection_mixer.is_none());
    let point_policy = point_simulation.neutral_indirect_policy.unwrap();
    assert!(point_policy.pathing[0]);
    assert!(!point_policy.reflections[0]);
    assert_eq!(
        point_simulation.world.source_simulation_flags[0],
        ffi::IPL_SIMULATIONFLAGS_DIRECT | ffi::IPL_SIMULATIONFLAGS_PATHING
    );

    let reflected = crate::MultiSourceDescriptor::at(EnuVector3::default());
    let (reflected_simulation, reflected_graph) = build(&[reflected], &[1], 0);
    assert!(reflected_graph.sources[0].reflection_effect.is_some());
    assert!(reflected_graph.sources[0].reflection_scratch.is_some());
    assert!(reflected_graph.reflection_mixer.is_some());
    assert!(reflected_graph.reflection_mix.is_some());
    let reflected_policy = reflected_simulation.neutral_indirect_policy.unwrap();
    assert!(reflected_policy.pathing[0]);
    assert!(reflected_policy.reflections[0]);
    assert_eq!(
        reflected_simulation.world.source_simulation_flags[0],
        all_simulation_flags()
    );
    assert_governor_memory_matches_neutral_graph(&reflected_simulation, &reflected_graph);
    assert!(
        reflected_simulation
            .quality_governor_telemetry()
            .memory
            .reflection_ir_payload_capacity_bytes
            > 0
    );
}

#[test]
fn all_stereo_indirect_passes_publish_zero_without_vendor_runs_or_output_queries() {
    let descriptors = [stereo(1.0), stereo(2.0), stereo(3.0)];
    let (mut simulation, _graph) = build(&descriptors, &[2, 2, 2], 0);
    let initial_sequence = simulation.snapshot.sequence;

    simulation.run_pathing().unwrap();
    simulation
        .run_pass(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
        )
        .unwrap();

    let diagnostics = simulation.diagnostics();
    assert_eq!(diagnostics.vendor_pass_runs, [0, 0, 0]);
    assert_eq!(diagnostics.skipped_vendor_passes, [0, 1, 1]);
    assert_eq!(diagnostics.source_output_queries, [0, 0, 0]);
    assert_eq!(simulation.snapshot.sequence, initial_sequence + 2);
    for source in &simulation.snapshot.sources[..descriptors.len()] {
        assert!(source.path_sh.iter().all(|coefficient| *coefficient == 0.0));
        assert_eq!(source.path_eq, [1.0; 3]);
        assert_eq!(source.reflections, SteamReflectionParams::default());
    }
}

#[test]
fn direct_generation_advances_only_on_success_and_optional_passes_retain_it() {
    let (mut simulation, mut graph) = build(&[stereo(2.0)], &[2], 0);
    graph.prepare_for_realtime().unwrap();
    assert_eq!(simulation.latest_direct_sequence(), 0);

    let mut invalid = one_active_source_update();
    invalid.listener.linear_velocity_mps = EnuVector3::new(f32::NAN, 0.0, 0.0);
    simulation.update_inputs(&invalid);
    assert_eq!(simulation.run_direct(), Err(SimulationError::InvalidUpdate));
    assert_eq!(simulation.latest_direct_sequence(), 0);

    simulation.update_inputs(&one_active_source_update());
    simulation.run_direct().unwrap();
    let direct_sequence = simulation.latest_direct_sequence();
    let publication_sequence = simulation.snapshot.sequence;
    assert_eq!(direct_sequence, 1);

    simulation.run_pathing().unwrap();
    assert_eq!(simulation.latest_direct_sequence(), direct_sequence);
    assert_eq!(simulation.snapshot.sequence, publication_sequence + 1);
    simulation.run_reflections().unwrap();
    assert_eq!(simulation.latest_direct_sequence(), direct_sequence);
    assert_eq!(simulation.snapshot.sequence, publication_sequence + 2);

    let left = vec![0.125; BLOCK_FRAMES];
    let right = vec![-0.25; BLOCK_FRAMES];
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 2,
        program_planes: [&left, &right],
    }];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render_with_sequence(
        &mut graph,
        &source,
        0,
        direct_sequence,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
}

#[test]
fn one_generation_direct_skew_uses_history_but_two_generation_stale_fails_closed() {
    let (mut simulation, mut graph) = build(&[stereo(2.0)], &[2], 0);
    graph.prepare_for_realtime().unwrap();
    simulation.update_inputs(&one_active_source_update());
    simulation.run_direct().unwrap();
    let first_sequence = simulation.latest_direct_sequence();
    assert_eq!(first_sequence, 1);

    let left = vec![0.125; BLOCK_FRAMES];
    let right = vec![-0.25; BLOCK_FRAMES];
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 2,
        program_planes: [&left, &right],
    }];
    let mut presentation = vec![71.0; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES];
    let mut environment = vec![83.0; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES];
    let mut metadata = SpatialOutputMetadata::default();

    render_with_sequence(
        &mut graph,
        &source,
        0,
        first_sequence,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
    assert_eq!(
        graph.correlation_diagnostics(),
        NeutralCorrelationDiagnostics {
            current_direct_sequence: first_sequence,
            previous_direct_sequence: Some(0),
            history_hits: 0,
            misses: 0,
        }
    );

    simulation.run_direct().unwrap();
    let second_sequence = simulation.latest_direct_sequence();
    assert_eq!(second_sequence, first_sequence + 1);
    let frames_before_history_hit = graph.delay_instrumentation(0).unwrap().frames_processed;
    render_with_sequence(
        &mut graph,
        &source,
        BLOCK_FRAMES as u64,
        first_sequence,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
    assert_eq!(metadata.block_start_frame, BLOCK_FRAMES as u64);
    assert_eq!(
        graph.delay_instrumentation(0).unwrap().frames_processed,
        frames_before_history_hit + BLOCK_FRAMES as u64
    );
    assert_eq!(
        graph.correlation_diagnostics(),
        NeutralCorrelationDiagnostics {
            current_direct_sequence: second_sequence,
            previous_direct_sequence: Some(first_sequence),
            history_hits: 1,
            misses: 0,
        }
    );

    simulation.run_direct().unwrap();
    let third_sequence = simulation.latest_direct_sequence();
    assert_eq!(third_sequence, second_sequence + 1);
    presentation.fill(71.0);
    environment.fill(83.0);
    metadata = SpatialOutputMetadata {
        block_start_frame: 99,
        generation: 123,
        ..SpatialOutputMetadata::default()
    };
    let metadata_before = metadata;
    let delay_before = graph.delay_instrumentation(0).unwrap();
    let observation_before = graph.sources[0].last_propagation_observation;
    let quality_before = graph.sources[0].quality_gains;
    let render_active_before = graph.sources[0].render_active;
    let applied_governor_before = graph.applied_governor_quality;

    assert_eq!(
        render_with_sequence(
            &mut graph,
            &source,
            (BLOCK_FRAMES * 2) as u64,
            first_sequence,
            &mut presentation,
            &mut environment,
            &mut metadata,
        ),
        Err(SpatialBackendRenderError::PropagationSequenceMismatch)
    );
    assert!(presentation.iter().all(|sample| *sample == 71.0));
    assert!(environment.iter().all(|sample| *sample == 83.0));
    assert_eq!(metadata, metadata_before);
    assert_eq!(graph.delay_instrumentation(0).unwrap(), delay_before);
    assert_eq!(
        graph.sources[0].last_propagation_observation,
        observation_before
    );
    assert_eq!(graph.sources[0].quality_gains, quality_before);
    assert_eq!(graph.sources[0].render_active, render_active_before);
    assert_eq!(graph.applied_governor_quality, applied_governor_before);
    assert_eq!(
        graph.correlation_diagnostics(),
        NeutralCorrelationDiagnostics {
            current_direct_sequence: third_sequence,
            previous_direct_sequence: Some(second_sequence),
            history_hits: 1,
            misses: 1,
        }
    );
}

#[test]
fn skipped_direct_generation_clears_unproven_history() {
    let (mut simulation, mut graph) = build(&[point()], &[1], 0);
    let mut skipped = simulation.snapshot;
    assert_eq!(skipped.direct_sequence, 0);
    skipped.direct_sequence = 2;
    skipped.sequence = 2;
    simulation.publication.publish(skipped);

    assert_eq!(
        graph.snapshot_for_direct_sequence(0),
        Err(SpatialBackendRenderError::PropagationSequenceMismatch)
    );
    assert_eq!(
        graph.correlation_diagnostics(),
        NeutralCorrelationDiagnostics {
            current_direct_sequence: 2,
            previous_direct_sequence: None,
            history_hits: 0,
            misses: 1,
        }
    );
    assert_eq!(graph.snapshot_for_direct_sequence(2), Ok(skipped));
}

#[test]
fn direct_generation_wrap_retains_exact_adjacent_history() {
    let (_simulation, mut graph) = build(&[point()], &[1], 0);
    graph.correlation_current.direct_sequence = u64::MAX;
    graph.correlation_previous = None;
    let wrapped_previous = graph.correlation_current;

    assert_eq!(
        graph.snapshot_for_direct_sequence(u64::MAX),
        Ok(wrapped_previous)
    );
    assert_eq!(
        graph.correlation_diagnostics(),
        NeutralCorrelationDiagnostics {
            current_direct_sequence: 0,
            previous_direct_sequence: Some(u64::MAX),
            history_hits: 1,
            misses: 0,
        }
    );
}

#[test]
fn wrong_world_publication_fails_without_mutating_correlation_window() {
    let (mut simulation, mut graph) = build(&[point()], &[1], 0);
    let diagnostics_before = graph.correlation_diagnostics();
    let mut wrong_world = simulation.snapshot;
    wrong_world.world_generation = graph.world.generation.wrapping_add(1);
    wrong_world.direct_sequence = 1;
    simulation.publication.publish(wrong_world);

    assert_eq!(
        graph.snapshot_for_direct_sequence(1),
        Err(SpatialBackendRenderError::InactiveGraph)
    );
    assert_eq!(graph.correlation_diagnostics(), diagnostics_before);
}

#[test]
fn retiring_generation_requires_synchronized_direct_truth_during_swap_fade() {
    let (mut simulation, mut graph) = build(&[point()], &[1], 0);
    graph.prepare_for_realtime().unwrap();
    let pinned = graph.correlation_diagnostics();
    graph.begin_tail_retirement();

    let program = vec![0.125; BLOCK_FRAMES];
    let sources = mono_source_block(&program);
    let (mut presentation, mut environment, mut metadata) = output_banks();
    assert_eq!(
        render_with_sequence(
            &mut graph,
            &sources,
            0,
            pinned.current_direct_sequence.wrapping_add(91),
            &mut presentation,
            &mut environment,
            &mut metadata,
        ),
        Err(SpatialBackendRenderError::PropagationSequenceMismatch)
    );

    let mut synchronized = simulation.snapshot;
    synchronized.direct_sequence = pinned.current_direct_sequence.wrapping_add(1);
    simulation.publication.publish(synchronized);
    render_with_sequence(
        &mut graph,
        &sources,
        0,
        synchronized.direct_sequence,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
    assert_eq!(
        graph.correlation_diagnostics().current_direct_sequence,
        synchronized.direct_sequence
    );
}

#[test]
fn point_line_and_stereo_image_use_fixed_pre_hrtf_feed_slots() {
    let descriptors = [
        point(),
        descriptor(ExtentDescriptor::LineSegment { length_m: 2.0 }),
        stereo(2.0),
    ];
    let (_simulation, mut graph) = build(&descriptors, &[1, 1, 2], 0);
    let point_program = (0..BLOCK_FRAMES)
        .map(|frame| frame as f32 * 0.001 + 0.1)
        .collect::<Vec<_>>();
    let line_program = vec![0.2; BLOCK_FRAMES];
    let stereo_left = (0..BLOCK_FRAMES)
        .map(|frame| frame as f32 * 0.002 + 0.25)
        .collect::<Vec<_>>();
    let stereo_right = (0..BLOCK_FRAMES)
        .map(|frame| -(frame as f32 * 0.003 + 0.5))
        .collect::<Vec<_>>();
    let sources = [
        SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&point_program, &[]],
        },
        SpatialBackendSourceBlock {
            source_index: 1,
            program_plane_count: 1,
            program_planes: [&line_program, &[]],
        },
        SpatialBackendSourceBlock {
            source_index: 2,
            program_plane_count: 2,
            program_planes: [&stereo_left, &stereo_right],
        },
    ];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &sources,
        4_096,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
    assert_eq!(metadata.active_presentation_feed_count, 6);
    assert_eq!(metadata.block_start_frame, 4_096);
    assert!(!metadata.final_hrtf_applied);
    assert!(metadata.world_space_unrotated);
    assert!(metadata.source_drive_applied);
    assert!(metadata.source_safety_gain_applied);
    assert!(!metadata.monitor_gain_applied);
    assert!(!metadata.output_limiter_applied);

    let expected = [
        (
            0,
            true,
            SpatialPresentationComponent::DirectCenter,
            EnuVector3::new(0.0, 1.0, 0.0),
        ),
        (
            1,
            false,
            SpatialPresentationComponent::DirectCenter,
            EnuVector3::default(),
        ),
        (
            2,
            false,
            SpatialPresentationComponent::DirectCenter,
            EnuVector3::default(),
        ),
        (
            3,
            true,
            SpatialPresentationComponent::DirectCenter,
            EnuVector3::new(0.0, 1.0, 0.0),
        ),
        (
            4,
            true,
            SpatialPresentationComponent::WidthPositive,
            EnuVector3::new(0.0, 1.0, 0.0),
        ),
        (
            5,
            true,
            SpatialPresentationComponent::WidthNegative,
            EnuVector3::new(0.0, -1.0, 0.0),
        ),
        (
            6,
            false,
            SpatialPresentationComponent::DirectCenter,
            EnuVector3::default(),
        ),
        (
            7,
            true,
            SpatialPresentationComponent::WidthPositive,
            EnuVector3::new(1.0, 0.0, 0.0),
        ),
        (
            8,
            true,
            SpatialPresentationComponent::WidthNegative,
            EnuVector3::new(-1.0, 0.0, 0.0),
        ),
    ];
    for (slot, valid, component, direction_enu) in expected {
        let feed = metadata.presentation_feeds[slot];
        assert_eq!(feed.valid, valid, "slot {slot}");
        if valid {
            assert_eq!(feed.source_index, slot / 3);
            assert_eq!(feed.component, component);
            assert_eq!(feed.placement, SpatialFeedPlacement::Direction);
            assert_eq!(feed.direction_enu, direction_enu);
            assert_eq!(feed.latency_frames, 0);
        } else {
            assert!(
                plane(&presentation, slot)
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
        }
    }
    assert!(
        metadata
            .presentation_feeds
            .iter()
            .filter(|feed| feed.valid)
            .all(|feed| feed.component != SpatialPresentationComponent::DiscreteEcho)
    );

    let delayed = graph.delayed_program_for_source(2).unwrap();
    assert_eq!(
        delayed[0].iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        stereo_left.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
    );
    assert_eq!(
        delayed[1].iter().map(|x| x.to_bits()).collect::<Vec<_>>(),
        stereo_right.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
    );
    let instrumentation = graph.delay_instrumentation(2).unwrap();
    assert_eq!(instrumentation.frames_processed, BLOCK_FRAMES as u64);
    assert_eq!(instrumentation.trajectory_advances, BLOCK_FRAMES as u64);
    assert_eq!(instrumentation.read_plan_advances, BLOCK_FRAMES as u64);
    assert_eq!(instrumentation.channel_count_changes, 0);

    let minus = metadata.presentation_feeds[8].pose_enu.position;
    let plus = metadata.presentation_feeds[7].pose_enu.position;
    assert!((minus.east_m + 1.0).abs() < 1.0e-6, "minus={minus:?}");
    assert!((plus.east_m - 1.0).abs() < 1.0e-6, "plus={plus:?}");
    assert!((minus.north_m + plus.north_m).abs() < 1.0e-6);
    assert!((minus.up_m + plus.up_m).abs() < 1.0e-6);
    assert!(plane(&presentation, 8).iter().any(|sample| *sample > 0.0));
    assert!(plane(&presentation, 7).iter().any(|sample| *sample < 0.0));
}

#[test]
fn point_feed_directions_are_unit_cardinals_and_translation_invariant() {
    let descriptors = [
        crate::MultiSourceDescriptor::at(EnuVector3::new(3.0, 0.0, 0.0))
            .with_reflection_send(false),
        crate::MultiSourceDescriptor::at(EnuVector3::new(0.0, 3.0, 0.0))
            .with_reflection_send(false),
        crate::MultiSourceDescriptor::at(EnuVector3::new(0.0, 0.0, 3.0))
            .with_reflection_send(false),
    ];
    let (_simulation, mut graph) = build(&descriptors, &[1, 1, 1], 0);
    let program = vec![0.125; BLOCK_FRAMES];
    let sources = [
        SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&program, &[]],
        },
        SpatialBackendSourceBlock {
            source_index: 1,
            program_plane_count: 1,
            program_planes: [&program, &[]],
        },
        SpatialBackendSourceBlock {
            source_index: 2,
            program_plane_count: 1,
            program_planes: [&program, &[]],
        },
    ];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &sources,
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    for (source_index, expected) in [
        EnuVector3::new(1.0, 0.0, 0.0),
        EnuVector3::new(0.0, 1.0, 0.0),
        EnuVector3::new(0.0, 0.0, 1.0),
    ]
    .into_iter()
    .enumerate()
    {
        let feed =
            metadata.presentation_feeds[source_index * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE];
        assert_eq!(feed.placement, SpatialFeedPlacement::Direction);
        assert_eq!(feed.direction_enu, expected);
    }

    let render_translation = |translation: EnuVector3| {
        let relative = EnuVector3::new(3.0, 4.0, 0.0);
        let source_position = EnuVector3::new(
            translation.east_m + relative.east_m,
            translation.north_m + relative.north_m,
            translation.up_m + relative.up_m,
        );
        let descriptor =
            crate::MultiSourceDescriptor::at(source_position).with_reflection_send(false);
        let (mut simulation, mut graph) = build(&[descriptor], &[1], 0);
        let mut update = one_active_source_update();
        update.listener.pose.position = translation;
        update.sources[0].pose.position = source_position;
        simulation.update_inputs(&update);
        simulation.run_direct().unwrap();
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render(
            &mut graph,
            &mono_source_block(&program),
            0,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        metadata.presentation_feeds[0].direction_enu
    };
    assert_eq!(
        render_translation(EnuVector3::default()),
        render_translation(EnuVector3::new(128.0, -64.0, 32.0))
    );
}

#[test]
fn point_feed_direction_uses_the_correlated_smoothed_listener_position() {
    let source_position = EnuVector3::new(20.0, 0.0, 1.5);
    let descriptor = crate::MultiSourceDescriptor::at(source_position).with_reflection_send(false);
    let (mut simulation, mut graph) = build(&[descriptor], &[1], 0);
    let program = vec![0.125; BLOCK_FRAMES];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &mono_source_block(&program),
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    let mut update = one_active_source_update();
    update.listener.pose.position = EnuVector3::new(0.0, 10.0, 1.5);
    update.sources[0].pose.position = source_position;
    simulation.update_inputs(&update);
    simulation.run_direct().unwrap();
    render(
        &mut graph,
        &mono_source_block(&program),
        BLOCK_FRAMES as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    let smoothed = graph.sources[0].propagation_smoother.applied();
    let expected = normalized_api(EnuVector3::new(
        steam_vector_to_api(smoothed.source_position).east_m
            - steam_vector_to_api(smoothed.listener_position).east_m,
        steam_vector_to_api(smoothed.source_position).north_m
            - steam_vector_to_api(smoothed.listener_position).north_m,
        steam_vector_to_api(smoothed.source_position).up_m
            - steam_vector_to_api(smoothed.listener_position).up_m,
    ))
    .unwrap();
    let actual = metadata.presentation_feeds[0].direction_enu;
    for delta in [
        actual.east_m - expected.east_m,
        actual.north_m - expected.north_m,
        actual.up_m - expected.up_m,
    ] {
        assert!(delta.abs() <= 1.0e-6);
    }

    let latest_control_direction = normalized_api(EnuVector3::new(20.0, -10.0, 0.0)).unwrap();
    let latest_delta = (actual.east_m - latest_control_direction.east_m).abs()
        + (actual.north_m - latest_control_direction.north_m).abs()
        + (actual.up_m - latest_control_direction.up_m).abs();
    assert!(
        latest_delta > 0.01,
        "feed direction incorrectly used the latest uncorrelated control pose"
    );
}

#[test]
fn memory_telemetry_uses_live_capacities_for_one_two_and_sixteen_sources() {
    let one_history_bytes;
    let one_source_outer_vec_bytes;
    {
        let (simulation, graph) = build(&[point()], &[1], 0);
        assert_memory_matches_live_capacities(&graph);
        assert_governor_memory_matches_neutral_graph(&simulation, &graph);
        let memory = graph.persistent_memory();
        let live = graph.program_delays[0].memory();
        one_history_bytes = live.audio_history_payload_bytes;
        assert_eq!(live.geometry_history_payload_bytes, one_history_bytes);
        assert_eq!(live.additional_channel_payload_bytes, 0);
        assert_eq!(memory.source_capacity, 16);
        assert_eq!(memory.configured_source_count, 1);
        assert_eq!(memory.configured_stereo_source_count, 0);
        assert_eq!(memory.stereo_indirect_suppressed_source_count, 0);
        assert_eq!(memory.delayed_program_scratch_payload_bytes, 128 * 4);
        one_source_outer_vec_bytes = (MAX_ACTIVE_SOURCES * size_of::<NeutralSourceRenderState>()
            + size_of::<ApiEnuVector3>()
            + size_of::<bool>()
            + size_of::<NeutralProgramDelay>()
            + size_of::<[Vec<f32>; 2]>()) as u64;
        assert_eq!(memory.outer_vec_payload_bytes, one_source_outer_vec_bytes);
    }
    {
        let (simulation, graph) = build(&[stereo(2.0)], &[2], 0);
        assert_memory_matches_live_capacities(&graph);
        assert_governor_memory_matches_neutral_graph(&simulation, &graph);
        let memory = graph.persistent_memory();
        assert_eq!(memory.configured_source_count, 1);
        assert_eq!(memory.configured_stereo_source_count, 1);
        assert_eq!(memory.stereo_indirect_suppressed_source_count, 1);
        assert_eq!(
            memory.program_delay_audio_history_payload_bytes,
            2 * one_history_bytes
        );
        assert_eq!(
            memory.program_delay_geometry_history_payload_bytes,
            one_history_bytes
        );
        assert_eq!(
            memory.additional_program_channel_payload_bytes,
            one_history_bytes
        );
        assert_eq!(memory.delayed_program_scratch_payload_bytes, 2 * 128 * 4);
    }
    {
        let descriptors = [point(), stereo(1.0)];
        let (simulation, graph) = build(&descriptors, &[1, 2], 0);
        assert_memory_matches_live_capacities(&graph);
        assert_governor_memory_matches_neutral_graph(&simulation, &graph);
        let memory = graph.persistent_memory();
        assert_eq!(memory.configured_source_count, 2);
        assert_eq!(memory.configured_stereo_source_count, 1);
        assert_eq!(memory.stereo_indirect_suppressed_source_count, 1);
        assert_eq!(
            memory.program_delay_audio_history_payload_bytes,
            3 * one_history_bytes
        );
        assert_eq!(
            memory.program_delay_geometry_history_payload_bytes,
            2 * one_history_bytes
        );
        assert_eq!(
            memory.additional_program_channel_payload_bytes,
            one_history_bytes
        );
        assert_eq!(memory.delayed_program_scratch_payload_bytes, 3 * 128 * 4);
        assert_eq!(
            memory.outer_vec_payload_bytes,
            one_source_outer_vec_bytes
                + size_of::<ApiEnuVector3>() as u64
                + size_of::<bool>() as u64
                + size_of::<NeutralProgramDelay>() as u64
                + size_of::<[Vec<f32>; 2]>() as u64
        );
    }
    {
        let descriptors = [stereo(2.0); MAX_ACTIVE_SOURCES];
        let channels = [2; MAX_ACTIVE_SOURCES];
        let (simulation, graph) = build(&descriptors, &channels, 0);
        assert_memory_matches_live_capacities(&graph);
        assert_governor_memory_matches_neutral_graph(&simulation, &graph);
        let memory = graph.persistent_memory();
        assert_eq!(memory.configured_source_count, 16);
        assert_eq!(memory.configured_stereo_source_count, 16);
        assert_eq!(memory.stereo_indirect_suppressed_source_count, 16);
        assert_eq!(
            memory.program_delay_audio_history_payload_bytes,
            32 * one_history_bytes
        );
        assert_eq!(
            memory.program_delay_geometry_history_payload_bytes,
            16 * one_history_bytes
        );
        assert_eq!(
            memory.additional_program_channel_payload_bytes,
            16 * one_history_bytes
        );
        assert_eq!(
            memory.delayed_program_scratch_payload_bytes,
            16 * 2 * 128 * 4
        );
        assert_eq!(
            memory.outer_vec_payload_bytes,
            (MAX_ACTIVE_SOURCES
                * (size_of::<NeutralSourceRenderState>()
                    + size_of::<ApiEnuVector3>()
                    + size_of::<bool>()
                    + size_of::<NeutralProgramDelay>()
                    + size_of::<[Vec<f32>; 2]>())) as u64
        );
        eprintln!(
            "NEUTRAL_STEREO_MEMORY one_source_extra_bytes={} one_source_extra_mib={:.9} all_16_extra_bytes={} all_16_extra_mib={:.9}",
            one_history_bytes,
            one_history_bytes as f64 / 1_048_576.0,
            memory.additional_program_channel_payload_bytes,
            memory.additional_program_channel_payload_bytes as f64 / 1_048_576.0,
        );
    }
}

fn publish_path_snapshot(
    mut simulation: MultiSourceSimulation,
    graph: &mut NeutralMultiSourceRenderGraph,
    coefficients: &[f32],
) {
    let mut snapshot = simulation.snapshot;
    snapshot.sources[0].path_eq = [1.0; 3];
    snapshot.sources[0].path_sh.fill(0.0);
    snapshot.sources[0].path_sh[..coefficients.len()].copy_from_slice(coefficients);
    snapshot.sources[0].configured_pathing_order = graph.path_order as u8;
    simulation.snapshot = snapshot;
    simulation.publication.publish(snapshot);
    drop(simulation);
    Arc::get_mut(&mut graph.world)
        .expect("render graph retains the only world Arc")
        .has_baked_pathing = true;
    graph.sources[0].quality_gains[1] = 1.0;
}

#[test]
fn environmental_orders_emit_exact_active_acn_n3d_prefix_with_zero_latency() {
    let coefficients = [
        1.0_f32, -0.5, 0.25, -0.125, 0.0625, -0.03125, 0.015625, -0.0078125, 0.00390625,
    ];
    for order in 0..=2 {
        let (simulation, mut graph) = build(&[point()], &[1], order);
        let channels = (order + 1) * (order + 1);
        publish_path_snapshot(simulation, &mut graph, &coefficients[..channels]);
        let input = vec![0.25; BLOCK_FRAMES];
        let source = [SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render(
            &mut graph,
            &source,
            0,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();

        assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
        assert_eq!(metadata.active_environmental_order.order(), Some(order));
        assert_eq!(metadata.active_environmental_plane_count, channels);
        assert_eq!(metadata.environmental_latency_frames, 0);
        assert_eq!(
            metadata.environmental_channel_order,
            SpatialAmbisonicChannelOrder::Acn
        );
        assert_eq!(
            metadata.environmental_normalization,
            SpatialAmbisonicNormalization::N3d
        );
        assert_eq!(
            metadata.environmental_basis,
            SpatialEnvironmentalBasis::RightHandedXRightYUpZBack
        );
        assert!(metadata.world_space_unrotated);

        let reference = plane(&environment, 0)[BLOCK_FRAMES - 1];
        assert!(
            reference.abs() > 1.0e-6,
            "order {order} path field was silent"
        );
        for channel in 0..channels {
            let actual = plane(&environment, channel)[BLOCK_FRAMES - 1] / reference;
            let expected = coefficients[channel] / coefficients[0];
            assert!(
                (actual - expected).abs() < 2.0e-4,
                "order {order} channel {channel}: {actual} != {expected}"
            );
        }
        for channel in channels..MAX_SPATIAL_ENVIRONMENT_PLANES {
            assert!(
                plane(&environment, channel)
                    .iter()
                    .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
            );
        }
    }
}

#[test]
fn stereo_indirect_suppression_is_machine_readable_and_cannot_contaminate_environment() {
    let (mut simulation, mut graph) = build(&[stereo(2.0)], &[2], 2);
    let mut snapshot = simulation.snapshot;
    snapshot.sources[0].path_eq = [1.0; 3];
    snapshot.sources[0].path_sh = [1.0; crate::backend_snapshot::MAX_PATH_SH_COEFFS];
    simulation.snapshot = snapshot;
    simulation.publication.publish(snapshot);
    drop(simulation);
    Arc::get_mut(&mut graph.world).unwrap().has_baked_pathing = true;
    graph.sources[0].quality_gains = [1.0; 3];

    let memory = graph.persistent_memory();
    assert_eq!(memory.configured_stereo_source_count, 1);
    assert_eq!(memory.stereo_indirect_suppressed_source_count, 1);
    let left = vec![0.4; BLOCK_FRAMES];
    let right = vec![-0.2; BLOCK_FRAMES];
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 2,
        program_planes: [&left, &right],
    }];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &source,
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert!(
        environment
            .iter()
            .all(|sample| sample.to_bits() == 0.0_f32.to_bits())
    );
    assert_eq!(metadata.active_presentation_feed_count, 2);
}

#[test]
fn physical_time_of_flight_is_content_timing_not_feed_latency() {
    let source_position = EnuVector3::new(1.0, 0.0, 0.0);
    let descriptor = crate::MultiSourceDescriptor::at(source_position).with_reflection_send(false);
    let (_simulation, mut graph) = build(&[descriptor], &[1], 0);
    let mut captured = Vec::new();
    for block_index in 0..3 {
        let mut input = vec![0.0; BLOCK_FRAMES];
        if block_index == 0 {
            input[0] = 1.0;
        }
        let source = [SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render(
            &mut graph,
            &source,
            (block_index * BLOCK_FRAMES) as u64,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert_eq!(metadata.presentation_feeds[0].latency_frames, 0);
        captured.extend_from_slice(plane(&presentation, 0));
    }
    let onset = captured
        .iter()
        .position(|sample| sample.abs() > 1.0e-7)
        .expect("propagated impulse onset");
    let physical_delay = SAMPLE_RATE_HZ as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
    assert!(
        (onset as f32 - physical_delay).abs() <= 3.0,
        "onset={onset}, physical_delay={physical_delay}"
    );
    assert!(onset > 0);
}

#[test]
fn environmental_path_impulse_has_no_algorithmic_lookahead_latency() {
    let (simulation, mut graph) = build(&[point()], &[1], 0);
    publish_path_snapshot(simulation, &mut graph, &[1.0]);
    let mut input = vec![0.0; BLOCK_FRAMES];
    input[0] = 1.0;
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 1,
        program_planes: [&input, &[]],
    }];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &source,
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    let onset = plane(&environment, 0)
        .iter()
        .position(|sample| sample.abs() > 1.0e-7)
        .expect("path impulse onset");
    assert_eq!(onset, NEUTRAL_ENVIRONMENTAL_LATENCY_FRAMES as usize);
    assert_eq!(metadata.environmental_latency_frames, onset as u32);
}

fn zero_distance_reflector_mesh() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            crate::EnuVector3::new(-80.0, -80.0, 0.0),
            crate::EnuVector3::new(80.0, -80.0, 0.0),
            crate::EnuVector3::new(80.0, 80.0, 0.0),
            crate::EnuVector3::new(-80.0, 80.0, 0.0),
        ],
        triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
        material_indices: vec![0; 4],
        materials: vec![crate::AcousticMaterial::MASONRY],
    }
}

fn relative_response_onset(response: &[f32]) -> usize {
    let peak = response.iter().copied().map(f32::abs).fold(0.0, f32::max);
    assert!(peak > 0.0, "reflection response must contain energy");
    response
        .iter()
        .position(|sample| sample.abs() > peak * 1.0e-3)
        .expect("nonzero reflection response has an onset")
}

fn response_similarity(reference: &[f32], candidate: &[f32]) -> (f64, f64) {
    assert_eq!(reference.len(), candidate.len());
    let (dot, reference_energy, candidate_energy) = reference.iter().zip(candidate).fold(
        (0.0_f64, 0.0_f64, 0.0_f64),
        |(dot, reference_energy, candidate_energy), (reference, candidate)| {
            let reference = f64::from(*reference);
            let candidate = f64::from(*candidate);
            (
                dot + reference * candidate,
                reference_energy + reference * reference,
                candidate_energy + candidate * candidate,
            )
        },
    );
    assert!(reference_energy > 0.0);
    assert!(candidate_energy > 0.0);
    (
        dot / (reference_energy.sqrt() * candidate_energy.sqrt()),
        10.0 * (candidate_energy / reference_energy).log10(),
    )
}

#[test]
fn reflection_graph_matches_raw_steam_ir_with_zero_algorithmic_latency() {
    let source_position = EnuVector3::default();
    let descriptor = crate::MultiSourceDescriptor::at(source_position);
    let cfg = S3SimulationConfig {
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 2,
        reflection_duration_s: 0.15,
        reflection_order: 1,
        pathing_order: 0,
        ..S3SimulationConfig::default()
    };
    let (mut simulation, mut graph) = build_neutral_multi_source_generation(
        &zero_distance_reflector_mesh(),
        None,
        audio(),
        cfg,
        &[descriptor],
        &[1],
        0,
        23,
        QualityTier::Desktop,
    )
    .unwrap();
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
    }
    let mut motions = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    motions[0] = SourceMotion {
        active: true,
        pose: default_api_pose(source_position),
        linear_velocity_mps: EnuVector3::default(),
    };
    let update = SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: default_api_pose(source_position),
            linear_velocity_mps: EnuVector3::default(),
        },
        sources: motions,
    };
    simulation.update_inputs(&update);
    for _ in 0..4 {
        simulation.run_reflections().unwrap();
    }
    let mut reflection = simulation.snapshot.sources[0].reflections;
    assert_ne!(reflection.ir, 0);
    assert!(reflection.ir_size > 0);
    assert_eq!(reflection.num_channels, 4);

    let context = simulation.world.context();
    let standalone_effect = handle(
        graph.sources[0]
            .reflection_effect
            .as_ref()
            .expect("reflection-enabled source owns an effect")
            .0,
    );
    let graph_mixer = handle(
        graph
            .reflection_mixer
            .as_ref()
            .expect("reflection-enabled graph owns a mixer")
            .0,
    );
    let mut program = vec![0.0; BLOCK_FRAMES];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    // Prime the graph's publication reader and settle its no-bake path fade
    // with silence before either latency response is captured.
    render(
        &mut graph,
        &mono_source_block(&program),
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    ffi::reflection_effect_reset(standalone_effect);
    ffi::reflection_mixer_reset(graph_mixer);
    simulation
        .run_pass(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
        )
        .unwrap();
    reflection = simulation.snapshot.sources[0].reflections;

    let mut standalone_input = OwnedAudioBuffer::allocate(context, 1, audio().frame_size).unwrap();
    let mut standalone_output =
        OwnedAudioBuffer::allocate(context, reflection.num_channels, audio().frame_size).unwrap();
    let mut standalone_interleaved = vec![0.0; BLOCK_FRAMES * reflection.num_channels as usize];
    let blocks = reflection.ir_size as usize / BLOCK_FRAMES + 4;
    // Steam keeps IPLReflectionEffectIR opaque. For convolution, the response
    // of a reset effect to a unit sample is the observable raw IR: δ * h = h.
    // This probe has no program delay, reflection mixer, or neutral graph.
    let mut raw_ir_response = Vec::with_capacity(blocks * BLOCK_FRAMES);
    for block in 0..blocks {
        program.fill(0.0);
        if block == 0 {
            program[0] = 1.0;
        }

        standalone_input.write_mono(&mut program);
        let mut raw_input = standalone_input.raw();
        let mut raw_output = standalone_output.raw();
        let mut params = reflection_effect_params(reflection, cfg);
        ffi::reflection_effect_apply(
            standalone_effect,
            &mut params,
            &mut raw_input,
            &mut raw_output,
        );
        standalone_output.read_interleaved(&mut standalone_interleaved);
        raw_ir_response.extend(
            standalone_interleaved
                .chunks_exact(reflection.num_channels as usize)
                .map(|frame| frame[0]),
        );
    }
    ffi::reflection_effect_reset(standalone_effect);
    simulation
        .run_pass(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
        )
        .unwrap();
    let mixer_reflection = simulation.snapshot.sources[0].reflections;
    let mut audio_settings = raw_audio_settings(audio());
    let mut mixer_settings = ffi::IPLReflectionEffectSettings {
        type_: reflection_effect_ffi_type(cfg.reflection_effect.effect_type).unwrap(),
        irSize: reflection.ir_size,
        numChannels: reflection.num_channels,
    };
    let mut manual_mixer = core::ptr::null_mut();
    assert_eq!(
        ffi::reflection_mixer_create(
            context,
            &mut audio_settings,
            &mut mixer_settings,
            &mut manual_mixer,
        ),
        ffi::IPL_STATUS_SUCCESS
    );
    let mut mixer_output =
        OwnedAudioBuffer::allocate(context, reflection.num_channels, audio().frame_size).unwrap();
    let mut mixer_interleaved = vec![0.0; BLOCK_FRAMES * reflection.num_channels as usize];
    let mut mixer_response = Vec::with_capacity(blocks * BLOCK_FRAMES);
    for block in 0..blocks {
        program.fill(0.0);
        if block == 0 {
            program[0] = 1.0;
        }
        standalone_input.write_mono(&mut program);
        let mut raw_input = standalone_input.raw();
        let mut scratch = standalone_output.raw();
        let mut params = reflection_effect_params(mixer_reflection, cfg);
        ffi::reflection_effect_apply_to_mixer(
            standalone_effect,
            &mut params,
            &mut raw_input,
            &mut scratch,
            manual_mixer,
        );
        let mut raw_mixer_output = mixer_output.raw();
        ffi::reflection_mixer_apply(manual_mixer, &mut params, &mut raw_mixer_output);
        mixer_output.read_interleaved(&mut mixer_interleaved);
        mixer_response.extend(
            mixer_interleaved
                .chunks_exact(reflection.num_channels as usize)
                .map(|frame| frame[0]),
        );
    }
    ffi::reflection_mixer_release(&mut manual_mixer);
    ffi::reflection_effect_reset(standalone_effect);
    simulation
        .run_pass(
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            GovernorSimulationPass::Reflections,
        )
        .unwrap();

    let mut graph_response = Vec::with_capacity(blocks * BLOCK_FRAMES);

    for block in 0..blocks {
        program.fill(0.0);
        if block == 0 {
            program[0] = 1.0;
        }
        render(
            &mut graph,
            &mono_source_block(&program),
            ((block + 1) * BLOCK_FRAMES) as u64,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert_eq!(metadata.environmental_latency_frames, 0);
        graph_response.extend_from_slice(plane(&environment, 0));
    }

    let raw_ir_onset = relative_response_onset(&raw_ir_response);
    let mixer_onset = relative_response_onset(&mixer_response);
    let graph_onset = relative_response_onset(&graph_response);
    // Source, listener, and double-sided reflector are co-located, so geometric
    // travel time is zero. Steam 4.8.1's first measurable coefficient is at
    // sample 1, within one sample of that analytic onset. The nonzero raw-IR
    // bin is content placement; graph latency is measured relative to it.
    assert!(
        raw_ir_onset.abs_diff(0) <= 1,
        "zero-distance raw Steam IR onset was {raw_ir_onset}, expected 0 ± 1 frame"
    );
    assert_eq!(
        mixer_onset as isize - raw_ir_onset as isize,
        0,
        "ReflectionMixer added algorithmic latency"
    );
    assert_eq!(
        graph_onset as isize - raw_ir_onset as isize,
        0,
        "the neutral reflection graph added algorithmic latency"
    );
    let (mixer_correlation, mixer_energy_delta_db) =
        response_similarity(&raw_ir_response, &mixer_response);
    let (graph_correlation, graph_energy_delta_db) =
        response_similarity(&raw_ir_response, &graph_response);
    assert!(
        mixer_correlation > 0.999_999,
        "unaligned mixer/raw-IR response: correlation={mixer_correlation:.9}"
    );
    assert!(
        graph_correlation > 0.999_999,
        "unaligned graph/raw-IR response: correlation={graph_correlation:.9}"
    );
    assert!(
        mixer_energy_delta_db.abs() < 0.001,
        "mixer/raw-IR energy delta was {mixer_energy_delta_db:+.9} dB"
    );
    assert!(
        graph_energy_delta_db.abs() < 0.001,
        "graph/raw-IR energy delta was {graph_energy_delta_db:+.9} dB"
    );
}

#[test]
fn supplied_set_is_active_authority_and_explicit_zero_advances_reactivation_state() {
    let descriptor = point().with_initially_active(false);
    let (_simulation, mut graph) = build(&[descriptor], &[1], 0);
    let full = vec![0.5; BLOCK_FRAMES];
    let zero = vec![0.0; BLOCK_FRAMES];
    let (mut presentation, mut environment, mut metadata) = output_banks();

    render(
        &mut graph,
        &mono_source_block(&full),
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.active_presentation_feed_count, 1);
    assert!(
        plane(&presentation, 0)
            .iter()
            .any(|sample| sample.abs() > 1.0e-7),
        "a supplied source must render despite a lagging inactive Steam snapshot"
    );
    assert!(graph.sources[0].render_active);

    render(
        &mut graph,
        &[],
        BLOCK_FRAMES as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert!(!graph.sources[0].render_active);
    let NeutralProgramDelay::Mono(delay) = &graph.program_delays[0] else {
        panic!("point source owns a mono propagation delay");
    };
    assert!(delay.guard_reactivation_history);
    assert_eq!(delay.reactivation_epoch_samples, 0);

    render(
        &mut graph,
        &[],
        (2 * BLOCK_FRAMES) as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    let NeutralProgramDelay::Mono(delay) = &graph.program_delays[0] else {
        unreachable!()
    };
    assert!(delay.guard_reactivation_history);
    assert_eq!(delay.reactivation_epoch_samples, 0);

    render(
        &mut graph,
        &mono_source_block(&zero),
        (3 * BLOCK_FRAMES) as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert_eq!(metadata.active_presentation_feed_count, 1);
    assert!(presentation.iter().all(|sample| sample.abs() <= 1.0e-7));
    let NeutralProgramDelay::Mono(delay) = &graph.program_delays[0] else {
        unreachable!()
    };
    assert!(
        !delay.guard_reactivation_history,
        "explicit zero PCM is an active block and must advance guarded history"
    );
    assert!(graph.sources[0].render_active);
}

#[test]
fn reactivation_guard_releases_valid_audio_inside_the_first_ready_block() {
    let source_position = EnuVector3::new(1.0, 0.0, 0.0);
    let descriptor = crate::MultiSourceDescriptor::at(source_position).with_reflection_send(false);
    let (_simulation, mut graph) = build(&[descriptor], &[1], 0);
    let full = vec![0.75; BLOCK_FRAMES];
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 1,
        program_planes: [&full, &[]],
    }];
    let (mut presentation, mut environment, mut metadata) = output_banks();

    render(
        &mut graph,
        &[],
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    render(
        &mut graph,
        &source,
        BLOCK_FRAMES as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    assert!(
        graph.delayed_program_for_source(0).unwrap()[0]
            .iter()
            .all(|sample| sample.to_bits() == 0)
    );

    render(
        &mut graph,
        &source,
        (2 * BLOCK_FRAMES) as u64,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();
    let delayed_onset = graph.delayed_program_for_source(0).unwrap()[0]
        .iter()
        .position(|sample| sample.abs() > 1.0e-7)
        .expect("newly written program history becomes valid inside this block");
    let presentation_onset = plane(&presentation, 0)
        .iter()
        .position(|sample| sample.abs() > 1.0e-7)
        .expect("valid delayed samples are rendered in the same callback");
    assert!(delayed_onset < BLOCK_FRAMES);
    assert_eq!(presentation_onset, delayed_onset);
}

#[test]
fn repeated_mono_reactivation_cannot_expose_retained_program_history() {
    let initial_position = EnuVector3::new(10.0, 0.0, 0.0);
    let descriptor = crate::MultiSourceDescriptor::at(initial_position).with_reflection_send(false);
    let (mut simulation, mut graph) = build(&[descriptor], &[1], 0);
    let full = vec![0.75; BLOCK_FRAMES];
    let zero = vec![0.0; BLOCK_FRAMES];
    let mut block_start = 0_u64;

    let render_program = |graph: &mut NeutralMultiSourceRenderGraph,
                          program: &[f32],
                          block_start: u64|
     -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let source = [SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [program, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render(
            graph,
            &source,
            block_start,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        (
            graph.delayed_program_for_source(0).unwrap()[0].to_vec(),
            plane(&presentation, 0).to_vec(),
            environment,
        )
    };
    let render_omitted = |graph: &mut NeutralMultiSourceRenderGraph,
                          block_start: u64|
     -> (Vec<f32>, Vec<f32>, Vec<f32>) {
        let (mut presentation, mut environment, mut metadata) = output_banks();
        render(
            graph,
            &[],
            block_start,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
        .unwrap();
        assert_eq!(metadata.active_presentation_feed_count, 0);
        (
            graph.delayed_program_for_source(0).unwrap()[0].to_vec(),
            presentation,
            environment,
        )
    };
    let publish = |simulation: &mut MultiSourceSimulation, active: bool, position: EnuVector3| {
        let mut snapshot = simulation.snapshot;
        snapshot.sequence = snapshot.sequence.wrapping_add(1);
        snapshot.sources[0].active = active;
        snapshot.sources[0].source_position = api_enu_to_steam(position);
        simulation.snapshot = snapshot;
        simulation.publication.publish(snapshot);
    };

    // Fill beyond the initial 10 m propagation time so the retained ring has
    // definitely audible history at every nearby reactivation read head.
    for _ in 0..14 {
        render_program(&mut graph, &full, block_start);
        block_start += BLOCK_FRAMES as u64;
    }

    for (reactivated_position, guarded_blocks) in [
        (EnuVector3::new(1.0, 0.0, 0.0), 2_usize),
        (EnuVector3::new(2.0, 0.0, 0.0), 3_usize),
    ] {
        publish(&mut simulation, false, reactivated_position);
        // The callback source set is authoritative: one retained-term edge is
        // still rendered even though Steam's control snapshot is already
        // inactive. Omission on the next block performs the transition reset.
        render_program(&mut graph, &full, block_start);
        block_start += BLOCK_FRAMES as u64;
        let (delayed, presentation, environment) = render_omitted(&mut graph, block_start);
        block_start += BLOCK_FRAMES as u64;
        assert!(delayed.iter().all(|sample| sample.to_bits() == 0));
        assert!(presentation.iter().all(|sample| sample.to_bits() == 0));
        assert!(environment.iter().all(|sample| sample.to_bits() == 0));

        for guarded_block in 0..guarded_blocks {
            if guarded_block == 1 {
                publish(&mut simulation, true, reactivated_position);
            }
            let (delayed, presentation, environment) =
                render_program(&mut graph, &zero, block_start);
            block_start += BLOCK_FRAMES as u64;
            assert!(delayed.iter().all(|sample| sample.to_bits() == 0));
            let presentation_peak = presentation
                .iter()
                .copied()
                .map(f32::abs)
                .fold(0.0_f32, f32::max);
            assert!(
                presentation_peak <= 1.0e-5,
                "guarded block {guarded_block} retained presentation peak {presentation_peak:e}"
            );
            assert!(environment.iter().all(|sample| sample.abs() <= 1.0e-7));
        }

        // Refill the nearby history before the next deactivate/reactivate
        // cycle, making the repeated-cycle assertion meaningful.
        for _ in 0..4 {
            render_program(&mut graph, &full, block_start);
            block_start += BLOCK_FRAMES as u64;
        }
    }
}

#[test]
fn malformed_callback_blocks_return_typed_errors() {
    let descriptors = [point(), point()];
    let (_simulation, mut graph) = build(&descriptors, &[1, 1], 0);
    let good = vec![0.1; BLOCK_FRAMES];
    let short = vec![0.1; BLOCK_FRAMES - 1];
    let extra = vec![0.2; BLOCK_FRAMES];
    let nan = vec![f32::NAN; BLOCK_FRAMES];
    let valid = SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 1,
        program_planes: [&good, &[]],
    };

    let mut attempt = |sources: &[SpatialBackendSourceBlock<'_>], p_len: usize, e_len: usize| {
        let mut presentation = vec![0.0; p_len];
        let mut environment = vec![0.0; e_len];
        let mut metadata = SpatialOutputMetadata::default();
        render(
            &mut graph,
            sources,
            0,
            &mut presentation,
            &mut environment,
            &mut metadata,
        )
    };
    assert_eq!(
        attempt(
            &[valid],
            MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES - 1,
            MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES,
        ),
        Err(SpatialBackendRenderError::InvalidBlockLength)
    );
    assert_eq!(
        attempt(
            &[valid],
            MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES,
            MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES - 1,
        ),
        Err(SpatialBackendRenderError::InvalidBlockLength)
    );
    let out_of_range = SpatialBackendSourceBlock {
        source_index: 2,
        ..valid
    };
    assert_eq!(
        attempt(
            &[out_of_range],
            MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES,
            MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES,
        ),
        Err(SpatialBackendRenderError::InvalidSourceIndex)
    );
    assert_eq!(
        attempt(
            &[valid, valid],
            MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES,
            MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES,
        ),
        Err(SpatialBackendRenderError::InvalidSourceIndex)
    );
    for malformed in [
        SpatialBackendSourceBlock {
            program_plane_count: 2,
            program_planes: [&good, &extra],
            ..valid
        },
        SpatialBackendSourceBlock {
            program_planes: [&short, &[]],
            ..valid
        },
        SpatialBackendSourceBlock {
            program_planes: [&good, &extra],
            ..valid
        },
        SpatialBackendSourceBlock {
            program_planes: [&nan, &[]],
            ..valid
        },
    ] {
        let expected = if malformed.program_plane_count == 2 {
            SpatialBackendRenderError::InvalidProgramPlaneCount
        } else {
            SpatialBackendRenderError::InvalidBlockLength
        };
        assert_eq!(
            attempt(
                &[malformed],
                MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_FRAMES,
                MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_FRAMES,
            ),
            Err(expected)
        );
    }
}

#[test]
fn steady_state_neutral_callback_performs_zero_rust_allocations() {
    let descriptors = [point(), stereo(2.0)];
    let (_simulation, mut graph) = build(&descriptors, &[1, 2], 0);
    let mono = vec![0.1; BLOCK_FRAMES];
    let left = vec![0.2; BLOCK_FRAMES];
    let right = vec![-0.3; BLOCK_FRAMES];
    let sources = [
        SpatialBackendSourceBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&mono, &[]],
        },
        SpatialBackendSourceBlock {
            source_index: 1,
            program_plane_count: 2,
            program_planes: [&left, &right],
        },
    ];
    let (mut presentation, mut environment, mut metadata) = output_banks();
    render(
        &mut graph,
        &sources,
        0,
        &mut presentation,
        &mut environment,
        &mut metadata,
    )
    .unwrap();

    let mut block_start_frame = BLOCK_FRAMES as u64;
    let allocations = crate::propagation_delay_stereo_tests::count_allocations(|| {
        for _ in 0..16 {
            render(
                &mut graph,
                &sources,
                block_start_frame,
                &mut presentation,
                &mut environment,
                &mut metadata,
            )
            .unwrap();
            block_start_frame += BLOCK_FRAMES as u64;
        }
    });
    assert_eq!(allocations, 0);
}

#[test]
fn partial_neutral_construction_releases_every_acquired_handle() {
    let mesh = SceneMesh::controlled_s3_corner();
    let cfg = config(0);
    let descriptors = [crate::MultiSourceDescriptor::at(EnuVector3::default())];
    let (simulation, reader, governor) = build_simulation_generation(
        &mesh,
        None,
        audio(),
        cfg,
        &descriptors,
        99,
        QualityTier::Desktop,
        None,
    )
    .unwrap();

    reset_neutral_release_counts();
    let result = create_neutral_render_graph(
        Arc::clone(&simulation.world),
        audio(),
        cfg,
        reader,
        governor,
        &descriptors,
        &[2],
        0,
    );
    assert!(matches!(result, Err(BackendError::InvalidInput(_))));
    assert_eq!(
        neutral_release_counts(),
        NeutralReleaseCounts {
            mixer: 1,
            ..NeutralReleaseCounts::default()
        }
    );

    let mut invalid_effect = cfg;
    invalid_effect.reflection_effect = crate::ReflectionEffectConfig {
        effect_type: ReflectionEffectType::TrueAudioNext,
        hybrid_transition_time_s: None,
        hybrid_overlap_percent: None,
    };
    let mut audio_settings = raw_audio_settings(audio());
    reset_neutral_release_counts();
    let result = create_neutral_source_render_state(
        simulation.world.context(),
        &mut audio_settings,
        invalid_effect,
        reflection_ir_size(cfg.reflection_duration_s, SAMPLE_RATE_HZ).unwrap(),
        1,
        0,
        1,
        NeutralPresentationShape::Point,
        fightbox_api::ImpulseClass::None,
        true,
        true,
    );
    assert!(matches!(result, Err(BackendError::InvalidInput(_))));
    assert_eq!(
        neutral_release_counts(),
        NeutralReleaseCounts {
            direct: 1,
            path: 1,
            ..NeutralReleaseCounts::default()
        }
    );
}
