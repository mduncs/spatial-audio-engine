use crate::motion_smoothing::{
    SPEED_OF_SOUND_METERS_PER_SECOND, maximum_propagation_delay_samples,
};
use crate::propagation_delay::{
    PropagationDelayLine, StereoProgramDelayError, StereoProgramPropagationDelay,
    TELEPORT_CROSSFADE_SECONDS,
};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f32::consts::TAU;

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: usize = 128;

struct CountingAllocator;

thread_local! {
    static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
    static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every operation delegates directly to `System`; the thread-local
// counter observes calls without changing their allocation semantics.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
            }
        });
        // SAFETY: the caller-provided layout is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from the delegated allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
            }
        });
        // SAFETY: the caller-provided layout is forwarded unchanged.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        TRACK_ALLOCATIONS.with(|tracking| {
            if tracking.get() {
                ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
            }
        });
        // SAFETY: all arguments are forwarded under `GlobalAlloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

pub(crate) fn count_allocations(operation: impl FnOnce()) -> usize {
    ALLOCATION_COUNT.with(|count| count.set(0));
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
    operation();
    TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
    ALLOCATION_COUNT.with(Cell::get)
}

fn tone(frame: usize, hertz: f32) -> f32 {
    (TAU * hertz * frame as f32 / SAMPLE_RATE as f32).sin()
}

fn deliberately_different_inputs(frame: usize) -> [f32; 2] {
    let left_impulse = if frame % 997 == 31 { 0.35 } else { 0.0 };
    let right_impulse = if frame % 683 == 47 { -0.2 } else { 0.0 };
    [
        tone(frame, 317.0) * 0.61 + left_impulse,
        tone(frame, 941.0) * -0.37 + right_impulse,
    ]
}

fn observe_all_with_motion(
    stereo: &mut StereoProgramPropagationDelay,
    left: &mut PropagationDelayLine,
    right: &mut PropagationDelayLine,
    target_samples: f32,
    radial_velocity_mps: f32,
    relative_speed_mps: f32,
) {
    stereo.observe_block_target_with_motion(
        target_samples,
        radial_velocity_mps,
        relative_speed_mps,
    );
    left.observe_block_target_with_motion(target_samples, radial_velocity_mps, relative_speed_mps);
    right.observe_block_target_with_motion(target_samples, radial_velocity_mps, relative_speed_mps);
}

fn process_and_require_legacy_identity(
    stereo: &mut StereoProgramPropagationDelay,
    left: &mut PropagationDelayLine,
    right: &mut PropagationDelayLine,
    frame: usize,
) -> [f32; 2] {
    let input = deliberately_different_inputs(frame);
    let actual = stereo
        .process_frame(input, 2)
        .expect("two input planes are supported");
    let expected = [
        left.process_sample(input[0]),
        right.process_sample(input[1]),
    ];
    assert_eq!(
        actual[0].to_bits(),
        expected[0].to_bits(),
        "left diverged from the frozen mono controller at frame {frame}"
    );
    assert_eq!(
        actual[1].to_bits(),
        expected[1].to_bits(),
        "right diverged from the frozen mono controller at frame {frame}"
    );
    assert_eq!(
        left.current_delay_samples().to_bits(),
        right.current_delay_samples().to_bits(),
        "reference controllers acquired distinct read phases at frame {frame}"
    );
    assert_eq!(
        stereo.current_delay_samples().to_bits(),
        left.current_delay_samples().to_bits(),
        "shared trajectory diverged from the frozen controller at frame {frame}"
    );
    assert!(stereo.read_phase().is_finite());
    assert!((0.0..1.0).contains(&stereo.read_phase()) || stereo.read_phase() == 0.0);
    actual
}

#[test]
fn shared_read_plan_is_bit_exact_at_static_30_110_and_167_mps() {
    const BLOCKS: usize = 220;
    const PREHISTORY_FRAMES: usize = 16_000;
    const MEASURE_FROM_BLOCK: usize = 140;
    let cases = [
        (0.0_f32, 100.0_f32),
        (30.0, 100.0),
        (-30.0, 100.0),
        (110.0, 100.0),
        (-110.0, 100.0),
        (167.0, 100.0),
        (-167.0, 100.0),
    ];

    for (radial_velocity_mps, start_distance_m) in cases {
        let maximum = maximum_propagation_delay_samples(SAMPLE_RATE);
        let mut stereo = StereoProgramPropagationDelay::new(maximum, SAMPLE_RATE);
        let mut left = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut right = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut frame = 0_usize;
        let mut previous_delay = None;
        let mut accumulated_read_rate = 0.0_f64;
        let mut measured_frames = 0_usize;

        let initial_target =
            start_distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
        // Establish real static audio prehistory before motion begins. A zero
        // radial component with nonzero full speed is tangential motion, not a
        // static history: the constant-vector range must curve away from the
        // tangency instead of remaining pinned to `initial_target`.
        stereo.observe_block_target(initial_target);
        left.observe_block_target(initial_target);
        right.observe_block_target(initial_target);
        for _ in 0..PREHISTORY_FRAMES {
            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            previous_delay = Some(stereo.current_delay_samples());
            frame += 1;
        }

        for block in 0..BLOCKS {
            let seconds = block as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
            let distance_m = start_distance_m + radial_velocity_mps * seconds;
            let target = distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
            if radial_velocity_mps == 0.0 {
                stereo.observe_block_target(target);
                left.observe_block_target(target);
                right.observe_block_target(target);
            } else {
                observe_all_with_motion(
                    &mut stereo,
                    &mut left,
                    &mut right,
                    target,
                    radial_velocity_mps,
                    radial_velocity_mps.abs(),
                );
            }

            for _ in 0..BLOCK_FRAMES {
                let _ =
                    process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
                let delay = stereo.current_delay_samples();
                if block >= MEASURE_FROM_BLOCK
                    && let Some(previous) = previous_delay
                {
                    accumulated_read_rate += f64::from(1.0 - (delay - previous));
                    measured_frames += 1;
                }
                previous_delay = Some(delay);
                frame += 1;
            }
        }

        let requested_ratio = if radial_velocity_mps == 0.0 {
            1.0
        } else {
            (1.0 + radial_velocity_mps / SPEED_OF_SOUND_METERS_PER_SECOND)
                .recip()
                .clamp(2.0 / 3.0, 2.0)
        };
        let expected_actual_ratio = requested_ratio;
        let measured_ratio = (accumulated_read_rate / measured_frames as f64) as f32;
        println!(
            "STEREO_SHARED_PLAN speed_mps={radial_velocity_mps:.1} requested_ratio={requested_ratio:.6} actual_ratio={measured_ratio:.6}"
        );
        assert!(
            (measured_ratio - expected_actual_ratio).abs() < 0.002,
            "{radial_velocity_mps} m/s produced {measured_ratio}, expected {expected_actual_ratio}"
        );

        let counters = stereo.instrumentation();
        assert_eq!(counters.frames_processed, frame as u64);
        assert_eq!(counters.trajectory_advances, frame as u64);
        assert_eq!(counters.read_plan_advances, frame as u64);
        assert_eq!(counters.channel_count_changes, 0);
    }
}

#[test]
fn shared_stereo_static_prehistory_delays_motion_until_the_causal_corner() {
    const STATIC_DELAY_SAMPLES: f32 = 4_800.0;
    const SPEED_MPS: f32 = 110.0;
    const WARMUP_FRAMES: usize = 12_000;
    let mut moving = StereoProgramPropagationDelay::new(300_000, SAMPLE_RATE);
    let mut static_reference = StereoProgramPropagationDelay::new(300_000, SAMPLE_RATE);
    moving.observe_block_target(STATIC_DELAY_SAMPLES);
    static_reference.observe_block_target(STATIC_DELAY_SAMPLES);

    let mut frame = 0_usize;
    for _ in 0..WARMUP_FRAMES {
        let input = deliberately_different_inputs(frame);
        let actual = moving.process_frame(input, 2).unwrap();
        let expected = static_reference.process_frame(input, 2).unwrap();
        assert_eq!(actual[0].to_bits(), expected[0].to_bits());
        assert_eq!(actual[1].to_bits(), expected[1].to_bits());
        frame += 1;
    }

    moving.observe_block_target_with_motion(STATIC_DELAY_SAMPLES, SPEED_MPS, SPEED_MPS);
    let mut maximum_audible_output = [0.0_f32; 2];
    for causal_offset in 0..=STATIC_DELAY_SAMPLES as usize {
        let input = deliberately_different_inputs(frame);
        let actual = moving.process_frame(input, 2).unwrap();
        let expected = static_reference.process_frame(input, 2).unwrap();
        for channel in 0..2 {
            maximum_audible_output[channel] =
                maximum_audible_output[channel].max(actual[channel].abs());
            assert_eq!(
                actual[channel].to_bits(),
                expected[channel].to_bits(),
                "channel {channel} heard motion early at causal offset {causal_offset}"
            );
        }
        assert_eq!(
            moving.target_delay_samples().to_bits(),
            STATIC_DELAY_SAMPLES.to_bits(),
            "shared retarded target moved early at causal offset {causal_offset}"
        );
        assert_eq!(
            moving.current_delay_samples().to_bits(),
            STATIC_DELAY_SAMPLES.to_bits(),
            "shared read plan moved early at causal offset {causal_offset}"
        );
        frame += 1;
    }
    assert!(maximum_audible_output.into_iter().all(|peak| peak > 0.25));

    let input = deliberately_different_inputs(frame);
    let actual = moving.process_frame(input, 2).unwrap();
    let expected = static_reference.process_frame(input, 2).unwrap();
    assert!(moving.target_delay_samples() > STATIC_DELAY_SAMPLES);
    assert!(moving.current_delay_samples() > STATIC_DELAY_SAMPLES);
    assert_ne!(actual[0].to_bits(), expected[0].to_bits());
    assert_ne!(actual[1].to_bits(), expected[1].to_bits());
}

#[test]
fn phase_shifted_sparse_off_axis_passes_keep_stereo_on_mono_reference() {
    const SPEED_MPS: f32 = 110.0;
    const CLOSEST_RANGE_M: f32 = 30.0;
    const CLOSEST_TIME_SECONDS: f32 = 2.0;
    const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
    const TOTAL_FRAMES: usize = SAMPLE_RATE as usize * 4;
    const MEASURE_FROM_FRAME: usize = SAMPLE_RATE as usize;

    for publication_phase_frames in [0_usize, 137, 399, 799] {
        let maximum = maximum_propagation_delay_samples(SAMPLE_RATE);
        let mut stereo = StereoProgramPropagationDelay::new(maximum, SAMPLE_RATE);
        let mut left = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut right = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut maximum_publication_correction = 0.0_f32;
        let mut cap_hit_frames = 0_usize;
        let mut cap_hit_bursts = 0_usize;
        let mut cap_hit_burst_frames = 0_usize;
        let mut maximum_cap_hit_burst_frames = 0_usize;
        let mut maximum_sample_pitch_jump = 0.0_f32;
        let mut previous_delay: Option<f32> = None;
        let mut previous_sample_pitch: Option<f32> = None;

        for frame in 0..TOTAL_FRAMES {
            if frame >= publication_phase_frames
                && (frame - publication_phase_frames) % PUBLICATION_FRAMES == 0
            {
                let seconds = frame as f32 / SAMPLE_RATE as f32;
                let along_track_m = SPEED_MPS * (seconds - CLOSEST_TIME_SECONDS);
                let distance_m =
                    (along_track_m * along_track_m + CLOSEST_RANGE_M * CLOSEST_RANGE_M).sqrt();
                let radial_velocity_mps = SPEED_MPS * along_track_m / distance_m;
                let target = distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
                if frame > publication_phase_frames {
                    assert_eq!(
                        stereo.current_geometry_delay_samples().to_bits(),
                        left.current_geometry_delay_samples().to_bits(),
                        "stereo geometry diverged before publication at frame {frame}"
                    );
                    maximum_publication_correction = maximum_publication_correction
                        .max((target - left.current_geometry_delay_samples()).abs());
                }
                observe_all_with_motion(
                    &mut stereo,
                    &mut left,
                    &mut right,
                    target,
                    radial_velocity_mps,
                    SPEED_MPS,
                );
            }

            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            let current_delay = stereo.current_delay_samples();
            if frame >= MEASURE_FROM_FRAME
                && let Some(previous) = previous_delay
            {
                let delay_step = current_delay - previous;
                let sample_pitch = 1.0 - delay_step;
                if let Some(previous_pitch) = previous_sample_pitch {
                    maximum_sample_pitch_jump =
                        maximum_sample_pitch_jump.max((sample_pitch - previous_pitch).abs());
                }
                previous_sample_pitch = Some(sample_pitch);
                if delay_step.abs() >= 0.49 {
                    cap_hit_frames += 1;
                    if cap_hit_burst_frames == 0 {
                        cap_hit_bursts += 1;
                    }
                    cap_hit_burst_frames += 1;
                    maximum_cap_hit_burst_frames =
                        maximum_cap_hit_burst_frames.max(cap_hit_burst_frames);
                } else {
                    cap_hit_burst_frames = 0;
                }
            }
            previous_delay = Some(current_delay);
        }

        println!(
            "STEREO_OFF_AXIS speed_mps=110.0 closest_range_m=30.0 publication_hz=60 publication_phase_frames={publication_phase_frames} maximum_publication_correction_samples={maximum_publication_correction:.6} cap_hit_frames={cap_hit_frames} cap_hit_bursts={cap_hit_bursts} maximum_cap_hit_burst_frames={maximum_cap_hit_burst_frames} maximum_sample_pitch_jump={maximum_sample_pitch_jump:.6}"
        );
        assert!(maximum_publication_correction < 0.05);
        assert_eq!(cap_hit_frames, 0);
        assert_eq!(cap_hit_bursts, 0);
        assert!(maximum_sample_pitch_jump < 0.02);
    }
}

#[test]
fn maximum_horizon_outward_and_inward_motion_keeps_stereo_on_mono_reference() {
    const MAXIMUM_DELAY_SAMPLES: usize = 2_000;
    const SPEED_MPS: f32 = 110.0;
    const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
    let geometry_rate = f64::from(SPEED_MPS) / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND);
    let mut analytic_delay = 1_900.0_f64;
    let mut stereo = StereoProgramPropagationDelay::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut left = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut right = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut frame = 0_usize;

    for _ in 0..4 {
        observe_all_with_motion(
            &mut stereo,
            &mut left,
            &mut right,
            analytic_delay as f32,
            SPEED_MPS,
            SPEED_MPS,
        );
        for _ in 0..PUBLICATION_FRAMES {
            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            analytic_delay += geometry_rate;
            frame += 1;
        }
    }
    assert!(left.current_geometry_delay_samples() > MAXIMUM_DELAY_SAMPLES as f32);
    assert_eq!(
        stereo.current_delay_samples().to_bits(),
        (MAXIMUM_DELAY_SAMPLES as f32).to_bits()
    );

    let mut previous_delay = stereo.current_delay_samples();
    let mut cap_hit_frames = 0_usize;
    for _ in 0..7 {
        observe_all_with_motion(
            &mut stereo,
            &mut left,
            &mut right,
            analytic_delay as f32,
            -SPEED_MPS,
            SPEED_MPS,
        );
        for _ in 0..PUBLICATION_FRAMES {
            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            analytic_delay -= geometry_rate;
            let current = stereo.current_delay_samples();
            if (current - previous_delay).abs() >= 0.49 {
                cap_hit_frames += 1;
            }
            previous_delay = current;
            frame += 1;
        }
    }

    assert!(left.current_geometry_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
    assert!(stereo.current_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
    assert_eq!(cap_hit_frames, 0);
}

#[test]
fn far_to_far_horizon_teleport_and_inward_reentry_keep_stereo_on_mono_reference() {
    const MAXIMUM_DELAY_SAMPLES: usize = 10_000;
    const SPEED_MPS: f32 = 110.0;
    const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
    let geometry_rate = f64::from(SPEED_MPS) / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND);
    let mut stereo = StereoProgramPropagationDelay::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut left = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut right = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
    let mut frame = 0_usize;

    observe_all_with_motion(
        &mut stereo,
        &mut left,
        &mut right,
        15_000.0,
        SPEED_MPS,
        SPEED_MPS,
    );
    for _ in 0..128 {
        let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
        frame += 1;
    }
    assert_eq!(
        stereo.previous_raw_target_samples().to_bits(),
        15_000.0_f64.to_bits()
    );

    let mut analytic_geometry = 18_000.0_f64;
    observe_all_with_motion(
        &mut stereo,
        &mut left,
        &mut right,
        analytic_geometry as f32,
        -SPEED_MPS,
        SPEED_MPS,
    );
    assert!(stereo.is_crossfading());
    assert!(left.is_crossfading());
    assert!(right.is_crossfading());
    assert_eq!(
        stereo.previous_raw_target_samples().to_bits(),
        analytic_geometry.to_bits()
    );
    assert_eq!(
        stereo.current_geometry_delay_samples().to_bits(),
        (analytic_geometry as f32).to_bits()
    );

    let mut previous_delay = stereo.current_delay_samples();
    let mut reentered = false;
    let mut cap_hit_frames = 0_usize;
    for capture_frame in 0..45_000 {
        if capture_frame > 0 && capture_frame % PUBLICATION_FRAMES == 0 {
            observe_all_with_motion(
                &mut stereo,
                &mut left,
                &mut right,
                analytic_geometry as f32,
                -SPEED_MPS,
                SPEED_MPS,
            );
        }
        let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
        analytic_geometry -= geometry_rate;
        let current = stereo.current_delay_samples();
        if current < MAXIMUM_DELAY_SAMPLES as f32 {
            reentered = true;
        }
        if reentered && (current - previous_delay).abs() >= 0.49 {
            cap_hit_frames += 1;
        }
        previous_delay = current;
        frame += 1;
    }

    assert!(reentered);
    assert!(stereo.current_geometry_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
    assert!(stereo.current_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
    assert_eq!(cap_hit_frames, 0);
}

#[test]
fn different_channel_impulses_keep_order_and_shared_onset_offset() {
    let mut stereo = StereoProgramPropagationDelay::new(8_192, SAMPLE_RATE);
    stereo.observe_block_target(1_234.5);
    let mut left_onset = None;
    let mut right_onset = None;

    for frame in 0..2_000 {
        let output = stereo
            .process_frame(
                [
                    if frame == 0 { 1.0 } else { 0.0 },
                    if frame == 73 { -0.5 } else { 0.0 },
                ],
                2,
            )
            .unwrap();
        if left_onset.is_none() && output[0].abs() > 1.0e-7 {
            left_onset = Some(frame);
        }
        if right_onset.is_none() && output[1].abs() > 1.0e-7 {
            right_onset = Some(frame);
        }
    }

    let left_onset = left_onset.expect("left impulse must emerge");
    let right_onset = right_onset.expect("right impulse must emerge");
    assert_eq!(right_onset - left_onset, 73);
    assert!(
        (1_232..=1_235).contains(&left_onset),
        "fractional-delay onset landed at {left_onset}"
    );
}

#[test]
fn eight_enter_seven_leave_hysteresis_is_shared_by_both_planes() {
    let mut stereo = StereoProgramPropagationDelay::new(300_000, SAMPLE_RATE);
    let target = 4_800.0;

    stereo.observe_block_target_with_motion(target, 1.0, 8.0);
    assert!(stereo.fast_motion_guided());
    for speed in [7.99_f32, 7.5, 7.01, 7.99] {
        stereo.observe_block_target_with_motion(target, 1.0, speed);
        assert!(stereo.fast_motion_guided(), "fast path left at {speed}");
        assert!(stereo.process_frame([0.1, -0.2], 2).is_ok());
    }

    stereo.observe_block_target_with_motion(target, 1.0, 7.0);
    assert!(!stereo.fast_motion_guided());
    for speed in [7.01_f32, 7.5, 7.99] {
        stereo.observe_block_target_with_motion(target, 1.0, speed);
        assert!(!stereo.fast_motion_guided(), "slow path left at {speed}");
        assert!(stereo.process_frame([0.1, -0.2], 2).is_ok());
    }
    stereo.observe_block_target_with_motion(target, 1.0, 8.0);
    assert!(stereo.fast_motion_guided());

    let counters = stereo.instrumentation();
    assert_eq!(counters.frames_processed, 7);
    assert_eq!(counters.trajectory_advances, 7);
    assert_eq!(counters.read_plan_advances, 7);
}

#[test]
fn teleports_remain_channel_locked_at_static_and_fast_speeds() {
    let speeds = [0.0_f32, 30.0, -110.0, -167.0];
    let fade_frames = (TELEPORT_CROSSFADE_SECONDS * SAMPLE_RATE as f32).ceil() as usize;

    for speed_mps in speeds {
        let maximum = maximum_propagation_delay_samples(SAMPLE_RATE);
        let mut stereo = StereoProgramPropagationDelay::new(maximum, SAMPLE_RATE);
        let mut left = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut right = PropagationDelayLine::new(maximum, SAMPLE_RATE);
        let mut target = 2_400.25_f32;
        let mut frame = 0_usize;

        if speed_mps == 0.0 {
            stereo.observe_block_target(target);
            left.observe_block_target(target);
            right.observe_block_target(target);
        } else {
            observe_all_with_motion(
                &mut stereo,
                &mut left,
                &mut right,
                target,
                speed_mps,
                speed_mps.abs(),
            );
        }
        for _ in 0..8_000 {
            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            frame += 1;
        }

        target += 12_000.5;
        if speed_mps == 0.0 {
            stereo.observe_block_target(target);
            left.observe_block_target(target);
            right.observe_block_target(target);
        } else {
            observe_all_with_motion(
                &mut stereo,
                &mut left,
                &mut right,
                target,
                speed_mps,
                speed_mps.abs(),
            );
        }
        assert!(stereo.is_crossfading());
        assert!(left.is_crossfading());
        assert!(right.is_crossfading());

        for fade_frame in 0..fade_frames {
            if fade_frame > 0 && fade_frame % BLOCK_FRAMES == 0 {
                target += speed_mps / SPEED_OF_SOUND_METERS_PER_SECOND * BLOCK_FRAMES as f32;
                if speed_mps == 0.0 {
                    stereo.observe_block_target(target);
                    left.observe_block_target(target);
                    right.observe_block_target(target);
                } else {
                    observe_all_with_motion(
                        &mut stereo,
                        &mut left,
                        &mut right,
                        target,
                        speed_mps,
                        speed_mps.abs(),
                    );
                }
            }
            let _ = process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
            frame += 1;
        }
        assert!(!stereo.is_crossfading());
        assert!(!left.is_crossfading());
        assert!(!right.is_crossfading());
    }
}

#[test]
fn velocity_turn_and_reversal_never_split_the_channel_clock() {
    let maximum = maximum_propagation_delay_samples(SAMPLE_RATE);
    let mut stereo = StereoProgramPropagationDelay::new(maximum, SAMPLE_RATE);
    let mut left = PropagationDelayLine::new(maximum, SAMPLE_RATE);
    let mut right = PropagationDelayLine::new(maximum, SAMPLE_RATE);
    let schedule = [
        (30.0_f32, 30.0_f32, 90_usize),
        (0.0, 30.0, 40),
        (-30.0, 30.0, 90),
        (110.0, 110.0, 70),
        (-167.0, 167.0, 70),
    ];
    let mut distance_m = 900.0_f32;
    let mut frame = 0_usize;

    for (radial_mps, relative_mps, blocks) in schedule {
        for _ in 0..blocks {
            let target = distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
            observe_all_with_motion(
                &mut stereo,
                &mut left,
                &mut right,
                target,
                radial_mps,
                relative_mps,
            );
            for _ in 0..BLOCK_FRAMES {
                let output =
                    process_and_require_legacy_identity(&mut stereo, &mut left, &mut right, frame);
                assert!(output.into_iter().all(f32::is_finite));
                frame += 1;
            }
            distance_m += radial_mps * BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
            assert!(
                !stereo.is_crossfading(),
                "continuous turn became a teleport"
            );
        }
    }

    let counters = stereo.instrumentation();
    assert_eq!(counters.trajectory_advances, frame as u64);
    assert_eq!(counters.read_plan_advances, frame as u64);
}

#[test]
fn channel_count_changes_and_resets_cannot_replay_stale_right_audio() {
    let mut stereo = StereoProgramPropagationDelay::new(256, SAMPLE_RATE);
    stereo.reset_to(16.5);

    for _ in 0..96 {
        let _ = stereo.process_frame([0.25, 1.0], 2).unwrap();
    }
    for _ in 0..2 {
        assert_eq!(stereo.process_frame([0.25, 9.0], 1).unwrap()[1], 0.0);
    }
    let mut final_right = 0.0;
    for frame in 0..32 {
        let right = stereo.process_frame([0.25, 0.25], 2).unwrap()[1];
        if frame < 17 {
            assert_eq!(
                right, 0.0,
                "right history opened before every shared-plan tap was valid at frame {frame}"
            );
        }
        // The cubic interpolator may overshoot the new 0.25 step to 0.3125,
        // but the preceding 1.0 program must be unreachable.
        assert!(
            (-0.1..=0.4).contains(&right),
            "stale authored-right history escaped after reactivation at frame {frame}: {right}"
        );
        final_right = right;
    }
    assert!((final_right - 0.25).abs() < 1.0e-6);

    let before_invalid = stereo.instrumentation();
    assert_eq!(
        stereo.process_frame([1.0, 1.0], 0),
        Err(StereoProgramDelayError::UnsupportedChannelCount(0))
    );
    assert_eq!(
        stereo.process_frame([1.0, 1.0], 3),
        Err(StereoProgramDelayError::UnsupportedChannelCount(3))
    );
    assert_eq!(stereo.instrumentation(), before_invalid);

    for _ in 0..96 {
        let _ = stereo.process_frame([1.0, -1.0], 2).unwrap();
    }
    stereo.invalidate();
    stereo.observe_block_target(32.25);
    for frame in 0..35 {
        let output = stereo.process_frame([0.0, 0.0], 2).unwrap();
        assert_eq!(
            output,
            [0.0, 0.0],
            "stale source history escaped after invalidation at frame {frame}"
        );
    }

    for _ in 0..96 {
        let _ = stereo.process_frame([1.0, -1.0], 2).unwrap();
    }
    stereo.reset_to(48.5);
    for frame in 0..52 {
        let output = stereo.process_frame([0.0, 0.0], 2).unwrap();
        assert_eq!(
            output,
            [0.0, 0.0],
            "stale source history escaped after reset at frame {frame}"
        );
    }

    let mut pristine = StereoProgramPropagationDelay::new(32, SAMPLE_RATE);
    pristine.observe_block_target(0.0);
    assert_eq!(pristine.process_frame([0.0, 7.0], 1).unwrap(), [0.0, 0.0]);
    assert_eq!(pristine.process_frame([0.0, 0.75], 2).unwrap()[1], 0.75);
}

#[test]
fn callback_operations_allocate_zero_times_even_across_adversarial_state_changes() {
    let mut stereo = StereoProgramPropagationDelay::new(300_000, SAMPLE_RATE);
    stereo.observe_block_target(4_800.25);
    let _ = stereo.process_frame([0.0, 0.0], 2).unwrap();

    let allocations = count_allocations(|| {
        for frame in 0..4_096 {
            if frame % BLOCK_FRAMES == 0 {
                let speed = if frame % (BLOCK_FRAMES * 4) == 0 {
                    110.0
                } else {
                    -167.0
                };
                stereo.observe_block_target_with_motion(
                    4_800.25 + frame as f32 * 0.1,
                    speed,
                    speed.abs(),
                );
            }
            let planes = if frame % 257 == 0 { 1 } else { 2 };
            let _ = stereo
                .process_frame(deliberately_different_inputs(frame), planes)
                .unwrap();
        }
        stereo.reset_to(1_024.5);
        let _ = stereo.process_frame([0.1, -0.2], 2).unwrap();
        stereo.invalidate();
        stereo.observe_block_target(2_048.25);
        let _ = stereo.process_frame([0.1, -0.2], 1).unwrap();
        let _ = stereo.process_frame([0.0, 0.0], 0);
        let _ = stereo.process_frame([0.0, 0.0], 3);
    });

    assert_eq!(
        allocations, 0,
        "callback path allocated {allocations} times"
    );
}

#[test]
fn exact_stereo_history_delta_is_reported_for_the_2048_meter_horizon() {
    let maximum = maximum_propagation_delay_samples(SAMPLE_RATE);
    let stereo = StereoProgramPropagationDelay::new(maximum, SAMPLE_RATE);
    let memory = stereo.memory();
    let expected_history_bytes =
        super::propagation_delay::delay_history_len(maximum) * core::mem::size_of::<f32>();
    let legacy_inline_bytes = core::mem::size_of::<PropagationDelayLine>();
    let stereo_inline_bytes = StereoProgramPropagationDelay::inline_state_bytes();
    let inline_delta_bytes = stereo_inline_bytes - legacy_inline_bytes;
    let persistent_delta_bytes = expected_history_bytes + inline_delta_bytes;
    let projected_sixteen_bytes = persistent_delta_bytes * 16;

    assert_eq!(
        memory.additional_channel_payload_bytes,
        expected_history_bytes
    );
    assert_eq!(
        memory.audio_history_payload_bytes,
        expected_history_bytes * 2
    );
    assert_eq!(
        memory.geometry_history_payload_bytes,
        expected_history_bytes
    );
    assert_eq!(memory.total_heap_payload_bytes, expected_history_bytes * 3);
    println!(
        "STEREO_PROPAGATION_MEMORY maximum_delay_samples={maximum} additional_channel_payload_bytes={expected_history_bytes} additional_channel_payload_mib={:.9} inline_delta_bytes={inline_delta_bytes} persistent_delta_bytes={persistent_delta_bytes} persistent_delta_mib={:.9} projected_all_16_bytes={projected_sixteen_bytes} projected_all_16_mib={:.9}",
        expected_history_bytes as f64 / 1_048_576.0,
        persistent_delta_bytes as f64 / 1_048_576.0,
        projected_sixteen_bytes as f64 / 1_048_576.0,
    );
}
