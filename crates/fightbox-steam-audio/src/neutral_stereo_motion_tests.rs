use super::*;
use crate::motion_smoothing::SPEED_OF_SOUND_METERS_PER_SECOND;
use crate::propagation_delay::{
    StereoProgramDelayInstrumentation, StereoProgramPropagationDelay, TELEPORT_CROSSFADE_SECONDS,
};
use std::f32::consts::TAU;

const STEREO_WIDTH_M: f32 = 4.0;
const LEFT_TONE_HZ: f32 = 997.0;
const RIGHT_TONE_HZ: f32 = 1_499.0;
const START_DISTANCE_M: f32 = 250.0;
const LISTENER_UP_M: f32 = 10.0;
const STATIC_PREHISTORY_BLOCKS: usize = 300;
const MOTION_BLOCKS: usize = 500;
const MOTION_CAPTURE_FROM_BLOCK: usize = 380;
const ISOLATED_MARKER_FRAMES: usize = 512;
const MARKER_PULSE_FRAMES: usize = 16;

fn stereo_descriptor(position: EnuVector3) -> crate::MultiSourceDescriptor {
    crate::MultiSourceDescriptor::at(position)
        .with_extent(ExtentDescriptor::StereoImage {
            width_m: STEREO_WIDTH_M,
        })
        .with_reflection_send(false)
}

fn stereo_pair(position: EnuVector3) -> (MultiSourceSimulation, NeutralMultiSourceRenderGraph) {
    build(&[stereo_descriptor(position)], &[2], 0)
}

fn one_source_update(position: EnuVector3, velocity_mps: EnuVector3) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: default_api_pose(position),
        linear_velocity_mps: velocity_mps,
    };
    SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: default_api_pose(EnuVector3::new(0.0, 0.0, LISTENER_UP_M)),
            linear_velocity_mps: EnuVector3::default(),
        },
        sources,
    }
}

fn stereo_delay(graph: &NeutralMultiSourceRenderGraph) -> &StereoProgramPropagationDelay {
    match &graph.program_delays[0] {
        NeutralProgramDelay::Stereo(delay) => delay,
        NeutralProgramDelay::Mono(_) => panic!("StereoImage must own one shared stereo delay"),
    }
}

fn assert_single_frame_clock_advance(
    before: StereoProgramDelayInstrumentation,
    after: StereoProgramDelayInstrumentation,
) {
    assert_eq!(
        after.frames_processed - before.frames_processed,
        BLOCK_FRAMES as u64
    );
    assert_eq!(
        after.trajectory_advances - before.trajectory_advances,
        BLOCK_FRAMES as u64
    );
    assert_eq!(
        after.read_plan_advances - before.read_plan_advances,
        BLOCK_FRAMES as u64
    );
    assert_eq!(
        after.channel_count_changes, before.channel_count_changes,
        "a fixed V2 two-plane source cannot change channel count"
    );
}

#[allow(clippy::too_many_arguments)]
fn render_stereo_block(
    simulation: &mut MultiSourceSimulation,
    graph: &mut NeutralMultiSourceRenderGraph,
    position: EnuVector3,
    velocity_mps: EnuVector3,
    left: &[f32; BLOCK_FRAMES],
    right: &[f32; BLOCK_FRAMES],
    block_start_frame: u64,
    presentation: &mut [f32],
    environment: &mut [f32],
    metadata: &mut SpatialOutputMetadata,
) {
    simulation.update_inputs(&one_source_update(position, velocity_mps));
    simulation
        .run_direct()
        .expect("real linked Steam direct simulation pass");
    let source = [SpatialBackendSourceBlock {
        source_index: 0,
        program_plane_count: 2,
        program_planes: [&left[..], &right[..]],
    }];
    let before = graph
        .delay_instrumentation(0)
        .expect("StereoImage delay instrumentation");
    render(
        graph,
        &source,
        block_start_frame,
        presentation,
        environment,
        metadata,
    )
    .expect("real linked neutral StereoImage render");
    let after = graph
        .delay_instrumentation(0)
        .expect("StereoImage delay instrumentation");
    assert_single_frame_clock_advance(before, after);

    assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
    assert_eq!(metadata.sample_rate_hz, SAMPLE_RATE_HZ as u32);
    assert_eq!(metadata.block_size_frames, BLOCK_FRAMES as u32);
    assert_eq!(metadata.block_start_frame, block_start_frame);
    assert_eq!(metadata.active_presentation_feed_count, 2);
    assert!(!metadata.presentation_feeds[0].valid);
    assert_eq!(
        metadata.presentation_feeds[1].component,
        SpatialPresentationComponent::WidthPositive
    );
    assert_eq!(
        metadata.presentation_feeds[2].component,
        SpatialPresentationComponent::WidthNegative
    );
    assert!(metadata.presentation_feeds[1].valid);
    assert!(metadata.presentation_feeds[2].valid);
    assert_eq!(metadata.presentation_feeds[1].source_index, 0);
    assert_eq!(metadata.presentation_feeds[2].source_index, 0);
    assert!(!metadata.final_hrtf_applied);
    assert!(metadata.world_space_unrotated);
    assert!(presentation.iter().copied().all(f32::is_finite));
    assert!(environment.iter().copied().all(f32::is_finite));
    assert!(environment.iter().all(|sample| sample.to_bits() == 0));
    assert!(
        plane(presentation, 0)
            .iter()
            .all(|sample| sample.to_bits() == 0)
    );

    let delayed = graph
        .delayed_program_for_source(0)
        .expect("StereoImage delayed program planes");
    assert!(delayed[0].iter().copied().all(f32::is_finite));
    assert!(delayed[1].iter().copied().all(f32::is_finite));
    let read_phase = stereo_delay(graph).read_phase();
    assert!(read_phase.is_finite());
    assert!((0.0..1.0).contains(&read_phase) || read_phase == 0.0);
}

fn tone(frame: usize, hertz: f32) -> f32 {
    (TAU * hertz * frame as f32 / SAMPLE_RATE_HZ as f32).sin()
}

fn motion_program_block(block: usize) -> ([f32; BLOCK_FRAMES], [f32; BLOCK_FRAMES]) {
    let left = std::array::from_fn(|frame| {
        let program_frame = block * BLOCK_FRAMES + frame;
        if program_frame < MARKER_PULSE_FRAMES {
            1.0
        } else if program_frame < ISOLATED_MARKER_FRAMES {
            0.0
        } else {
            0.025 * tone(program_frame - ISOLATED_MARKER_FRAMES, LEFT_TONE_HZ)
        }
    });
    let right = std::array::from_fn(|frame| {
        let program_frame = block * BLOCK_FRAMES + frame;
        if program_frame < MARKER_PULSE_FRAMES {
            -0.375
        } else if program_frame < ISOLATED_MARKER_FRAMES {
            0.0
        } else {
            -0.02 * tone(program_frame - ISOLATED_MARKER_FRAMES, RIGHT_TONE_HZ)
        }
    });
    (left, right)
}

fn first_nonzero(samples: &[f32]) -> usize {
    samples
        .iter()
        .position(|sample| sample.abs() > 1.0e-7)
        .expect("delayed marker must emerge")
}

fn positive_crossing_frequency_hz(samples: &[f32]) -> f32 {
    let mut first = None;
    let mut last = 0.0_f64;
    let mut count = 0_usize;
    for (index, pair) in samples.windows(2).enumerate() {
        if pair[0] <= 0.0 && pair[1] > 0.0 {
            let denominator = f64::from(pair[1] - pair[0]);
            if denominator <= 0.0 {
                continue;
            }
            let crossing = index as f64 + f64::from(-pair[0]) / denominator;
            first.get_or_insert(crossing);
            last = crossing;
            count += 1;
        }
    }
    assert!(count >= 8, "pitch window contained only {count} crossings");
    ((count - 1) as f64 * f64::from(SAMPLE_RATE_HZ) / (last - first.unwrap())) as f32
}

fn engine_requested_pitch_ratio(radial_velocity_mps: f32) -> f32 {
    (1.0 + radial_velocity_mps / SPEED_OF_SOUND_METERS_PER_SECOND)
        .recip()
        .clamp(2.0 / 3.0, 2.0)
}

fn radial_motion_label(radial_velocity_mps: f32) -> &'static str {
    if radial_velocity_mps > 0.0 {
        "recede"
    } else if radial_velocity_mps < 0.0 {
        "approach"
    } else {
        "static"
    }
}

fn tangential_prehistory_state(block: usize, speed_mps: f32) -> (EnuVector3, EnuVector3) {
    let speed_mps = speed_mps.abs();
    let seconds_before_motion =
        (STATIC_PREHISTORY_BLOCKS - block) as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE_HZ as f32;
    let angle = -speed_mps * seconds_before_motion / START_DISTANCE_M;
    (
        EnuVector3::new(
            START_DISTANCE_M * angle.cos(),
            START_DISTANCE_M * angle.sin(),
            LISTENER_UP_M,
        ),
        EnuVector3::new(-speed_mps * angle.sin(), speed_mps * angle.cos(), 0.0),
    )
}

fn run_motion_matrix_row(radial_velocity_mps: f32) {
    let initial_position = tangential_prehistory_state(0, radial_velocity_mps).0;
    let (mut simulation, mut graph) = stereo_pair(initial_position);
    let (mut presentation, mut environment, mut metadata) = output_banks();
    let silence = [0.0; BLOCK_FRAMES];
    let mut block_start_frame = 0_u64;

    // Keep fast rows velocity-guided while radial speed remains zero. Entering
    // -167 m/s from unguided silence extrapolates that approach into the empty
    // past and can skip an isolated marker; the constant-radius history makes
    // the real velocity corner causal before the rate-aware readout engages.
    for block in 0..STATIC_PREHISTORY_BLOCKS {
        let (position, velocity) = tangential_prehistory_state(block, radial_velocity_mps);
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            velocity,
            &silence,
            &silence,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        block_start_frame += BLOCK_FRAMES as u64;
    }

    let mut all_left = Vec::with_capacity(MOTION_BLOCKS * BLOCK_FRAMES);
    let mut all_right = Vec::with_capacity(MOTION_BLOCKS * BLOCK_FRAMES);
    let mut feed_left = Vec::with_capacity(MOTION_BLOCKS * BLOCK_FRAMES);
    let mut feed_right = Vec::with_capacity(MOTION_BLOCKS * BLOCK_FRAMES);
    let mut pitch_left =
        Vec::with_capacity((MOTION_BLOCKS - MOTION_CAPTURE_FROM_BLOCK) * BLOCK_FRAMES);
    let mut pitch_right =
        Vec::with_capacity((MOTION_BLOCKS - MOTION_CAPTURE_FROM_BLOCK) * BLOCK_FRAMES);
    let mut read_ratio_sum = 0.0_f64;
    let mut read_ratio_blocks = 0_u64;
    let mut previous_delay = stereo_delay(&graph).current_delay_samples();

    for block in 0..MOTION_BLOCKS {
        let elapsed_s = block as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE_HZ as f32;
        let position = EnuVector3::new(
            START_DISTANCE_M + radial_velocity_mps * elapsed_s,
            0.0,
            LISTENER_UP_M,
        );
        let velocity = EnuVector3::new(radial_velocity_mps, 0.0, 0.0);
        let (left, right) = motion_program_block(block);
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            velocity,
            &left,
            &right,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        block_start_frame += BLOCK_FRAMES as u64;

        let delayed = graph.delayed_program_for_source(0).unwrap();
        all_left.extend_from_slice(delayed[0]);
        all_right.extend_from_slice(delayed[1]);
        feed_left.extend_from_slice(plane(&presentation, 2));
        feed_right.extend_from_slice(plane(&presentation, 1));
        let current_delay = stereo_delay(&graph).current_delay_samples();
        if block >= MOTION_CAPTURE_FROM_BLOCK {
            pitch_left.extend_from_slice(delayed[0]);
            pitch_right.extend_from_slice(delayed[1]);
            read_ratio_sum +=
                f64::from(1.0 - (current_delay - previous_delay) / BLOCK_FRAMES as f32);
            read_ratio_blocks += 1;
        }
        previous_delay = current_delay;
    }

    let left_onset = first_nonzero(&all_left);
    let right_onset = first_nonzero(&all_right);
    assert_eq!(left_onset, right_onset, "L/R onset diverged");
    let expected_onset =
        START_DISTANCE_M * SAMPLE_RATE_HZ as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
    assert!(
        (left_onset as f32 - expected_onset).abs() <= 8.0,
        "{radial_velocity_mps} m/s onset {left_onset} missed {expected_onset:.3}"
    );
    let marker_peak = (left_onset.saturating_sub(3)..left_onset + 5)
        .max_by(|left, right| all_left[*left].abs().total_cmp(&all_left[*right].abs()))
        .unwrap();
    assert!(all_left[marker_peak] > 0.0);
    assert!(all_right[marker_peak] < 0.0);
    assert!(
        (all_right[marker_peak] + 0.375 * all_left[marker_peak]).abs() < 1.0e-6,
        "isolated authored L/R marker lost its shared read plan"
    );

    // Steam's DirectEffect may retain sub-threshold filter residue on one
    // authored channel, so onset/read-clock evidence stays on the deterministic
    // delayed program above. The real feed contract is valid, finite, mapped,
    // and observably non-silent on both authored sides.
    assert!(feed_left.iter().any(|sample| *sample != 0.0));
    assert!(feed_right.iter().any(|sample| *sample != 0.0));

    let left_ratio = positive_crossing_frequency_hz(&pitch_left) / LEFT_TONE_HZ;
    let right_ratio = positive_crossing_frequency_hz(&pitch_right) / RIGHT_TONE_HZ;
    let read_ratio = (read_ratio_sum / read_ratio_blocks as f64) as f32;
    let motion = radial_motion_label(radial_velocity_mps);
    let requested_ratio = engine_requested_pitch_ratio(radial_velocity_mps);
    let delivered_ratio = requested_ratio;
    println!(
        "LINKED_STEREO_MOTION motion={motion} signed_radial_mps={radial_velocity_mps:.1} positive_is_recession=true requested_ratio={requested_ratio:.6} delivered_ratio={delivered_ratio:.6} measured_left_ratio={left_ratio:.6} measured_right_ratio={right_ratio:.6} read_ratio={read_ratio:.6} shared_onset_frame={left_onset}"
    );
    assert!(
        (read_ratio - delivered_ratio).abs() < 0.002,
        "{radial_velocity_mps} m/s read ratio {read_ratio} != {delivered_ratio}"
    );
    assert!(
        (left_ratio - delivered_ratio).abs() < 0.006,
        "{radial_velocity_mps} m/s left ratio {left_ratio} != {delivered_ratio}"
    );
    assert!(
        (right_ratio - delivered_ratio).abs() < 0.006,
        "{radial_velocity_mps} m/s right ratio {right_ratio} != {delivered_ratio}"
    );
    assert!(
        (left_ratio - right_ratio).abs() < 0.004,
        "{radial_velocity_mps} m/s L/R pitch ratios diverged: {left_ratio} vs {right_ratio}"
    );

    if radial_velocity_mps == -167.0 {
        assert!((requested_ratio - 1.948_864).abs() < 1.0e-6);
        let pitch_error_cents = 1_200.0 * (read_ratio / requested_ratio).log2().abs();
        assert!(pitch_error_cents <= 10.0);
        println!(
            "LINKED_STEREO_WP3 approach_mps=167.0 requested_ratio={requested_ratio:.6} delivered_ratio={delivered_ratio:.6} pitch_error_cents={pitch_error_cents:.3} gamma_10_cent_result=true"
        );
    }

    let counters = graph.delay_instrumentation(0).unwrap();
    let expected_frames = ((STATIC_PREHISTORY_BLOCKS + MOTION_BLOCKS) * BLOCK_FRAMES) as u64;
    assert_eq!(counters.frames_processed, expected_frames);
    assert_eq!(counters.trajectory_advances, expected_frames);
    assert_eq!(counters.read_plan_advances, expected_frames);
    assert_eq!(counters.channel_count_changes, 0);
}

#[test]
fn linked_v2_stereo_image_motion_matrix_shares_onset_phase_and_pitch() {
    for speed_mps in [0.0_f32, 30.0, -30.0, 110.0, -110.0, 167.0, -167.0] {
        run_motion_matrix_row(speed_mps);
    }
}

fn proportional_program_block(start_frame: usize) -> ([f32; BLOCK_FRAMES], [f32; BLOCK_FRAMES]) {
    let left = std::array::from_fn(|frame| 0.025 * tone(start_frame + frame, 733.0));
    let right = std::array::from_fn(|frame| -0.375 * left[frame]);
    (left, right)
}

fn assert_proportional_delayed_program(graph: &NeutralMultiSourceRenderGraph) {
    let delayed = graph.delayed_program_for_source(0).unwrap();
    let maximum_error = delayed[0]
        .iter()
        .zip(delayed[1])
        .map(|(left, right)| (right + 0.375 * left).abs())
        .fold(0.0_f32, f32::max);
    assert!(
        maximum_error < 1.0e-6,
        "shared teleport plan produced L/R error {maximum_error:e}"
    );
}

#[test]
fn linked_v2_stereo_hysteresis_and_teleport_are_one_shared_state() {
    let mut position = EnuVector3::new(50.0, 0.0, LISTENER_UP_M);
    let (mut simulation, mut graph) = stereo_pair(position);
    let (mut presentation, mut environment, mut metadata) = output_banks();
    let mut block_start_frame = 0_u64;
    let mut program_frame = 0_usize;

    for _ in 0..150 {
        let (left, right) = proportional_program_block(program_frame);
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            EnuVector3::default(),
            &left,
            &right,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        assert_proportional_delayed_program(&graph);
        program_frame += BLOCK_FRAMES;
        block_start_frame += BLOCK_FRAMES as u64;
    }

    let hysteresis = [
        (8.0_f32, true),
        (7.99, true),
        (7.5, true),
        (7.01, true),
        (7.99, true),
        (7.0, false),
        (7.01, false),
        (7.5, false),
        (7.99, false),
        (8.0, true),
    ];
    for (speed_mps, expected_fast) in hysteresis {
        position.east_m += speed_mps * BLOCK_FRAMES as f32 / SAMPLE_RATE_HZ as f32;
        let (left, right) = proportional_program_block(program_frame);
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            EnuVector3::new(speed_mps, 0.0, 0.0),
            &left,
            &right,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        assert_eq!(
            stereo_delay(&graph).fast_motion_guided(),
            expected_fast,
            "8-enter/7-leave state at {speed_mps} m/s"
        );
        assert_proportional_delayed_program(&graph);
        program_frame += BLOCK_FRAMES;
        block_start_frame += BLOCK_FRAMES as u64;
    }

    assert!(!stereo_delay(&graph).is_crossfading());
    position.east_m += 80.0;
    let teleported_target =
        position.east_m * SAMPLE_RATE_HZ as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
    let (left, right) = proportional_program_block(program_frame);
    render_stereo_block(
        &mut simulation,
        &mut graph,
        position,
        EnuVector3::default(),
        &left,
        &right,
        block_start_frame,
        &mut presentation,
        &mut environment,
        &mut metadata,
    );
    program_frame += BLOCK_FRAMES;
    block_start_frame += BLOCK_FRAMES as u64;
    assert!(stereo_delay(&graph).is_crossfading());
    assert!(
        (stereo_delay(&graph).current_delay_samples() - teleported_target).abs() < 0.01,
        "teleport primary head did not adopt the new distance"
    );
    assert_proportional_delayed_program(&graph);

    let fade_frames = (TELEPORT_CROSSFADE_SECONDS * SAMPLE_RATE_HZ as f32).ceil() as usize;
    let fade_blocks = fade_frames.div_ceil(BLOCK_FRAMES);
    for completed_fade_blocks in 1..fade_blocks {
        let (left, right) = proportional_program_block(program_frame);
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            EnuVector3::default(),
            &left,
            &right,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        program_frame += BLOCK_FRAMES;
        block_start_frame += BLOCK_FRAMES as u64;
        assert_proportional_delayed_program(&graph);
        if completed_fade_blocks + 1 < fade_blocks {
            assert!(stereo_delay(&graph).is_crossfading());
        }
    }
    assert!(!stereo_delay(&graph).is_crossfading());
    println!(
        "LINKED_STEREO_SHARED_STATE hysteresis_enter_mps=8.0 hysteresis_leave_mps=7.0 teleport_fade_frames={fade_frames} teleport_fade_blocks={fade_blocks}"
    );
}

fn scheduled_motion(elapsed_s: f32) -> (EnuVector3, EnuVector3) {
    if elapsed_s < 1.0 {
        (
            EnuVector3::new(200.0 + 30.0 * elapsed_s, 0.0, 10.0),
            EnuVector3::new(30.0, 0.0, 0.0),
        )
    } else if elapsed_s < 2.0 {
        (
            EnuVector3::new(230.0, 30.0 * (elapsed_s - 1.0), 10.0),
            EnuVector3::new(0.0, 30.0, 0.0),
        )
    } else if elapsed_s < 3.0 {
        (
            EnuVector3::new(230.0, 30.0 - 30.0 * (elapsed_s - 2.0), 10.0),
            EnuVector3::new(0.0, -30.0, 0.0),
        )
    } else {
        (
            EnuVector3::new(230.0 - 30.0 * (elapsed_s - 3.0), 0.0, 10.0),
            EnuVector3::new(-30.0, 0.0, 0.0),
        )
    }
}

fn radial_velocity_mps(position: EnuVector3, velocity: EnuVector3) -> f32 {
    let relative_up_m = position.up_m - LISTENER_UP_M;
    let distance = (position.east_m * position.east_m
        + position.north_m * position.north_m
        + relative_up_m * relative_up_m)
        .sqrt();
    (position.east_m * velocity.east_m
        + position.north_m * velocity.north_m
        + relative_up_m * velocity.up_m)
        / distance
}

#[test]
fn linked_v2_stereo_turn_and_reversal_schedule_is_causal_and_channel_locked() {
    const PREHISTORY_BLOCKS: usize = 600;
    const SCHEDULE_BLOCKS: usize = 1_650;
    const PITCH_WINDOW_FRAMES: usize = 4_800;
    let initial_position = EnuVector3::new(200.0, 0.0, 10.0);
    let (mut simulation, mut graph) = stereo_pair(initial_position);
    let (mut presentation, mut environment, mut metadata) = output_banks();
    let mut delayed_left = Vec::with_capacity((PREHISTORY_BLOCKS + SCHEDULE_BLOCKS) * BLOCK_FRAMES);
    let mut delayed_right =
        Vec::with_capacity((PREHISTORY_BLOCKS + SCHEDULE_BLOCKS) * BLOCK_FRAMES);
    let mut block_start_frame = 0_u64;

    for block in 0..(PREHISTORY_BLOCKS + SCHEDULE_BLOCKS) {
        let (position, velocity) = if block < PREHISTORY_BLOCKS {
            (initial_position, EnuVector3::default())
        } else {
            let elapsed_s =
                (block - PREHISTORY_BLOCKS) as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE_HZ as f32;
            scheduled_motion(elapsed_s)
        };
        let left =
            std::array::from_fn(|frame| 0.025 * tone(block * BLOCK_FRAMES + frame, LEFT_TONE_HZ));
        let right =
            std::array::from_fn(|frame| -0.02 * tone(block * BLOCK_FRAMES + frame, RIGHT_TONE_HZ));
        render_stereo_block(
            &mut simulation,
            &mut graph,
            position,
            velocity,
            &left,
            &right,
            block_start_frame,
            &mut presentation,
            &mut environment,
            &mut metadata,
        );
        assert!(
            !stereo_delay(&graph).is_crossfading(),
            "continuous turn/reversal schedule was misread as a teleport"
        );
        let delayed = graph.delayed_program_for_source(0).unwrap();
        delayed_left.extend_from_slice(delayed[0]);
        delayed_right.extend_from_slice(delayed[1]);
        block_start_frame += BLOCK_FRAMES as u64;
    }

    let prehistory_s = PREHISTORY_BLOCKS as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE_HZ as f32;
    let probes = [
        ("static", -0.8_f32, initial_position, EnuVector3::default()),
        (
            "recede",
            0.5,
            scheduled_motion(0.5).0,
            scheduled_motion(0.5).1,
        ),
        (
            "turn_north",
            1.5,
            scheduled_motion(1.5).0,
            scheduled_motion(1.5).1,
        ),
        (
            "reverse_south",
            2.5,
            scheduled_motion(2.5).0,
            scheduled_motion(2.5).1,
        ),
        (
            "approach",
            3.5,
            scheduled_motion(3.5).0,
            scheduled_motion(3.5).1,
        ),
    ];
    let mut ratios = Vec::with_capacity(probes.len());
    for (name, motion_s, position, velocity) in probes {
        let emission_s = prehistory_s + motion_s;
        let relative_up_m = position.up_m - LISTENER_UP_M;
        let distance_m = (position.east_m * position.east_m
            + position.north_m * position.north_m
            + relative_up_m * relative_up_m)
            .sqrt();
        let arrival_s = emission_s + distance_m / SPEED_OF_SOUND_METERS_PER_SECOND;
        let center = (arrival_s * SAMPLE_RATE_HZ as f32).round() as usize;
        let start = center - PITCH_WINDOW_FRAMES / 2;
        let end = start + PITCH_WINDOW_FRAMES;
        let left_ratio = positive_crossing_frequency_hz(&delayed_left[start..end]) / LEFT_TONE_HZ;
        let right_ratio =
            positive_crossing_frequency_hz(&delayed_right[start..end]) / RIGHT_TONE_HZ;
        let radial = radial_velocity_mps(position, velocity);
        let expected = engine_requested_pitch_ratio(radial);
        println!(
            "LINKED_STEREO_SCHEDULE phase={name} emission_s={emission_s:.6} arrival_s={arrival_s:.6} radial_mps={radial:.6} expected_ratio={expected:.6} left_ratio={left_ratio:.6} right_ratio={right_ratio:.6}"
        );
        assert!(
            (left_ratio - expected).abs() < 0.012,
            "{name} left ratio {left_ratio} != {expected}"
        );
        assert!(
            (right_ratio - expected).abs() < 0.012,
            "{name} right ratio {right_ratio} != {expected}"
        );
        assert!(
            (left_ratio - right_ratio).abs() < 0.006,
            "{name} L/R ratios diverged"
        );
        ratios.push((left_ratio + right_ratio) * 0.5);
    }
    assert!(ratios[0] > ratios[1]);
    assert!(ratios[1] < ratios[2]);
    assert!(ratios[2] < ratios[3]);
    assert!(ratios[3] < ratios[4]);

    let counters = graph.delay_instrumentation(0).unwrap();
    let expected_frames = ((PREHISTORY_BLOCKS + SCHEDULE_BLOCKS) * BLOCK_FRAMES) as u64;
    assert_eq!(counters.frames_processed, expected_frames);
    assert_eq!(counters.trajectory_advances, expected_frames);
    assert_eq!(counters.read_plan_advances, expected_frames);
    assert_eq!(counters.channel_count_changes, 0);
}
