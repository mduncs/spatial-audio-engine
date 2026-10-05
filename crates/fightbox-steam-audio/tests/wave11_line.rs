//! Wave 11 percept gates through the retained, linked Steam Audio renderer.
//!
//! Run with:
//! `STEAM_AUDIO_SDK_DIR=/absolute/path/to/steamaudio cargo test -p fightbox-steam-audio --features linked-sdk --test wave11_line -- --nocapture`

#![cfg(feature = "linked-sdk")]

use fightbox_api::{EnuVector3 as ApiVector, ExtentDescriptor, ListenerState, Pose};
use fightbox_evidence::ears::corpus::MOVING_NOTCH_WINDOW_FRAMES;
use fightbox_evidence::ears::{
    Pcm, PitchTrack, PitchTrackConfig, WidthProfile, WidthProfileConfig, WidthTrack,
    stereo_reference_moving_spectral_notches, windowed_pitch_track, windowed_width_profile,
};
use fightbox_evidence::{DEFAULT_MOVING_NOTCH_THRESHOLD_DB, WavSpec};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationRunner, SimulationUpdate, SourceMotion,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, EnuVector3 as MeshVector,
    MultiSourceDescriptor, PathBakeConfig, ProbeVolume, S3BakeRequest, S3SimulationConfig,
    SceneMesh, StageOutputGains, SteamAudioRenderGraph, bake_s3, build_multi_source_session,
};
use std::f64::consts::TAU;
use std::sync::OnceLock;
use std::time::Instant;

const SAMPLE_RATE_HZ: u32 = 48_000;
const BLOCK_FRAMES: usize = 512;
const PRE_ROLL_BLOCKS: usize = 96;
const PRE_ROLL_FRAMES: usize = PRE_ROLL_BLOCKS * BLOCK_FRAMES;
const LISTENER_POSITION: ApiVector = ApiVector::new(0.0, 0.0, 1.5);
const EAST: ApiVector = ApiVector::new(1.0, 0.0, 0.0);
const UP: ApiVector = ApiVector::new(0.0, 0.0, 1.0);

// W2a's synthetic wide/collapsed pair measured 0.243988/0.000000 at this 0.20 gate.
const HONEST_WIDE_MIN: f64 = 0.20;
// W2a's synthetic collapse spans 0.737073 to 0.011047; Wave 11 reserves 0.10 as material.
const WIDTH_DROP_MIN: f64 = 0.10;
// W2a's injected re-expansion measured 0.334439 and is rejected above 0.02.
const MAX_OUTWARD_WIDTH_INCREASE: f64 = 0.02;
// W2a's far synthetic truth is 0.011047; 0.03 is the decision's point-approach allowance.
const FAR_POINT_WIDTH_TOLERANCE: f64 = 0.03;
// W2a's honest pitch delta was effectively 0 Hz; its smear failed all 31 windows at 0.75 Hz.
const PITCH_IDENTITY_TOLERANCE_HZ: f64 = 0.75;
// W2a's synthetic collapse is perfectly rank-decreasing; the decision admits rho down to -0.95.
const MIN_COLLAPSE_SPEARMAN: f64 = -0.95;
// W2a measured honest/corrupt moving-notch residuals of 0.000/17.163 dB at this 15 dB ceiling.
const MOVING_NOTCH_CEILING_DB: f32 = DEFAULT_MOVING_NOTCH_THRESHOLD_DB;
const MOVING_NOTCH_HOP_FRAMES: usize = MOVING_NOTCH_WINDOW_FRAMES;
const WIDTH_METERS: f32 = 6.0;
const WIDTH_HALF_METERS: f64 = WIDTH_METERS as f64 / 2.0;

const COLLAPSE_DISTANCE_CENTERS_M: [f64; 7] = [2.5, 4.0, 6.3, 10.0, 16.0, 25.0, 40.0];

#[derive(Debug)]
struct StereoCapture {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl StereoCapture {
    fn pcm(&self) -> Pcm<'_> {
        Pcm {
            left: &self.left,
            right: &self.right,
            sample_rate_hz: SAMPLE_RATE_HZ,
        }
    }

    fn interleaved(&self) -> Vec<f32> {
        self.left
            .iter()
            .zip(&self.right)
            .flat_map(|(&left, &right)| [left, right])
            .collect()
    }
}

#[derive(Clone, Copy, Debug)]
struct CollapseMetrics {
    spearman: f64,
    max_outward_increase: f64,
    near_to_far_drop: f64,
}

#[derive(Clone, Copy, Debug)]
struct PitchComparison {
    maximum_difference_hz: f64,
    violating_windows: usize,
    missing_windows: usize,
    window_count: usize,
}

#[test]
fn width_at_near_is_wide_while_matched_point_fails() {
    let started = Instant::now();
    let capture_frames = 3 * SAMPLE_RATE_HZ as usize;
    let program = broadband_program(render_frame_count(capture_frames), 0.25);
    let center = ApiVector::new(0.0, 5.0, 1.5);
    let point_descriptor = descriptor(center, ExtentDescriptor::Point, EAST);
    let line_descriptor = descriptor(
        center,
        ExtentDescriptor::LineSegment {
            length_m: WIDTH_METERS,
        },
        EAST,
    );
    let motion =
        move |_time_s: f64, _source_index: usize| source_motion(center, EAST, ApiVector::default());
    let point = render_capture(&[point_descriptor], capture_frames, &program, motion);
    let line = render_capture(&[line_descriptor], capture_frames, &program, motion);
    let distances = vec![5.0_f32; capture_frames];
    let subtenses = vec![broadside_subtense(5.0) as f32; capture_frames];
    let config = WidthProfileConfig::wave11(SAMPLE_RATE_HZ);
    let point_profile = windowed_width_profile(
        point.pcm(),
        WidthTrack {
            distances_m: &distances,
            angular_subtenses_rad: None,
        },
        config,
    )
    .expect("profile matched point render");
    let line_profile = windowed_width_profile(
        line.pcm(),
        WidthTrack {
            distances_m: &distances,
            angular_subtenses_rad: Some(&subtenses),
        },
        config,
    )
    .expect("profile real line render");
    let point_width = admitted_median_width(&point_profile);
    let line_width = admitted_median_width(&line_profile);
    let delta = line_width - point_width;

    assert!(
        line_width >= HONEST_WIDE_MIN,
        "real 6 m line at 5 m was not honestly wide: {line_width:.6}"
    );
    assert!(
        point_width < HONEST_WIDE_MIN,
        "matched point unexpectedly passed the near-width gate: {point_width:.6}"
    );
    assert!(
        delta >= WIDTH_DROP_MIN,
        "line/point width separation was not material: {delta:.6}"
    );
    println!(
        "WAVE11 gate=width-near line={line_width:.6} point_control={point_width:.6} delta={delta:.6} threshold={HONEST_WIDE_MIN:.3} delta_threshold={WIDTH_DROP_MIN:.3} windows={} elapsed_s={:.3}",
        line_profile.windows.len(),
        started.elapsed().as_secs_f64(),
    );
}

#[test]
fn broadside_recede_collapses_monotonically_and_point_control_does_not() {
    let started = Instant::now();
    const START_DISTANCE_M: f64 = 2.5;
    const END_DISTANCE_M: f64 = 40.0;
    const SPEED_MPS: f64 = 1.5;
    let duration_s = (END_DISTANCE_M - START_DISTANCE_M) / SPEED_MPS;
    let capture_frames = (duration_s * SAMPLE_RATE_HZ as f64).round() as usize;
    let program = broadband_program(render_frame_count(capture_frames), 0.25);
    let initial = ApiVector::new(0.0, START_DISTANCE_M as f32, 1.5);
    let point_descriptor = descriptor(initial, ExtentDescriptor::Point, EAST);
    let line_descriptor = descriptor(
        initial,
        ExtentDescriptor::LineSegment {
            length_m: WIDTH_METERS,
        },
        EAST,
    );
    let recede_motion = move |time_s: f64, _source_index: usize| {
        let active_time_s = time_s.clamp(0.0, duration_s);
        let distance_m = START_DISTANCE_M + SPEED_MPS * active_time_s;
        let velocity = if (0.0..duration_s).contains(&time_s) {
            ApiVector::new(0.0, SPEED_MPS as f32, 0.0)
        } else {
            ApiVector::default()
        };
        source_motion(ApiVector::new(0.0, distance_m as f32, 1.5), EAST, velocity)
    };
    let point = render_capture(&[point_descriptor], capture_frames, &program, recede_motion);
    let line = render_capture(&[line_descriptor], capture_frames, &program, recede_motion);
    let distances = (0..capture_frames)
        .map(|frame| {
            let time_s = (frame as f64 + 0.5) / SAMPLE_RATE_HZ as f64;
            (START_DISTANCE_M + SPEED_MPS * time_s).min(END_DISTANCE_M) as f32
        })
        .collect::<Vec<_>>();
    let subtenses = distances
        .iter()
        .map(|distance| broadside_subtense(f64::from(*distance)) as f32)
        .collect::<Vec<_>>();
    let config = WidthProfileConfig::wave11(SAMPLE_RATE_HZ);
    let point_profile = windowed_width_profile(
        point.pcm(),
        WidthTrack {
            distances_m: &distances,
            angular_subtenses_rad: None,
        },
        config,
    )
    .expect("profile receding point render");
    let line_profile = windowed_width_profile(
        line.pcm(),
        WidthTrack {
            distances_m: &distances,
            angular_subtenses_rad: Some(&subtenses),
        },
        config,
    )
    .expect("profile receding line render");
    let point_bins = log_distance_bin_medians(&point_profile);
    let line_bins = log_distance_bin_medians(&line_profile);
    let line_metrics = collapse_metrics(&line_bins);
    let point_metrics = collapse_metrics(&point_bins);
    let far_point_delta = (line_bins[line_bins.len() - 1] - point_bins[point_bins.len() - 1]).abs();

    assert!(
        line_metrics.spearman <= MIN_COLLAPSE_SPEARMAN,
        "line collapse rank correlation was too weak: {line_metrics:?}, bins={line_bins:?}"
    );
    assert!(
        line_metrics.max_outward_increase <= MAX_OUTWARD_WIDTH_INCREASE,
        "line width re-expanded beyond tolerance: {line_metrics:?}, bins={line_bins:?}"
    );
    assert!(
        line_metrics.near_to_far_drop >= WIDTH_DROP_MIN,
        "line did not materially collapse: {line_metrics:?}, bins={line_bins:?}"
    );
    assert!(
        far_point_delta <= FAR_POINT_WIDTH_TOLERANCE,
        "far line did not approach matched point width: delta={far_point_delta:.6}, line={line_bins:?}, point={point_bins:?}"
    );
    assert!(
        point_metrics.near_to_far_drop < WIDTH_DROP_MIN,
        "point-shaped failure control accidentally passed material collapse: {point_metrics:?}, bins={point_bins:?}"
    );
    println!(
        "WAVE11 gate=monotonic-collapse line_bins={line_bins:?} point_control_bins={point_bins:?} rho={:.6} max_outward={:.6} drop={:.6} point_control_drop={:.6} far_point_delta={far_point_delta:.6} thresholds=[rho<={MIN_COLLAPSE_SPEARMAN:.2},outward<={MAX_OUTWARD_WIDTH_INCREASE:.2},drop>={WIDTH_DROP_MIN:.2},far_delta<={FAR_POINT_WIDTH_TOLERANCE:.2}] elapsed_s={:.3}",
        line_metrics.spearman,
        line_metrics.max_outward_increase,
        line_metrics.near_to_far_drop,
        point_metrics.near_to_far_drop,
        started.elapsed().as_secs_f64(),
    );
}

#[test]
fn approach_orbit_recede_has_no_added_moving_comb_but_two_points_do() {
    let started = Instant::now();
    const CAPTURE_SECONDS: usize = 12;
    let capture_frames = CAPTURE_SECONDS * SAMPLE_RATE_HZ as usize;
    let program = broadband_program(render_frame_count(capture_frames), 0.10);
    let start = ApiVector::new(0.0, 40.0, 1.5);
    let point_descriptor = descriptor(start, ExtentDescriptor::Point, EAST);
    let line_descriptor = descriptor(
        start,
        ExtentDescriptor::LineSegment {
            length_m: WIDTH_METERS,
        },
        EAST,
    );
    let point = render_capture(
        &[point_descriptor],
        capture_frames,
        &program,
        |time_s, _| orbit_motion(time_s, 0.0),
    );
    let line = render_capture(&[line_descriptor], capture_frames, &program, |time_s, _| {
        orbit_motion(time_s, 0.0)
    });
    let endpoint_descriptors = [
        descriptor(
            ApiVector::new(-WIDTH_HALF_METERS as f32, 40.0, 1.5),
            ExtentDescriptor::Point,
            EAST,
        ),
        descriptor(
            ApiVector::new(WIDTH_HALF_METERS as f32, 40.0, 1.5),
            ExtentDescriptor::Point,
            EAST,
        ),
    ];
    let walking_comb = render_capture(
        &endpoint_descriptors,
        capture_frames,
        &program,
        |time_s, source_index| {
            let offset = if source_index == 0 {
                -WIDTH_HALF_METERS
            } else {
                WIDTH_HALF_METERS
            };
            orbit_motion(time_s, offset)
        },
    );
    let point_interleaved = point.interleaved();
    let line_interleaved = line.interleaved();
    let walking_comb_interleaved = walking_comb.interleaved();
    let spec = stereo_spec();
    let line_report = stereo_reference_moving_spectral_notches(
        spec,
        &line_interleaved,
        &point_interleaved,
        MOVING_NOTCH_WINDOW_FRAMES,
        MOVING_NOTCH_HOP_FRAMES,
    )
    .expect("compare real line against matched point");
    let walking_comb_report = stereo_reference_moving_spectral_notches(
        spec,
        &walking_comb_interleaved,
        &point_interleaved,
        MOVING_NOTCH_WINDOW_FRAMES,
        MOVING_NOTCH_HOP_FRAMES,
    )
    .expect("compare two physical endpoints against matched point");

    assert!(
        line_report.maximum_added_moving_notch_depth_db < MOVING_NOTCH_CEILING_DB,
        "real width renderer added a moving comb: {line_report:?}"
    );
    assert!(
        walking_comb_report.maximum_added_moving_notch_depth_db > MOVING_NOTCH_CEILING_DB,
        "two-point walking-comb control failed to trip the veto: {walking_comb_report:?}"
    );
    println!(
        "WAVE11 gate=no-moving-comb line={:.3}dB line_channels=[L={:.3},R={:.3},sum={:.3}] line_pairs=[L={},R={},sum={}] walking_comb_control={:.3}dB control_channels=[L={:.3},R={:.3},sum={:.3}] threshold={MOVING_NOTCH_CEILING_DB:.1}dB control_pairs=[L={},R={},sum={}] elapsed_s={:.3}",
        line_report.maximum_added_moving_notch_depth_db,
        line_report.left.max_moving_notch_depth_db,
        line_report.right.max_moving_notch_depth_db,
        line_report.mono_sum.max_moving_notch_depth_db,
        line_report.left.moving_window_pair_count,
        line_report.right.moving_window_pair_count,
        line_report.mono_sum.moving_window_pair_count,
        walking_comb_report.maximum_added_moving_notch_depth_db,
        walking_comb_report.left.max_moving_notch_depth_db,
        walking_comb_report.right.max_moving_notch_depth_db,
        walking_comb_report.mono_sum.max_moving_notch_depth_db,
        walking_comb_report.left.moving_window_pair_count,
        walking_comb_report.right.moving_window_pair_count,
        walking_comb_report.mono_sum.moving_window_pair_count,
        started.elapsed().as_secs_f64(),
    );
}

#[test]
fn drive_by_line_matches_point_doppler_while_two_trajectories_smear() {
    let started = Instant::now();
    const CAPTURE_SECONDS: usize = 4;
    const LINE_LENGTH_M: f32 = 16.0;
    const HALF_LINE_M: f64 = LINE_LENGTH_M as f64 / 2.0;
    let capture_frames = CAPTURE_SECONDS * SAMPLE_RATE_HZ as usize;
    let program = tone_program(render_frame_count(capture_frames), 1_000.0, 0.20);
    let initial = ApiVector::new(-30.0, 3.0, 1.5);
    let point_descriptor = descriptor(initial, ExtentDescriptor::Point, EAST);
    let line_descriptor = descriptor(
        initial,
        ExtentDescriptor::LineSegment {
            length_m: LINE_LENGTH_M,
        },
        EAST,
    );
    let point = render_capture(
        &[point_descriptor],
        capture_frames,
        &program,
        |time_s, _| drive_by_motion(time_s, 0.0),
    );
    let line = render_capture(&[line_descriptor], capture_frames, &program, |time_s, _| {
        drive_by_motion(time_s, 0.0)
    });
    let endpoint_descriptors = [
        descriptor(
            ApiVector::new(-30.0 - HALF_LINE_M as f32, 3.0, 1.5),
            ExtentDescriptor::Point,
            EAST,
        ),
        descriptor(
            ApiVector::new(-30.0 + HALF_LINE_M as f32, 3.0, 1.5),
            ExtentDescriptor::Point,
            EAST,
        ),
    ];
    let smeared = render_capture(
        &endpoint_descriptors,
        capture_frames,
        &program,
        |time_s, source_index| {
            let offset = if source_index == 0 {
                -HALF_LINE_M
            } else {
                HALF_LINE_M
            };
            drive_by_motion(time_s, offset)
        },
    );
    let config = PitchTrackConfig::wave11(SAMPLE_RATE_HZ);
    let point_track = windowed_pitch_track(point.pcm(), config).expect("track moving point pitch");
    let line_track = windowed_pitch_track(line.pcm(), config).expect("track moving line pitch");
    let smear_track =
        windowed_pitch_track(smeared.pcm(), config).expect("track two-trajectory smear pitch");
    let line_comparison = compare_pitch_tracks(&line_track, &point_track);
    let smear_comparison = compare_pitch_tracks(&smear_track, &point_track);

    assert_eq!(
        line_comparison.violating_windows, 0,
        "line and matched point pitch diverged: {line_comparison:?}"
    );
    assert_eq!(
        line_comparison.missing_windows, 0,
        "line or point pitch became untrackable: {line_comparison:?}"
    );
    assert!(
        smear_comparison.violating_windows > 0,
        "two-trajectory failure control accidentally preserved one pitch path: {smear_comparison:?}"
    );
    println!(
        "WAVE11 gate=doppler-identity line_max_delta_hz={:.6} line_failures={}/{} smear_control_max_delta_hz={:.6} smear_control_failures={}/{} smear_missing={} threshold_hz={PITCH_IDENTITY_TOLERANCE_HZ:.2} elapsed_s={:.3}",
        line_comparison.maximum_difference_hz,
        line_comparison.violating_windows,
        line_comparison.window_count,
        smear_comparison.maximum_difference_hz,
        smear_comparison.violating_windows,
        smear_comparison.window_count,
        smear_comparison.missing_windows,
        started.elapsed().as_secs_f64(),
    );
}

fn render_capture<F>(
    descriptors: &[MultiSourceDescriptor],
    capture_frames: usize,
    program: &[f32],
    motion_at: F,
) -> StereoCapture
where
    F: Fn(f64, usize) -> SourceMotion,
{
    assert!(!descriptors.is_empty() && descriptors.len() <= MAX_ACTIVE_SOURCES);
    assert_eq!(program.len(), render_frame_count(capture_frames));
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE_HZ as i32,
        frame_size: BLOCK_FRAMES as i32,
    };
    let mut config = S3SimulationConfig::default();
    config.max_occlusion_samples = 1;
    config.reflection_rays = 64;
    config.diffuse_samples = 8;
    config.reflection_bounces = 1;
    config.reflection_duration_s = 0.05;
    config.reflection_order = 0;
    config.pathing_order = 0;
    config.validate_paths = false;
    config.find_alternate_paths = false;
    let (mut simulation, mut render) = build_multi_source_session(
        &qualification_scene(),
        qualification_bake(),
        audio,
        config,
        descriptors,
    )
    .expect("build linked Wave 11 qualification session");
    let mut stage_gains = render
        .take_stage_output_gain_control()
        .expect("new graph owns stage-gain control");
    stage_gains
        .publish(StageOutputGains {
            direct: 1.0,
            pathing: 0.0,
            reflections: 0.0,
        })
        .expect("publish direct-only qualification mix");

    let mut capture = StereoCapture {
        left: Vec::with_capacity(capture_frames),
        right: Vec::with_capacity(capture_frames),
    };
    for (block_index, input) in program.chunks_exact(BLOCK_FRAMES).enumerate() {
        let block_start = block_index * BLOCK_FRAMES;
        let center_frame = block_start + BLOCK_FRAMES / 2;
        let capture_time_s = (center_frame as f64 - PRE_ROLL_FRAMES as f64) / SAMPLE_RATE_HZ as f64;
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for (source_index, source) in sources.iter_mut().enumerate().take(descriptors.len()) {
            *source = motion_at(capture_time_s, source_index);
        }
        let listener_forward = listener_forward_for_sources(&sources[..descriptors.len()]);
        simulation.update_inputs(&SimulationUpdate {
            listener: ListenerState {
                pose: pose(LISTENER_POSITION, listener_forward),
                linear_velocity_mps: ApiVector::default(),
            },
            sources,
        });
        simulation
            .run_direct()
            .expect("run linked direct simulation");
        let source_blocks = (0..descriptors.len())
            .map(|source_index| BackendSourceBlock {
                source_index,
                input_mono: input,
            })
            .collect::<Vec<_>>();
        let mut left = [0.0_f32; BLOCK_FRAMES];
        let mut right = [0.0_f32; BLOCK_FRAMES];
        render_block(
            &mut render,
            &source_blocks,
            listener_forward,
            &mut left,
            &mut right,
        );
        for frame in 0..BLOCK_FRAMES {
            let absolute_frame = block_start + frame;
            if (PRE_ROLL_FRAMES..PRE_ROLL_FRAMES + capture_frames).contains(&absolute_frame) {
                assert!(left[frame].is_finite() && right[frame].is_finite());
                capture.left.push(left[frame]);
                capture.right.push(right[frame]);
            }
        }
    }
    assert_eq!(capture.left.len(), capture_frames);
    assert_eq!(capture.right.len(), capture_frames);
    capture
}

fn render_block(
    render: &mut SteamAudioRenderGraph,
    sources: &[BackendSourceBlock<'_>],
    listener_forward: ApiVector,
    left: &mut [f32],
    right: &mut [f32],
) {
    render
        .render_block(PropagationRenderBlock {
            listener_orientation: ListenerOrientation {
                forward: listener_forward,
                up: UP,
            },
            sources,
            output_left: left,
            output_right: right,
        })
        .expect("render linked Wave 11 block");
}

fn listener_forward_for_sources(sources: &[SourceMotion]) -> ApiVector {
    let inverse_count = 1.0 / sources.len() as f32;
    let center = sources.iter().fold(ApiVector::default(), |sum, source| {
        ApiVector::new(
            sum.east_m + source.pose.position.east_m * inverse_count,
            sum.north_m + source.pose.position.north_m * inverse_count,
            sum.up_m + source.pose.position.up_m * inverse_count,
        )
    });
    let east = center.east_m - LISTENER_POSITION.east_m;
    let north = center.north_m - LISTENER_POSITION.north_m;
    let length = east.hypot(north);
    assert!(length > 0.0 && length.is_finite());
    ApiVector::new(east / length, north / length, 0.0)
}

fn qualification_bake() -> &'static BakedProbeBatch {
    static BAKE: OnceLock<BakedProbeBatch> = OnceLock::new();
    BAKE.get_or_init(|| {
        bake_s3(&S3BakeRequest {
            mesh: qualification_scene(),
            probes: ProbeVolume {
                min_enu_m: MeshVector::new(-2.0, -2.0, 0.0),
                max_enu_m: MeshVector::new(2.0, 2.0, 3.0),
                spacing_m: 2.0,
                height_above_floor_m: 1.5,
            },
            elevated_probe_layers: Vec::new(),
            pathing: PathBakeConfig {
                num_visibility_samples: 1,
                probe_visibility_radius_m: 0.5,
                visibility_threshold: 0.1,
                visibility_range_m: 100.0,
                path_range_m: 100.0,
                num_threads: 1,
            },
        })
        .expect("bake deterministic Wave 11 floor probes once")
    })
}

fn qualification_scene() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            MeshVector::new(-64.0, -64.0, 0.0),
            MeshVector::new(64.0, -64.0, 0.0),
            MeshVector::new(64.0, 64.0, 0.0),
            MeshVector::new(-64.0, 64.0, 0.0),
        ],
        triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
        material_indices: vec![0; 4],
        materials: vec![AcousticMaterial::GROUND],
    }
}

fn descriptor(
    position: ApiVector,
    extent: ExtentDescriptor,
    forward: ApiVector,
) -> MultiSourceDescriptor {
    MultiSourceDescriptor::at(position)
        .with_initial_pose(pose(position, forward))
        .with_extent(extent)
}

fn source_motion(position: ApiVector, forward: ApiVector, velocity: ApiVector) -> SourceMotion {
    SourceMotion {
        active: true,
        pose: pose(position, forward),
        linear_velocity_mps: velocity,
    }
}

fn pose(position: ApiVector, forward: ApiVector) -> Pose {
    Pose {
        position,
        forward,
        up: UP,
    }
}

fn orbit_motion(time_s: f64, east_offset_m: f64) -> SourceMotion {
    const APPROACH_SECONDS: f64 = 3.0;
    const ORBIT_SECONDS: f64 = 6.0;
    const RECEDE_SECONDS: f64 = 3.0;
    const FAR_METERS: f64 = 40.0;
    const ORBIT_RADIUS_METERS: f64 = 2.5;
    let radial_speed = (FAR_METERS - ORBIT_RADIUS_METERS) / APPROACH_SECONDS;
    let (east, north, east_velocity, north_velocity) = if time_s < 0.0 {
        (0.0, FAR_METERS, 0.0, 0.0)
    } else if time_s < APPROACH_SECONDS {
        (0.0, FAR_METERS - radial_speed * time_s, 0.0, -radial_speed)
    } else if time_s < APPROACH_SECONDS + ORBIT_SECONDS {
        let theta = TAU * (time_s - APPROACH_SECONDS) / ORBIT_SECONDS;
        let omega = TAU / ORBIT_SECONDS;
        (
            ORBIT_RADIUS_METERS * theta.sin(),
            ORBIT_RADIUS_METERS * theta.cos(),
            ORBIT_RADIUS_METERS * omega * theta.cos(),
            -ORBIT_RADIUS_METERS * omega * theta.sin(),
        )
    } else if time_s < APPROACH_SECONDS + ORBIT_SECONDS + RECEDE_SECONDS {
        let recede_time = time_s - APPROACH_SECONDS - ORBIT_SECONDS;
        (
            0.0,
            ORBIT_RADIUS_METERS + radial_speed * recede_time,
            0.0,
            radial_speed,
        )
    } else {
        (0.0, FAR_METERS, 0.0, 0.0)
    };
    source_motion(
        ApiVector::new((east + east_offset_m) as f32, north as f32, 1.5),
        EAST,
        ApiVector::new(east_velocity as f32, north_velocity as f32, 0.0),
    )
}

fn drive_by_motion(time_s: f64, east_offset_m: f64) -> SourceMotion {
    const SPEED_MPS: f64 = 15.0;
    let east = -30.0 + SPEED_MPS * time_s + east_offset_m;
    source_motion(
        ApiVector::new(east as f32, 3.0, 1.5),
        EAST,
        ApiVector::new(SPEED_MPS as f32, 0.0, 0.0),
    )
}

fn broadband_program(frame_count: usize, amplitude: f32) -> Vec<f32> {
    let mut state = 0x243f_6a88_85a3_08d3_u64;
    (0..frame_count)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let unit = (state >> 11) as f64 / (1_u64 << 53) as f64;
            amplitude * (2.0 * unit as f32 - 1.0)
        })
        .collect()
}

fn tone_program(frame_count: usize, frequency_hz: f64, amplitude: f32) -> Vec<f32> {
    (0..frame_count)
        .map(|frame| {
            let time_s = frame as f64 / SAMPLE_RATE_HZ as f64;
            amplitude * (TAU * frequency_hz * time_s).sin() as f32
        })
        .collect()
}

fn render_frame_count(capture_frames: usize) -> usize {
    let required = PRE_ROLL_FRAMES + capture_frames;
    required.div_ceil(BLOCK_FRAMES) * BLOCK_FRAMES
}

fn admitted_median_width(profile: &WidthProfile) -> f64 {
    let values = profile
        .windows
        .iter()
        .filter(|window| window.admitted)
        .map(|window| window.width)
        .collect::<Vec<_>>();
    assert_eq!(
        values.len(),
        profile.windows.len(),
        "all qualification windows must be admitted"
    );
    median(&values)
}

fn log_distance_bin_medians(profile: &WidthProfile) -> Vec<f64> {
    let boundaries = COLLAPSE_DISTANCE_CENTERS_M
        .windows(2)
        .map(|pair| (pair[0] * pair[1]).sqrt())
        .collect::<Vec<_>>();
    let mut bins = vec![Vec::<f64>::new(); COLLAPSE_DISTANCE_CENTERS_M.len()];
    for window in profile.windows.iter().filter(|window| window.admitted) {
        let bin = boundaries
            .iter()
            .position(|boundary| window.distance_m < *boundary)
            .unwrap_or(boundaries.len());
        bins[bin].push(window.width);
    }
    bins.iter()
        .enumerate()
        .map(|(index, values)| {
            assert!(
                !values.is_empty(),
                "distance bin {index} has no admitted width windows"
            );
            median(values)
        })
        .collect()
}

fn collapse_metrics(widths: &[f64]) -> CollapseMetrics {
    CollapseMetrics {
        spearman: spearman_against_distance(widths),
        max_outward_increase: widths
            .windows(2)
            .map(|pair| (pair[1] - pair[0]).max(0.0))
            .fold(0.0, f64::max),
        near_to_far_drop: widths[0] - widths[widths.len() - 1],
    }
}

fn spearman_against_distance(widths: &[f64]) -> f64 {
    let mut order = (0..widths.len()).collect::<Vec<_>>();
    order.sort_by(|&left, &right| widths[left].total_cmp(&widths[right]));
    let mut ranks = vec![0.0_f64; widths.len()];
    for (rank, index) in order.into_iter().enumerate() {
        ranks[index] = rank as f64 + 1.0;
    }
    let mean = (widths.len() as f64 + 1.0) / 2.0;
    let mut covariance = 0.0;
    let mut distance_energy = 0.0;
    let mut width_energy = 0.0;
    for (index, width_rank) in ranks.into_iter().enumerate() {
        let distance_rank = index as f64 + 1.0;
        covariance += (distance_rank - mean) * (width_rank - mean);
        distance_energy += (distance_rank - mean).powi(2);
        width_energy += (width_rank - mean).powi(2);
    }
    covariance / (distance_energy * width_energy).sqrt()
}

fn compare_pitch_tracks(candidate: &PitchTrack, point: &PitchTrack) -> PitchComparison {
    assert_eq!(candidate.windows.len(), point.windows.len());
    let mut maximum_difference_hz = 0.0_f64;
    let mut violating_windows = 0;
    let mut missing_windows = 0;
    for (candidate, point) in candidate.windows.iter().zip(&point.windows) {
        match (candidate.fundamental_hz, point.fundamental_hz) {
            (Some(candidate_hz), Some(point_hz)) => {
                let difference_hz = (candidate_hz - point_hz).abs();
                maximum_difference_hz = maximum_difference_hz.max(difference_hz);
                if difference_hz > PITCH_IDENTITY_TOLERANCE_HZ {
                    violating_windows += 1;
                }
            }
            _ => {
                missing_windows += 1;
                violating_windows += 1;
            }
        }
    }
    PitchComparison {
        maximum_difference_hz,
        violating_windows,
        missing_windows,
        window_count: point.windows.len(),
    }
}

fn broadside_subtense(distance_m: f64) -> f64 {
    2.0 * (WIDTH_HALF_METERS / distance_m).atan()
}

fn median(values: &[f64]) -> f64 {
    assert!(!values.is_empty());
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        0.5 * (values[middle - 1] + values[middle])
    } else {
        values[middle]
    }
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE_HZ,
        channels: 2,
    }
}
