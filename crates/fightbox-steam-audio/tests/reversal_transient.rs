//! Ignored, linked-SDK diagnosis harness for the sprint/reversal transient.
//!
//! The default paths point at the locally generated megablock package used by
//! the live workbench report. Override them with `FIGHTBOX_DIAG_PACKAGE` and
//! `FIGHTBOX_DIAG_BAKE` when the evidence bundle lives elsewhere.

use fightbox_api::{
    AssetAnalysis, AssetMeasurementProvenance, EngineConfig, EnuVector3, ExtentDescriptor,
    ListenerState, Pose, ReferenceLevel, SceneCalibration, SourceId, SourceProfile,
};
use fightbox_runtime::backend::{SimulationRunner, SimulationUpdate, SourceMotion};
use fightbox_runtime::{
    BlockProcessor, ProcessBlock, PropagationSnapshot, RuntimeGraph, SimulationCadences,
    SnapshotPublication, SourceBlock, SourcePropagation,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, MultiSourceDescriptor,
    PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata, ReflectionEffectConfig, S3SimulationConfig,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh, build_multi_source_session,
};
use std::env;
use std::f32::consts::PI;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const SAMPLE_RATE: usize = 48_000;
const BLOCK_FRAMES: usize = 128;
const SOURCE: EnuVector3 = EnuVector3::new(292.5, 292.5, 1.5);
const DEFAULT_START_RADIUS_M: f32 = 95.0;
const SPEED_MPS: f32 = 20.0;
const DURATION_SECONDS: f32 = 4.0;
const MOTION_START_SECONDS: f32 = 0.5;
const CLICK_START_SECONDS: f32 = 0.75;
const CLICK_PERIOD_SECONDS: f32 = 0.25;
const DIRECT_PERIOD_BLOCKS: usize = 6;
const PATH_PERIOD_BLOCKS: usize = 25;
const FIXED_REFLECTION_PERIOD_BLOCKS: usize = 75;
const PAIR_SPACING_TOLERANCE_MS: f32 = 3.0;
// The confirmed double-render presents two nearly equal attacks. Requiring
// the secondary to be within 2 dB rejects ordinary weaker echo reordering.
const ARTIFACT_PAIR_MIN_RELATIVE_DB: f32 = -2.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReflectionScheduling {
    Fixed,
    DisplacementBounded,
}

impl ReflectionScheduling {
    fn from_env() -> Self {
        match env::var("FIGHTBOX_DIAG_REFLECTION_SCHEDULING")
            .as_deref()
            .unwrap_or("displacement-bounded")
        {
            "fixed" => Self::Fixed,
            "displacement-bounded" => Self::DisplacementBounded,
            value => panic!("invalid FIGHTBOX_DIAG_REFLECTION_SCHEDULING value {value:?}"),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::DisplacementBounded => "displacement-bounded",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Motion {
    ReversalLine,
    ReversalArc,
    SteadyLine,
    SteadyArc,
    SingleReversalLine,
    SingleReversalArc,
    MatchedSteadyLine,
    MatchedSteadyArc,
}

impl Motion {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "reversal-line" => Some(Self::ReversalLine),
            "reversal-arc" => Some(Self::ReversalArc),
            "steady-line" => Some(Self::SteadyLine),
            "steady-arc" => Some(Self::SteadyArc),
            "single-reversal-line" => Some(Self::SingleReversalLine),
            "single-reversal-arc" => Some(Self::SingleReversalArc),
            "matched-steady-line" => Some(Self::MatchedSteadyLine),
            "matched-steady-arc" => Some(Self::MatchedSteadyArc),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::ReversalLine => "reversal-line",
            Self::ReversalArc => "reversal-arc",
            Self::SteadyLine => "steady-line",
            Self::SteadyArc => "steady-arc",
            Self::SingleReversalLine => "single-reversal-line",
            Self::SingleReversalArc => "single-reversal-arc",
            Self::MatchedSteadyLine => "matched-steady-line",
            Self::MatchedSteadyArc => "matched-steady-arc",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stages {
    direct: bool,
    path: bool,
    reflections: bool,
    freeze_reflections: bool,
}

impl Stages {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Self {
                direct: true,
                path: true,
                reflections: true,
                freeze_reflections: false,
            }),
            "frozen-reflections" => Some(Self {
                direct: true,
                path: true,
                reflections: true,
                freeze_reflections: true,
            }),
            "no-reflections" => Some(Self {
                direct: true,
                path: true,
                reflections: false,
                freeze_reflections: false,
            }),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match (
            self.direct,
            self.path,
            self.reflections,
            self.freeze_reflections,
        ) {
            (true, true, true, false) => "full",
            (true, true, true, true) => "frozen-reflections",
            (true, true, false, false) => "no-reflections",
            _ => "unsupported",
        }
    }
}

#[derive(Debug)]
struct Render {
    name: String,
    left: Vec<f32>,
    right: Vec<f32>,
    clicks: Vec<usize>,
    reversals: Vec<usize>,
    expected_delay_samples: usize,
    reflection_pass_ns: Vec<u64>,
    reflection_ticks: Vec<usize>,
    max_multiplexed_pass_ns: u64,
    render_block_ns: Vec<u64>,
}

#[derive(Clone, Copy, Debug)]
struct Peak {
    sample: usize,
    amplitude: f32,
}

#[derive(Clone, Copy, Debug)]
struct Pair {
    click: usize,
    primary: Peak,
    secondary: Peak,
    spacing_ms: f32,
    relative_db: f32,
    nearest_reversal_ms: f32,
}

#[derive(Clone, Copy, Debug)]
struct ArtifactPair {
    pair: Pair,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock bake"]
fn sprint_reversal_transient_diagnosis() {
    let package = env_path(
        "FIGHTBOX_DIAG_PACKAGE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox",
    );
    let bake = env_path(
        "FIGHTBOX_DIAG_BAKE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.baked",
    );
    let output = env_path(
        "FIGHTBOX_DIAG_OUT",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../target/reversal-diagnosis"
        ),
    );
    fs::create_dir_all(&output).expect("create diagnosis output");

    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let baked = load_baked(&bake);
    let reversal_ms = env::var("FIGHTBOX_DIAG_REVERSAL_MS")
        .ok()
        .map(|value| value.parse::<u32>().expect("integer reversal milliseconds"))
        .unwrap_or(200);
    let radius_m = env::var("FIGHTBOX_DIAG_RADIUS_M")
        .ok()
        .map(|value| value.parse::<f32>().expect("numeric radius meters"))
        .unwrap_or(DEFAULT_START_RADIUS_M);
    let motions = parse_list(
        "FIGHTBOX_DIAG_MOTIONS",
        "reversal-line,reversal-arc,steady-line,steady-arc",
        Motion::parse,
    );
    let stages = parse_list(
        "FIGHTBOX_DIAG_STAGES",
        "full,frozen-reflections,no-reflections",
        Stages::parse,
    );
    let reflection_scheduling = ReflectionScheduling::from_env();

    println!(
        "DIAG setup head={} reversal_ms={} radius_m={} reflection_scheduling={} mesh_vertices={} mesh_triangles={} probes={} ir_seconds=0.30",
        option_env!("VERGEN_GIT_SHA").unwrap_or("current-worktree"),
        reversal_ms,
        radius_m,
        reflection_scheduling.name(),
        mesh.vertices_enu_m.len(),
        mesh.triangles.len(),
        baked.metadata.probe_count,
    );

    let mut renders = Vec::new();
    for motion in motions {
        for stage in stages.iter().copied() {
            let name = format!(
                "{}-{}-{}ms-r{radius_m:.0}m",
                motion.name(),
                stage.name(),
                reversal_ms
            );
            println!("DIAG render-start {name}");
            let render = render_case(
                &mesh,
                &baked,
                motion,
                stage,
                reversal_ms,
                radius_m,
                reflection_scheduling,
            );
            write_float_wav(
                &output.join(format!("{name}.wav")),
                &render.left,
                &render.right,
            )
            .expect("write diagnosis WAV");
            let pairs = onset_pairs(&render);
            print_pairs(&render.name, &pairs);
            print_reflection_perf(&render);
            print_render_perf(&render);
            renders.push(render);
        }
    }

    let report = output.join(format!("summary-{reversal_ms}ms-r{radius_m:.0}m.txt"));
    let mut text = String::new();
    for render in &renders {
        let pairs = onset_pairs(render);
        text.push_str(&format!("{}\n", render.name));
        for pair in pairs {
            text.push_str(&format!(
                "click={} primary_ms={:.3} secondary_ms={:.3} spacing_ms={:.3} relative_db={:.2} nearest_reversal_ms={:.3}\n",
                pair.click,
                samples_to_ms(pair.primary.sample),
                samples_to_ms(pair.secondary.sample),
                pair.spacing_ms,
                pair.relative_db,
                pair.nearest_reversal_ms,
            ));
        }
    }
    for full in renders
        .iter()
        .filter(|render| render.name.contains("-full-"))
    {
        let frozen_name = full.name.replace("-full-", "-frozen-reflections-");
        let Some(frozen) = renders.iter().find(|render| render.name == frozen_name) else {
            continue;
        };
        let artifacts = artifact_pairs(&onset_pairs(full), &onset_pairs(frozen));
        print_artifact_pairs(&full.name, &frozen.name, &artifacts);
        text.push_str(&format!(
            "differential full={} frozen={} artifact_pairs={}\n",
            full.name,
            frozen.name,
            artifacts.len()
        ));
        for artifact in artifacts {
            let pair = artifact.pair;
            text.push_str(&format!(
                "artifact click={} spacing_ms={:.3} relative_db={:.2} nearest_reversal_ms={:.3}\n",
                pair.click, pair.spacing_ms, pair.relative_db, pair.nearest_reversal_ms,
            ));
        }
    }
    fs::write(&report, text).expect("write diagnosis summary");
    println!("DIAG output={}", output.display());
}

fn render_case(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    motion: Motion,
    stages: Stages,
    reversal_ms: u32,
    radius_m: f32,
    reflection_scheduling: ReflectionScheduling,
) -> Render {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK_FRAMES as i32,
    };
    let simulation_config = S3SimulationConfig {
        reflection_rays: 4_096,
        reflection_bounces: 3,
        reflection_duration_s: 0.30,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        simulation_threads: 1,
        pathing_order: 2,
        ..S3SimulationConfig::default()
    };
    let descriptors = [MultiSourceDescriptor::at(SOURCE)];
    let (mut simulation, backend) =
        build_multi_source_session(mesh, baked, audio, simulation_config, &descriptors)
            .expect("build linked Steam Audio session");

    let (mut propagation_writer, propagation_reader) =
        SnapshotPublication::new(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: u64::MAX,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
    // Keep the publication live and its timestamp fresh, matching the live
    // backend path while leaving the runtime-owned delay at its intended zero.
    propagation_writer.publish(PropagationSnapshot {
        sequence: 2,
        simulated_at_ns: u64::MAX,
        sources: std::array::from_fn(|index| SourcePropagation {
            active: index == 0,
            target_delay_samples: 0.0,
            left_gain: 1.0,
            right_gain: 1.0,
        }),
    });
    let engine = EngineConfig {
        sample_rate_hz: SAMPLE_RATE as u32,
        block_size_frames: BLOCK_FRAMES as u32,
        max_active_sources: 1,
        ..EngineConfig::default()
    };
    let mut graph =
        RuntimeGraph::new_with_backend(engine, propagation_reader, Box::new(backend)).unwrap();
    graph
        .set_source(0, &source_profile(), SceneCalibration::default())
        .unwrap();

    let total_frames = (DURATION_SECONDS * SAMPLE_RATE as f32) as usize;
    let total_blocks = total_frames.div_ceil(BLOCK_FRAMES);
    let mut left_all = Vec::with_capacity(total_blocks * BLOCK_FRAMES);
    let mut right_all = Vec::with_capacity(total_blocks * BLOCK_FRAMES);
    let reversal_period_samples = reversal_ms as usize * SAMPLE_RATE / 1_000;
    let motion_start_sample = (MOTION_START_SECONDS * SAMPLE_RATE as f32) as usize;
    let clicks = click_samples(
        total_blocks * BLOCK_FRAMES,
        motion_start_sample + reversal_period_samples,
    );
    let reversals = match motion {
        Motion::SingleReversalLine | Motion::SingleReversalArc => {
            vec![motion_start_sample + reversal_period_samples]
        }
        Motion::MatchedSteadyLine | Motion::MatchedSteadyArc => Vec::new(),
        _ => (motion_start_sample + reversal_period_samples..total_blocks * BLOCK_FRAMES)
            .step_by(reversal_period_samples)
            .collect::<Vec<_>>(),
    };
    let mut input = vec![0.0_f32; BLOCK_FRAMES];
    let mut left = vec![0.0_f32; BLOCK_FRAMES];
    let mut right = vec![0.0_f32; BLOCK_FRAMES];
    let mut source_motion = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
    source_motion[0] = SourceMotion {
        active: true,
        pose: pose(SOURCE, EnuVector3::new(1.0, 0.0, 0.0)),
        linear_velocity_mps: EnuVector3::default(),
    };
    let cadences = SimulationCadences::default();
    let reflection_period_samples = SAMPLE_RATE.div_ceil(cadences.reflections_hz as usize);
    let reflection_min_period_samples = SAMPLE_RATE.div_ceil(cadences.reflection_max_hz as usize);
    let mut next_periodic_reflection_sample = 0;
    let mut next_reflection_eligible_sample = 0;
    let mut reflection_displacement = None;
    let mut reflection_pass_ns = Vec::new();
    let mut reflection_ticks = Vec::new();
    let mut max_multiplexed_pass_ns = 0_u64;
    let mut render_block_ns = Vec::with_capacity(total_blocks);

    for block in 0..total_blocks {
        let multiplexed_started = Instant::now();
        let block_start = block * BLOCK_FRAMES;
        let (listener, velocity) = listener_at(
            motion,
            block_start,
            reversal_period_samples,
            motion_start_sample,
            radius_m,
        );
        let listener_state = ListenerState {
            pose: pose(listener, EnuVector3::new(1.0, 0.0, 0.0)),
            linear_velocity_mps: velocity,
        };
        let update = SimulationUpdate {
            listener: listener_state,
            sources: source_motion,
        };
        let displacement =
            reflection_displacement.get_or_insert_with(|| ReflectionDisplacement::new(update));
        displacement.observe(update);
        simulation.update_inputs(&update);
        if stages.direct && block % DIRECT_PERIOD_BLOCKS == 0 {
            simulation.run_direct().expect("direct simulation");
        }
        if stages.path && block % PATH_PERIOD_BLOCKS == 0 {
            simulation.run_pathing().expect("path simulation");
        }
        let periodic_reflection_due = match reflection_scheduling {
            ReflectionScheduling::Fixed => block % FIXED_REFLECTION_PERIOD_BLOCKS == 0,
            ReflectionScheduling::DisplacementBounded => {
                block_start >= next_periodic_reflection_sample
            }
        };
        let displacement_reflection_due = reflection_scheduling
            == ReflectionScheduling::DisplacementBounded
            && displacement.exceeded(cadences.reflection_max_displacement_m);
        let reflection_due = (periodic_reflection_due || displacement_reflection_due)
            && block_start >= next_reflection_eligible_sample;
        if stages.reflections && reflection_due && (!stages.freeze_reflections || block == 0) {
            let started = Instant::now();
            simulation.run_reflections().expect("reflection simulation");
            reflection_pass_ns.push(elapsed_ns(started));
            reflection_ticks.push(block_start);
            displacement.reset();
            next_reflection_eligible_sample = block_start + reflection_min_period_samples;
            if reflection_scheduling == ReflectionScheduling::DisplacementBounded
                && periodic_reflection_due
            {
                while next_periodic_reflection_sample <= block_start {
                    next_periodic_reflection_sample += reflection_period_samples;
                }
            }
        }
        max_multiplexed_pass_ns = max_multiplexed_pass_ns.max(elapsed_ns(multiplexed_started));

        input.fill(0.0);
        for click in clicks
            .iter()
            .copied()
            .filter(|click| (block_start..block_start + BLOCK_FRAMES).contains(click))
        {
            // A short, zero-DC click has a clear attack without exciting a
            // permanent step in the retained effects.
            let local = click - block_start;
            input[local] = 1.0;
            if local + 1 < input.len() {
                input[local + 1] = -0.75;
            }
            if local + 2 < input.len() {
                input[local + 2] = -0.25;
            }
        }
        graph.set_listener_state(listener_state);
        let sources = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let render_started = Instant::now();
        graph
            .process_block(ProcessBlock {
                now_ns: block as u64 * 2_666_667,
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        render_block_ns.push(elapsed_ns(render_started));
        assert!(
            left.iter().chain(&right).all(|sample| sample.is_finite()),
            "non-finite output in {}-{}",
            motion.name(),
            stages.name()
        );
        left_all.extend_from_slice(&left);
        right_all.extend_from_slice(&right);
    }

    Render {
        name: format!(
            "{}-{}-{}ms-r{radius_m:.0}m",
            motion.name(),
            stages.name(),
            reversal_ms
        ),
        left: left_all,
        right: right_all,
        clicks,
        reversals,
        expected_delay_samples: (radius_m * SAMPLE_RATE as f32 / 343.0).round() as usize,
        reflection_pass_ns,
        reflection_ticks,
        max_multiplexed_pass_ns,
        render_block_ns,
    }
}

struct ReflectionDisplacement {
    previous: SimulationUpdate,
    listener_m: f64,
    sources_m: [f64; fightbox_runtime::MAX_ACTIVE_SOURCES],
}

impl ReflectionDisplacement {
    fn new(initial: SimulationUpdate) -> Self {
        Self {
            previous: initial,
            listener_m: 0.0,
            sources_m: [0.0; fightbox_runtime::MAX_ACTIVE_SOURCES],
        }
    }

    fn observe(&mut self, current: SimulationUpdate) {
        self.listener_m += position_distance(
            self.previous.listener.pose.position,
            current.listener.pose.position,
        );
        for ((distance_m, previous), current) in self
            .sources_m
            .iter_mut()
            .zip(&self.previous.sources)
            .zip(&current.sources)
        {
            if previous.active && current.active {
                *distance_m += position_distance(previous.pose.position, current.pose.position);
            } else {
                *distance_m = 0.0;
            }
        }
        self.previous = current;
    }

    fn exceeded(&self, max_displacement_m: f32) -> bool {
        let max_displacement_m = f64::from(max_displacement_m);
        self.listener_m >= max_displacement_m
            || self
                .sources_m
                .iter()
                .any(|distance_m| *distance_m >= max_displacement_m)
    }

    fn reset(&mut self) {
        self.listener_m = 0.0;
        self.sources_m.fill(0.0);
    }
}

fn position_distance(left: EnuVector3, right: EnuVector3) -> f64 {
    let east = f64::from(right.east_m) - f64::from(left.east_m);
    let north = f64::from(right.north_m) - f64::from(left.north_m);
    let up = f64::from(right.up_m) - f64::from(left.up_m);
    (east * east + north * north + up * up).sqrt()
}

fn elapsed_ns(started: Instant) -> u64 {
    started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

fn listener_at(
    motion: Motion,
    sample: usize,
    reversal_period_samples: usize,
    motion_start_sample: usize,
    radius_m: f32,
) -> (EnuVector3, EnuVector3) {
    let base = EnuVector3::new(SOURCE.east_m - radius_m, SOURCE.north_m, 1.5);
    if sample < motion_start_sample {
        return (base, EnuVector3::default());
    }
    let elapsed_samples = sample - motion_start_sample;
    let period = elapsed_samples / reversal_period_samples;
    let within = elapsed_samples % reversal_period_samples;
    let direction = match motion {
        Motion::SteadyLine
        | Motion::SteadyArc
        | Motion::MatchedSteadyLine
        | Motion::MatchedSteadyArc => 1.0,
        Motion::SingleReversalLine | Motion::SingleReversalArc => {
            if elapsed_samples < reversal_period_samples {
                1.0
            } else {
                -1.0
            }
        }
        _ => {
            if period % 2 == 0 {
                1.0
            } else {
                -1.0
            }
        }
    };
    let signed_distance = match motion {
        Motion::ReversalLine | Motion::ReversalArc => {
            let completed = if period % 2 == 0 { 0.0 } else { 1.0 };
            (completed * reversal_period_samples as f32 + direction * within as f32) * SPEED_MPS
                / SAMPLE_RATE as f32
        }
        Motion::SteadyLine | Motion::SteadyArc => {
            elapsed_samples as f32 * SPEED_MPS / SAMPLE_RATE as f32
        }
        Motion::SingleReversalLine | Motion::SingleReversalArc => {
            let leg = reversal_period_samples as f32;
            let traveled_samples = if elapsed_samples < reversal_period_samples {
                elapsed_samples as f32
            } else {
                leg - (elapsed_samples - reversal_period_samples) as f32
            };
            traveled_samples * SPEED_MPS / SAMPLE_RATE as f32
        }
        Motion::MatchedSteadyLine | Motion::MatchedSteadyArc => {
            elapsed_samples as f32 * SPEED_MPS / SAMPLE_RATE as f32
        }
    };
    match motion {
        Motion::ReversalLine
        | Motion::SteadyLine
        | Motion::SingleReversalLine
        | Motion::MatchedSteadyLine => (
            EnuVector3::new(base.east_m, base.north_m + signed_distance, 1.5),
            EnuVector3::new(0.0, direction * SPEED_MPS, 0.0),
        ),
        Motion::ReversalArc
        | Motion::SteadyArc
        | Motion::SingleReversalArc
        | Motion::MatchedSteadyArc => {
            let angle = PI - signed_distance / radius_m;
            let position = EnuVector3::new(
                SOURCE.east_m + radius_m * angle.cos(),
                SOURCE.north_m + radius_m * angle.sin(),
                1.5,
            );
            let velocity = EnuVector3::new(
                direction * SPEED_MPS * angle.sin(),
                -direction * SPEED_MPS * angle.cos(),
                0.0,
            );
            (position, velocity)
        }
    }
}

fn source_profile() -> SourceProfile {
    SourceProfile {
        id: SourceId::new("reversal-click"),
        pose: pose(SOURCE, EnuVector3::new(1.0, 0.0, 0.0)),
        reference_level: ReferenceLevel::CreativeDb { db: 0.0 },
        asset_analysis: AssetAnalysis::new(
            -20.0,
            -1.0,
            AssetMeasurementProvenance::new("reversal-diagnosis/v1").unwrap(),
        )
        .unwrap(),
        extent: ExtentDescriptor::Point,
        max_speed_mps: SPEED_MPS,
    }
}

fn pose(position: EnuVector3, forward: EnuVector3) -> Pose {
    Pose {
        position,
        forward,
        up: EnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn click_samples(total_frames: usize, first_reversal: usize) -> Vec<usize> {
    if let Ok(offset_ms) = env::var("FIGHTBOX_DIAG_CLICK_OFFSET_MS") {
        let offset = offset_ms
            .parse::<f32>()
            .expect("FIGHTBOX_DIAG_CLICK_OFFSET_MS must be numeric");
        let click = (first_reversal as f32 + offset * SAMPLE_RATE as f32 / 1_000.0).round();
        assert!(click >= 0.0 && click < total_frames as f32);
        return vec![click as usize];
    }
    let start = (CLICK_START_SECONDS * SAMPLE_RATE as f32) as usize;
    let period = (CLICK_PERIOD_SECONDS * SAMPLE_RATE as f32) as usize;
    (start..total_frames).step_by(period).collect()
}

fn onset_pairs(render: &Render) -> Vec<Pair> {
    let envelope = energy_envelope(&render.left, &render.right, 24);
    let mut pairs = Vec::new();
    for (click_index, click) in render.clicks.iter().copied().enumerate() {
        let expected = click + render.expected_delay_samples;
        let search_start = expected.saturating_sub(samples_from_ms(20.0));
        let search_end = (expected + samples_from_ms(120.0)).min(envelope.len());
        if search_start >= search_end {
            continue;
        }
        let mut peaks = local_peaks(&envelope, search_start, search_end);
        peaks.sort_by(|left, right| right.amplitude.total_cmp(&left.amplitude));
        let Some(primary) = peaks.first().copied() else {
            continue;
        };
        let secondary = peaks.iter().copied().find(|candidate| {
            let spacing = candidate.sample.abs_diff(primary.sample);
            (samples_from_ms(5.0)..=samples_from_ms(80.0)).contains(&spacing)
        });
        let Some(secondary) = secondary else {
            continue;
        };
        let spacing_ms = samples_to_ms(primary.sample.abs_diff(secondary.sample));
        let relative_db = 20.0 * (secondary.amplitude / primary.amplitude.max(1.0e-20)).log10();
        let nearest_reversal_ms = render
            .reversals
            .iter()
            .map(|reversal| {
                (*reversal as isize - click as isize) as f32 * 1_000.0 / SAMPLE_RATE as f32
            })
            .min_by(|left, right| left.abs().total_cmp(&right.abs()))
            .unwrap_or(f32::NAN);
        pairs.push(Pair {
            click: click_index,
            primary,
            secondary,
            spacing_ms,
            relative_db,
            nearest_reversal_ms,
        });
    }
    pairs
}

fn artifact_pairs(full: &[Pair], frozen: &[Pair]) -> Vec<ArtifactPair> {
    full.iter()
        .copied()
        .filter(|full_pair| {
            if full_pair.relative_db < ARTIFACT_PAIR_MIN_RELATIVE_DB
                || frozen.iter().any(|frozen_pair| {
                    (frozen_pair.spacing_ms - full_pair.spacing_ms).abs()
                        <= PAIR_SPACING_TOLERANCE_MS
                })
            {
                return false;
            }
            // IR adoption can only create the confirmed premature duplicate:
            // a near-equal secondary arriving materially sooner than the
            // frozen control's pair for the same click. Longer spacings are
            // normal motion-dependent echo evolution, not reversal doubling.
            frozen.iter().any(|frozen_pair| {
                frozen_pair.click == full_pair.click
                    && full_pair.spacing_ms + PAIR_SPACING_TOLERANCE_MS < frozen_pair.spacing_ms
            })
        })
        .map(|pair| ArtifactPair { pair })
        .collect()
}

fn energy_envelope(left: &[f32], right: &[f32], radius: usize) -> Vec<f32> {
    let power = left
        .iter()
        .copied()
        .zip(right.iter().copied())
        .map(|(left, right)| 0.5 * (left * left + right * right))
        .collect::<Vec<_>>();
    let mut prefix = Vec::with_capacity(power.len() + 1);
    prefix.push(0.0_f64);
    for value in power {
        prefix.push(prefix.last().copied().unwrap() + f64::from(value));
    }
    (0..left.len())
        .map(|index| {
            let start = index.saturating_sub(radius);
            let end = (index + radius + 1).min(left.len());
            ((prefix[end] - prefix[start]) / (end - start) as f64).sqrt() as f32
        })
        .collect()
}

fn local_peaks(envelope: &[f32], start: usize, end: usize) -> Vec<Peak> {
    let floor = envelope[start..end].iter().copied().fold(0.0_f32, f32::max) * 0.01;
    let separation = samples_from_ms(1.0);
    let mut peaks = Vec::<Peak>::new();
    for sample in start + 1..end.saturating_sub(1) {
        let amplitude = envelope[sample];
        if amplitude < floor
            || amplitude < envelope[sample - 1]
            || amplitude <= envelope[sample + 1]
        {
            continue;
        }
        if let Some(previous) = peaks.last_mut()
            && sample - previous.sample < separation
        {
            if amplitude > previous.amplitude {
                *previous = Peak { sample, amplitude };
            }
        } else {
            peaks.push(Peak { sample, amplitude });
        }
    }
    peaks
}

fn print_pairs(name: &str, pairs: &[Pair]) {
    let strong = pairs
        .iter()
        .filter(|pair| pair.relative_db >= -18.0)
        .collect::<Vec<_>>();
    let median_relative_db = median_f32(
        &pairs
            .iter()
            .map(|pair| pair.relative_db)
            .collect::<Vec<_>>(),
    );
    println!(
        "DIAG result name={name} pairs={} strong_pairs={}/{} secondary_median_db={median_relative_db:.2}",
        pairs.len(),
        strong.len(),
        pairs.len(),
    );
    for pair in strong {
        println!(
            "DIAG pair name={name} click={} spacing_ms={:.3} relative_db={:.2} nearest_reversal_ms={:.3}",
            pair.click, pair.spacing_ms, pair.relative_db, pair.nearest_reversal_ms
        );
    }
}

fn print_artifact_pairs(full_name: &str, frozen_name: &str, artifacts: &[ArtifactPair]) {
    let levels = artifacts
        .iter()
        .map(|artifact| artifact.pair.relative_db)
        .collect::<Vec<_>>();
    println!(
        "DIAG differential full={full_name} frozen={frozen_name} artifact_pairs={} artifact_median_db={:.2}",
        artifacts.len(),
        median_f32(&levels),
    );
    for artifact in artifacts {
        let pair = artifact.pair;
        println!(
            "DIAG artifact full={full_name} click={} spacing_ms={:.3} relative_db={:.2} nearest_reversal_ms={:.3}",
            pair.click, pair.spacing_ms, pair.relative_db, pair.nearest_reversal_ms,
        );
    }
}

fn print_reflection_perf(render: &Render) {
    let reflection_p50_ns = percentile_u64(&render.reflection_pass_ns, 50.0);
    let reflection_p99_ns = percentile_u64(&render.reflection_pass_ns, 99.0);
    let reflection_max_ns = render.reflection_pass_ns.iter().copied().max().unwrap_or(0);
    let minimum_tick_gap_samples = render
        .reflection_ticks
        .windows(2)
        .map(|ticks| ticks[1] - ticks[0])
        .min()
        .unwrap_or(0);
    let tightest_period_ns = 1_000_000_000_u64 / u64::from(SimulationCadences::default().direct_hz);
    println!(
        "DIAG reflection-perf name={} passes={} p50_ms={:.3} p99_ms={:.3} max_ms={:.3} minimum_tick_gap_ms={:.3} max_multiplexed_ms={:.3} tightest_worker_period_ms={:.3} periods_met={}",
        render.name,
        render.reflection_pass_ns.len(),
        reflection_p50_ns as f64 / 1_000_000.0,
        reflection_p99_ns as f64 / 1_000_000.0,
        reflection_max_ns as f64 / 1_000_000.0,
        samples_to_ms(minimum_tick_gap_samples),
        render.max_multiplexed_pass_ns as f64 / 1_000_000.0,
        tightest_period_ns as f64 / 1_000_000.0,
        render.max_multiplexed_pass_ns < tightest_period_ns,
    );
}

fn print_render_perf(render: &Render) {
    println!(
        "DIAG render-perf name={} blocks={} p50_us={:.3} p99_us={:.3} max_us={:.3}",
        render.name,
        render.render_block_ns.len(),
        percentile_u64(&render.render_block_ns, 50.0) as f64 / 1_000.0,
        percentile_u64(&render.render_block_ns, 99.0) as f64 / 1_000.0,
        render.render_block_ns.iter().copied().max().unwrap_or(0) as f64 / 1_000.0,
    );
}

fn percentile_u64(values: &[u64], percentile: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let rank =
        (((percentile.clamp(0.0, 100.0) / 100.0) * sorted.len() as f64).ceil() as usize).max(1) - 1;
    sorted[rank]
}

fn median_f32(values: &[f32]) -> f32 {
    if values.is_empty() {
        return f32::NAN;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f32::total_cmp);
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        0.5 * (sorted[middle - 1] + sorted[middle])
    } else {
        sorted[middle]
    }
}

fn samples_from_ms(milliseconds: f32) -> usize {
    (milliseconds * SAMPLE_RATE as f32 / 1_000.0).round() as usize
}

fn samples_to_ms(samples: usize) -> f32 {
    samples as f32 * 1_000.0 / SAMPLE_RATE as f32
}

fn parse_list<T: Copy>(variable: &str, default: &str, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    env::var(variable)
        .unwrap_or_else(|_| default.to_owned())
        .split(',')
        .map(|value| {
            parse(value.trim()).unwrap_or_else(|| panic!("invalid {variable} entry {value:?}"))
        })
        .collect()
}

fn env_path(variable: &str, default: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

fn load_megablock_mesh(path: &Path) -> SceneMesh {
    let bytes = fs::read(path).expect("read megablock mesh.bin");
    assert!(bytes.len() >= 20 && &bytes[..8] == b"FBXMESH\0");
    assert_eq!(read_u32(&bytes, 8), 1);
    let vertex_count = read_u32(&bytes, 12) as usize;
    let triangle_count = read_u32(&bytes, 16) as usize;
    assert_eq!(bytes.len(), 20 + vertex_count * 12 + triangle_count * 16);
    let mut cursor = 20;
    let mut vertices = Vec::with_capacity(vertex_count);
    for _ in 0..vertex_count {
        vertices.push(fightbox_steam_audio::EnuVector3::new(
            read_f32(&bytes, cursor),
            read_f32(&bytes, cursor + 4),
            read_f32(&bytes, cursor + 8),
        ));
        cursor += 12;
    }
    let mut triangles = Vec::with_capacity(triangle_count);
    for _ in 0..triangle_count {
        triangles.push([
            read_u32(&bytes, cursor) as i32,
            read_u32(&bytes, cursor + 4) as i32,
            read_u32(&bytes, cursor + 8) as i32,
        ]);
        cursor += 12;
    }
    let mut material_indices = Vec::with_capacity(triangle_count);
    for _ in 0..triangle_count {
        material_indices.push(read_u32(&bytes, cursor) as i32);
        cursor += 4;
    }
    SceneMesh {
        vertices_enu_m: vertices,
        triangles,
        material_indices,
        // Material IDs in the deterministic package are the sorted
        // asphalt/brick/concrete/glass/grass table.
        materials: vec![
            AcousticMaterial {
                absorption: [0.02, 0.03, 0.04],
                scattering: 0.08,
                transmission: [0.0; 3],
            },
            AcousticMaterial {
                absorption: [0.03, 0.04, 0.07],
                scattering: 0.15,
                transmission: [0.0; 3],
            },
            AcousticMaterial {
                absorption: [0.02, 0.03, 0.05],
                scattering: 0.10,
                transmission: [0.0; 3],
            },
            AcousticMaterial {
                absorption: [0.08, 0.05, 0.03],
                scattering: 0.05,
                transmission: [0.12, 0.08, 0.04],
            },
            AcousticMaterial {
                absorption: [0.10, 0.35, 0.65],
                scattering: 0.40,
                transmission: [0.0; 3],
            },
        ],
    }
}

fn load_baked(path: &Path) -> BakedProbeBatch {
    let metadata = fs::read_to_string(path.join("probe-batch-metadata.json"))
        .expect("read megablock probe metadata");
    let bytes = fs::read(path.join("probe-batch.bin")).expect("read megablock probe batch");
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: json_u64(&metadata, "probe_count") as u32,
            path_data_size_bytes: json_u64(&metadata, "path_data_size_bytes"),
            serialized_size_bytes: json_u64(&metadata, "serialized_size_bytes"),
            content_sha256: json_string(&metadata, "content_sha256"),
            bake_progress_callback_count: json_u64(&metadata, "bake_progress_callback_count")
                as u32,
            final_bake_progress_millionths: json_u64(&metadata, "final_bake_progress_millionths")
                as u32,
        },
        bytes,
    };
    baked.validate().expect("validate megablock probe batch");
    baked
}

fn json_u64(json: &str, field: &str) -> u64 {
    let tail = json_field_tail(json, field);
    let digits = tail
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().unwrap_or_else(|_| panic!("numeric {field}"))
}

fn json_string(json: &str, field: &str) -> String {
    let tail = json_field_tail(json, field).trim_start();
    let tail = tail
        .strip_prefix('"')
        .unwrap_or_else(|| panic!("string {field}"));
    tail.split('"').next().unwrap().to_owned()
}

fn json_field_tail<'a>(json: &'a str, field: &str) -> &'a str {
    let needle = format!("\"{field}\":");
    json.split_once(&needle)
        .unwrap_or_else(|| panic!("metadata field {field}"))
        .1
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(read_u32(bytes, offset))
}

fn write_float_wav(path: &Path, left: &[f32], right: &[f32]) -> io::Result<()> {
    assert_eq!(left.len(), right.len());
    let data_bytes = (left.len() * 2 * size_of::<f32>()) as u32;
    let mut file = fs::File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + data_bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&3_u16.to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&(SAMPLE_RATE as u32).to_le_bytes())?;
    file.write_all(&((SAMPLE_RATE * 2 * size_of::<f32>()) as u32).to_le_bytes())?;
    file.write_all(&((2 * size_of::<f32>()) as u16).to_le_bytes())?;
    file.write_all(&(32_u16).to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_bytes.to_le_bytes())?;
    for (left, right) in left.iter().zip(right) {
        file.write_all(&left.to_le_bytes())?;
        file.write_all(&right.to_le_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod differential_metric_tests {
    use super::*;

    fn pair(click: usize, spacing_ms: f32, relative_db: f32) -> Pair {
        Pair {
            click,
            primary: Peak {
                sample: 100,
                amplitude: 1.0,
            },
            secondary: Peak {
                sample: 200,
                amplitude: 0.5,
            },
            spacing_ms,
            relative_db,
            nearest_reversal_ms: 0.0,
        }
    }

    #[test]
    fn premature_near_equal_pair_absent_from_control_is_an_artifact() {
        let full = [pair(0, 18.7, -0.7), pair(1, 36.7, -1.0)];
        let frozen = [pair(0, 36.7, -0.8), pair(1, 39.6, -0.9)];

        let artifacts = artifact_pairs(&full, &frozen);

        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].pair.click, 0);
        assert_eq!(artifacts[0].pair.spacing_ms, 18.7);
    }

    #[test]
    fn pair_at_tolerance_boundary_is_not_an_artifact() {
        let full = [pair(3, 36.0, -2.0)];
        let frozen = [pair(3, 39.0, -3.0)];

        assert!(artifact_pairs(&full, &frozen).is_empty());
    }

    #[test]
    fn weaker_or_later_motion_echoes_are_not_doubling_artifacts() {
        let full = [pair(0, 18.0, -2.1), pair(1, 46.0, -0.5)];
        let frozen = [pair(0, 36.0, -1.0), pair(1, 36.0, -1.0)];

        assert!(artifact_pairs(&full, &frozen).is_empty());
    }

    #[test]
    fn control_spacing_can_match_at_another_click() {
        let full = [pair(0, 40.9, -0.5)];
        let frozen = [pair(0, 36.7, -1.0), pair(1, 40.8, -1.0)];

        assert!(artifact_pairs(&full, &frozen).is_empty());
    }
}
