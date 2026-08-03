use super::*;
use crate::{
    AcousticMaterial, BakedProbeBatch, DirectOcclusionMode, PROBE_BATCH_METADATA_SCHEMA,
    PathQualityLevel, ProbeBatchMetadata, QualityGovernorTelemetry, ReflectionEffectConfig,
    ReflectionQualityLevel, ReverbStrategy, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION,
    SceneMesh, SourceQualityLevel, StageOutputGains,
};
use fightbox_api::EnuVector3 as ApiEnuVector3;
use fightbox_runtime::SnapshotWriter;
use fightbox_runtime::backend::{
    BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES, PropagationRenderBlock,
    SimulationUpdate, SourceMotion,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const SOURCE: ApiEnuVector3 = ApiEnuVector3::new(102.5, 102.5, 1.5);

// Compiled footprint of synth/1/1/0. The listener remains 1.5 m east of the
// building and crosses the source-to-southeast-corner tangent while walking
// north. Positive shadow distance is geometrically behind the southeast corner.
const OCCLUDER_WEST_M: f32 = 113.0;
const OCCLUDER_EAST_M: f32 = 147.0;
const OCCLUDER_SOUTH_M: f32 = 113.2;
const OCCLUDER_NORTH_M: f32 = 146.8;
const OCCLUDER_HEIGHT_M: f32 = 54.4;
const WALK_EAST_M: f32 = 148.5;
const WALK_START_NORTH_M: f32 = 108.0;
const WALK_POSITION_COUNT: usize = 12;
const WALK_STEP_M: f32 = 1.0;

// At least one full 1.5 s IR plus 0.377 s of margin is rendered after every
// position change. Stage changes then get four adoption blocks before a
// deterministic 0.512 s broadband RMS capture.
const POSITION_SETTLE_BLOCKS: usize = 704;
const STAGE_ADOPTION_BLOCKS: usize = 4;
const STAGE_MEASURE_BLOCKS: usize = 192;
const SIMULATION_SETTLE_PASSES: usize = 4;
const TOP_RECOVERY_TIMING_NS: u64 = 100_000;
const TOP_RECOVERY_MAX_OBSERVATIONS: usize = 30_000;
const BOTTOM_DEGRADE_TIMING_NS: u64 = 10_000_000;
const BOTTOM_SETTLE_TIMING_NS: u64 = 100_000;
const BOTTOM_DEGRADE_MAX_CYCLES: usize = 64;
const STOCHASTIC_REPEAT_COUNT: usize = 5;

const FULL_REFLECTION_RAYS: i32 = 4_096;
const FULL_REFLECTION_BOUNCES: i32 = 3;
const FULL_REFLECTION_DURATION_S: f32 = 1.5;
const FULL_REFLECTION_ORDER: i32 = 1;

// Mirrors the Desktop Reduced divisor row in governor.rs:1167. Cadence is
// reported as the rung would deliver it, but is not emulated: this diagnostic
// explicitly runs every simulation pass and settles every position, so cadence
// changes staleness rather than steady-state level.
const REDUCED_REFLECTION_RAYS: i32 = 2_048;
const REDUCED_REFLECTION_BOUNCES: i32 = 2;
const REDUCED_REFLECTION_DURATION_S: f32 = 0.75;
const REDUCED_REFLECTION_ORDER: i32 = 1;
const REDUCED_REFLECTION_CADENCE_DIVISOR: u8 = 2;

const MUTED: StageOutputGains = StageOutputGains {
    direct: 0.0,
    pathing: 0.0,
    reflections: 0.0,
};
const DIRECT_ONLY: StageOutputGains = StageOutputGains {
    direct: 1.0,
    pathing: 0.0,
    reflections: 0.0,
};
const PATH_ONLY: StageOutputGains = StageOutputGains {
    direct: 0.0,
    pathing: 1.0,
    reflections: 0.0,
};
const REFLECTIONS_ONLY: StageOutputGains = StageOutputGains {
    direct: 0.0,
    pathing: 0.0,
    reflections: 1.0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SweepMode {
    StartupBottom,
    ReducedDelivered,
    HeadroomTop,
}

impl SweepMode {
    const fn label(self) -> &'static str {
        match self {
            Self::StartupBottom => "startup-bottom",
            Self::ReducedDelivered => "reduced-delivered",
            Self::HeadroomTop => "headroom-top",
        }
    }

    const fn repeat_count(self) -> usize {
        match self {
            Self::StartupBottom => 1,
            Self::ReducedDelivered | Self::HeadroomTop => STOCHASTIC_REPEAT_COUNT,
        }
    }

    const fn target_cadence_divisor(self) -> u8 {
        match self {
            Self::StartupBottom => 4,
            Self::ReducedDelivered => REDUCED_REFLECTION_CADENCE_DIVISOR,
            Self::HeadroomTop => 1,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct StageLevels {
    direct_dbfs: f64,
    path_dbfs: f64,
    reflections_dbfs: f64,
    all_dbfs: f64,
}

#[derive(Clone, Copy, Debug)]
struct CornerObservation {
    mode: SweepMode,
    repeat: usize,
    index: usize,
    listener: ApiEnuVector3,
    shadow_distance_m: f32,
    direct_occlusion: f32,
    smoothed_direct_gain: f32,
    path_sh_energy: f32,
    path_eq: [f32; 3],
    source_has_probe: bool,
    listener_has_probe: bool,
    stages: StageLevels,
    quality: QualityGovernorTelemetry,
}

#[derive(Debug)]
struct SweepResult {
    mode: SweepMode,
    repeat_count: usize,
    recovery_observations: usize,
    rows: Vec<CornerObservation>,
}

#[derive(Clone, Copy, Debug)]
struct M4Statistics {
    los_mean_dbfs: f64,
    shadow_mean_dbfs: f64,
    shadow_mean_below_los_db: f64,
    deepest_mean_dbfs: f64,
    deepest_below_los_db: f64,
    max_adjacent_step_db_per_m: f64,
    max_adjacent_from_index: usize,
    max_adjacent_to_index: usize,
    max_reflections_repeat_spread_db: f64,
    max_reflections_repeat_spread_index: usize,
    max_all_repeat_spread_db: f64,
    max_all_repeat_spread_index: usize,
}

#[derive(Clone, Copy, Debug)]
struct BroadbandNoise {
    state: u64,
}

impl BroadbandNoise {
    const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn fill(&mut self, output: &mut [f32]) {
        for sample in output {
            let mut value = self.state;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.state = value;
            let unit = ((value >> 40) as u32) as f32 / 16_777_215.0;
            *sample = (unit * 2.0 - 1.0) * 0.25;
        }
    }
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock dual bake"]
fn megablock_corner_stage_energy_diagnostic() {
    let package = env_path(
        "FIGHTBOX_DIAG_PACKAGE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox",
    );
    let bake = env_path(
        "FIGHTBOX_DIAG_BAKE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.baked",
    );
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    assert_compiled_occluder(&mesh);
    let baked = load_baked(&bake);
    let tangent_north_m = shadow_tangent_north_m();

    println!(
        "CORNER_GEOMETRY source=[{:.3},{:.3},{:.3}] walk_east_m={WALK_EAST_M:.3} \
         walk_north_m={WALK_START_NORTH_M:.3}..{:.3} step_m={WALK_STEP_M:.3} \
         occluder=synth/1/1/0 south_edge=[{OCCLUDER_WEST_M:.3},{OCCLUDER_SOUTH_M:.3}]..\
         [{OCCLUDER_EAST_M:.3},{OCCLUDER_SOUTH_M:.3}] \
         east_edge=[{OCCLUDER_EAST_M:.3},{OCCLUDER_SOUTH_M:.3}]..\
         [{OCCLUDER_EAST_M:.3},{OCCLUDER_NORTH_M:.3}] height_m={OCCLUDER_HEIGHT_M:.3} \
         tangent_corner=[{OCCLUDER_EAST_M:.3},{OCCLUDER_SOUTH_M:.3}] \
         tangent_walk_north_m={tangent_north_m:.6}",
        SOURCE.east_m,
        SOURCE.north_m,
        SOURCE.up_m,
        WALK_START_NORTH_M + WALK_STEP_M * (WALK_POSITION_COUNT - 1) as f32,
    );
    println!(
        "CORNER_CONFIG sample_rate_hz={SAMPLE_RATE} block_frames={BLOCK_FRAMES} \
         block_period_ms={:.6} governor_p99_budget_ms={:.6} \
         direct_occlusion=Volumetric(radius_m=1.0,sample_count=32) \
         max_occlusion_samples=64 full_reflection_ask=4096/3/1.5s/order1 \
         pathing=order2/validation=true/alternates=true descriptor=Point \
         reference_level=CreativeDb(0.0)-uncalibrated input=deterministic-white-noise-peak-0.25 \
         position_settle_audio_s={:.6} stage_measure_audio_s={:.6}",
        block_period_ns() as f64 / 1.0e6,
        block_period_ns() as f64 / 2.0e6,
        POSITION_SETTLE_BLOCKS as f64 * BLOCK_FRAMES as f64 / SAMPLE_RATE as f64,
        STAGE_MEASURE_BLOCKS as f64 * BLOCK_FRAMES as f64 / SAMPLE_RATE as f64,
    );
    println!(
        "CORNER_REDUCED_CONFIG divisor_source=crates/fightbox-steam-audio/src/governor.rs:1167 \
         full_ask={FULL_REFLECTION_RAYS}/{FULL_REFLECTION_BOUNCES}/\
         {FULL_REFLECTION_DURATION_S:.3}s/order{FULL_REFLECTION_ORDER}/cadence1 \
         reduced_delivered={REDUCED_REFLECTION_RAYS}/{REDUCED_REFLECTION_BOUNCES}/\
         {REDUCED_REFLECTION_DURATION_S:.3}s/order{REDUCED_REFLECTION_ORDER}/\
         cadence{REDUCED_REFLECTION_CADENCE_DIVISOR} \
         emulation=governor-held-top-with-reduced-reflection-request \
         cadence_note=affects-staleness-not-steady-state-level-because-each-position-settles \
         stochastic_repeats={STOCHASTIC_REPEAT_COUNT}"
    );
    println!(
        "CORNER_DIVERGENCE diagnostic_has_one_point_source_not_workbench_four-source-session; \
         requested_point_source_differs_from_fixture_artillery_LineSegment(6m); \
         continuous_deterministic_noise_replaces_workbench_retriggered_artillery_asset; \
         offline_explicit_simulation_passes_replace_live_60/15/5Hz_scheduler; \
         startup-bottom_is_driven_down_by_10ms/0.1ms/0.1ms_timing_cycles; reduced-delivered_and_headroom-top_are_held_after_0.100ms \
         synthetic_callback_observations; reduced-cadence2_is_reported_not_scheduled; \
         no_cpal_device_or_live_callback_jitter"
    );

    let bottom = run_sweep(&mesh, &baked, SweepMode::StartupBottom);
    let top = run_sweep(&mesh, &baked, SweepMode::HeadroomTop);
    let reduced = run_sweep(&mesh, &baked, SweepMode::ReducedDelivered);

    print_table(&[&bottom, &reduced, &top]);
    print_summary(&bottom);
    print_summary(&reduced);
    print_summary(&top);
    print_contrast(&bottom, &top);
    print_repeat_spread_table(&reduced, &top);
    print_envelope_table(&[&bottom, &reduced, &top]);

    assert_eq!(bottom.rows.len(), WALK_POSITION_COUNT);
    assert_eq!(
        reduced.rows.len(),
        WALK_POSITION_COUNT * STOCHASTIC_REPEAT_COUNT
    );
    assert_eq!(
        top.rows.len(),
        WALK_POSITION_COUNT * STOCHASTIC_REPEAT_COUNT
    );
    assert!(bottom.rows.iter().all(|row| {
        row.mode == SweepMode::StartupBottom
            && row.quality.sources[0].quality == SourceQualityLevel::DirectOnly
    }));
    assert!(reduced.rows.iter().all(|row| {
        row.mode == SweepMode::ReducedDelivered
            && row.quality.sources[0].quality == SourceQualityLevel::Full
            && row.quality.reflections.level == ReflectionQualityLevel::Full
            && row.quality.reflections.rays == REDUCED_REFLECTION_RAYS
            && row.quality.reflections.bounces == REDUCED_REFLECTION_BOUNCES
            && (row.quality.reflections.ir_duration_s - REDUCED_REFLECTION_DURATION_S).abs()
                <= f32::EPSILON
            && row.quality.ambisonic_order == REDUCED_REFLECTION_ORDER
    }));
    assert!(top.rows.iter().all(|row| {
        row.mode == SweepMode::HeadroomTop
            && row.quality.sources[0].quality == SourceQualityLevel::Full
            && row.quality.reflections.level == ReflectionQualityLevel::Full
    }));
    assert!(bottom.rows.iter().all(observation_is_valid));
    assert!(reduced.rows.iter().all(observation_is_valid));
    assert!(top.rows.iter().all(observation_is_valid));
}

fn run_sweep(mesh: &SceneMesh, baked: &BakedProbeBatch, mode: SweepMode) -> SweepResult {
    let repeat_count = mode.repeat_count();
    let mut recovery_observations = None;
    let mut rows = Vec::with_capacity(WALK_POSITION_COUNT * repeat_count);
    for repeat in 0..repeat_count {
        println!(
            "CORNER_REPEAT_START run={} repeat={}/{}",
            mode.label(),
            repeat + 1,
            repeat_count
        );
        let (repeat_recovery_observations, mut repeat_rows) =
            run_sweep_repeat(mesh, baked, mode, repeat);
        match recovery_observations {
            Some(expected) => assert_eq!(repeat_recovery_observations, expected),
            None => recovery_observations = Some(repeat_recovery_observations),
        }
        rows.append(&mut repeat_rows);
        println!(
            "CORNER_REPEAT_DONE run={} repeat={}/{}",
            mode.label(),
            repeat + 1,
            repeat_count
        );
    }
    SweepResult {
        mode,
        repeat_count,
        recovery_observations: recovery_observations.unwrap_or(0),
        rows,
    }
}

fn run_sweep_repeat(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    mode: SweepMode,
    repeat: usize,
) -> (usize, Vec<CornerObservation>) {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = workbench_simulation_config(mode);
    let descriptors = [crate::MultiSourceDescriptor::at(SOURCE)];
    let (mut simulation, mut render) =
        build_multi_source_session(mesh, baked, audio, config, &descriptors)
            .expect("build megablock corner diagnostic session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take corner diagnostic stage control");
    let recovery_observations = match mode {
        SweepMode::StartupBottom => force_governor_to_bottom(&mut simulation),
        SweepMode::ReducedDelivered | SweepMode::HeadroomTop => {
            recover_governor_to_top(&mut simulation, config)
        }
    };
    let expected = simulation.quality_governor_telemetry();
    match mode {
        SweepMode::StartupBottom => assert_bottom_quality(expected),
        SweepMode::ReducedDelivered | SweepMode::HeadroomTop => {
            assert_top_quality(expected, config)
        }
    }

    let mut noise = BroadbandNoise::new(0x6a09_e667_f3bc_c909);
    let mut rows = Vec::with_capacity(WALK_POSITION_COUNT);
    for index in 0..WALK_POSITION_COUNT {
        let listener = ApiEnuVector3::new(
            WALK_EAST_M,
            WALK_START_NORTH_M + WALK_STEP_M * index as f32,
            1.5,
        );
        simulation.update_inputs(&one_source_update(SOURCE, listener));
        stage_gains.publish(MUTED);
        for _ in 0..SIMULATION_SETTLE_PASSES {
            simulation.run_direct().expect("corner direct simulation");
            simulation.run_pathing().expect("corner path simulation");
            simulation
                .run_reflections()
                .expect("corner reflection simulation");
            render_noise_block(&mut render, &mut noise, None);
        }
        for _ in SIMULATION_SETTLE_PASSES..POSITION_SETTLE_BLOCKS {
            render_noise_block(&mut render, &mut noise, None);
        }

        let raw = simulation.snapshot.sources[0];
        let smoothed = render.sources[0].propagation_smoother.applied();
        let quality = simulation.quality_governor_telemetry();
        let stages = StageLevels {
            direct_dbfs: measure_stage(&mut render, &mut stage_gains, &mut noise, DIRECT_ONLY),
            path_dbfs: measure_stage(&mut render, &mut stage_gains, &mut noise, PATH_ONLY),
            reflections_dbfs: measure_stage(
                &mut render,
                &mut stage_gains,
                &mut noise,
                REFLECTIONS_ONLY,
            ),
            all_dbfs: measure_stage(
                &mut render,
                &mut stage_gains,
                &mut noise,
                StageOutputGains::UNITY,
            ),
        };
        rows.push(CornerObservation {
            mode,
            repeat,
            index,
            listener,
            shadow_distance_m: listener.north_m - shadow_tangent_north_m(),
            direct_occlusion: raw.direct.occlusion,
            smoothed_direct_gain: predicted_direct_gain(smoothed.direct),
            path_sh_energy: coefficient_energy(raw.path_sh),
            path_eq: raw.path_eq,
            source_has_probe: simulation
                .world
                .has_influencing_probe(simulation.frame.sources[0].position),
            listener_has_probe: simulation
                .world
                .has_influencing_probe(simulation.frame.listener.position),
            stages,
            quality,
        });
    }
    (recovery_observations, rows)
}

fn workbench_simulation_config(mode: SweepMode) -> S3SimulationConfig {
    let mut config = S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 32,
        },
        reflection_rays: FULL_REFLECTION_RAYS,
        diffuse_samples: 32,
        reflection_bounces: FULL_REFLECTION_BOUNCES,
        reflection_duration_s: FULL_REFLECTION_DURATION_S,
        reflection_order: FULL_REFLECTION_ORDER,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        validate_paths: true,
        find_alternate_paths: true,
        ..S3SimulationConfig::default()
    };
    if mode == SweepMode::ReducedDelivered {
        config.reflection_rays = REDUCED_REFLECTION_RAYS;
        config.reflection_bounces = REDUCED_REFLECTION_BOUNCES;
        config.reflection_duration_s = REDUCED_REFLECTION_DURATION_S;
        config.reflection_order = REDUCED_REFLECTION_ORDER;
    }
    config
}

fn recover_governor_to_top(
    simulation: &mut MultiSourceSimulation,
    config: S3SimulationConfig,
) -> usize {
    for observation in 1..=TOP_RECOVERY_MAX_OBSERVATIONS {
        simulation.observe_render_timing(TOP_RECOVERY_TIMING_NS);
        if is_top_quality(simulation.quality_governor_telemetry(), config) {
            return observation;
        }
    }
    panic!(
        "governor did not recover to top: {:?}",
        simulation.quality_governor_telemetry()
    );
}

fn force_governor_to_bottom(simulation: &mut MultiSourceSimulation) -> usize {
    let mut observations = 0;
    for _ in 0..BOTTOM_DEGRADE_MAX_CYCLES {
        if is_bottom_quality(simulation.quality_governor_telemetry()) {
            return observations;
        }
        simulation.observe_render_timing(BOTTOM_DEGRADE_TIMING_NS);
        simulation.observe_render_timing(BOTTOM_SETTLE_TIMING_NS);
        simulation.observe_render_timing(BOTTOM_SETTLE_TIMING_NS);
        observations += 3;
    }
    panic!(
        "governor did not reach the startup-bottom control: {:?}",
        simulation.quality_governor_telemetry()
    );
}

fn is_bottom_quality(quality: QualityGovernorTelemetry) -> bool {
    quality.reflections.level == ReflectionQualityLevel::Minimum
        && quality.reflections.bounces == 0
        && quality.pathing == PathQualityLevel::PrimaryOnly
        && quality.ambisonic_order == 0
        && quality.reverb == ReverbStrategy::ShortIrLowerOrder
        && (quality.reflection_output_gain - 1.0).abs() <= f32::EPSILON
        && quality.sources[..usize::from(quality.source_count)]
            .iter()
            .all(|source| source.quality == SourceQualityLevel::DirectOnly)
}

fn assert_bottom_quality(quality: QualityGovernorTelemetry) {
    assert!(
        is_bottom_quality(quality),
        "expected the complete startup-bottom rung: {quality:?}"
    );
}

fn is_top_quality(quality: QualityGovernorTelemetry, config: S3SimulationConfig) -> bool {
    quality.ladder_position == 0
        && quality.sources[0].quality == SourceQualityLevel::Full
        && quality.reflections.level == ReflectionQualityLevel::Full
        && quality.reflections.rays == config.reflection_rays
        && quality.reflections.bounces == config.reflection_bounces
        && (quality.reflections.ir_duration_s - config.reflection_duration_s).abs() <= f32::EPSILON
        && quality.reflections.cadence_divisor == 1
        && quality.pathing == PathQualityLevel::Full
        && quality.ambisonic_order == config.reflection_order
        && quality.reverb == ReverbStrategy::SdkMixerConvolution
        && (quality.reflection_output_gain - 1.0).abs() <= f32::EPSILON
}

fn assert_top_quality(quality: QualityGovernorTelemetry, config: S3SimulationConfig) {
    assert!(
        is_top_quality(quality, config),
        "expected top quality for {config:?}: {quality:?}"
    );
}

fn measure_stage(
    render: &mut MultiSourceRenderGraph,
    stage_gains: &mut SnapshotWriter<StageOutputGains>,
    noise: &mut BroadbandNoise,
    gains: StageOutputGains,
) -> f64 {
    stage_gains.publish(gains);
    for _ in 0..STAGE_ADOPTION_BLOCKS {
        render_noise_block(render, noise, None);
    }
    let mut squared_sum = 0.0_f64;
    for _ in 0..STAGE_MEASURE_BLOCKS {
        render_noise_block(render, noise, Some(&mut squared_sum));
    }
    let sample_count = (STAGE_MEASURE_BLOCKS * BLOCK_FRAMES as usize * 2) as f64;
    rms_to_dbfs((squared_sum / sample_count).sqrt())
}

fn render_noise_block(
    render: &mut MultiSourceRenderGraph,
    noise: &mut BroadbandNoise,
    squared_sum: Option<&mut f64>,
) {
    let mut input = [0.0_f32; BLOCK_FRAMES as usize];
    let mut left = [0.0_f32; BLOCK_FRAMES as usize];
    let mut right = [0.0_f32; BLOCK_FRAMES as usize];
    noise.fill(&mut input);
    let sources = [BackendSourceBlock {
        source_index: 0,
        input_mono: &input,
    }];
    render
        .render_block(PropagationRenderBlock {
            listener_orientation: ListenerOrientation {
                forward: ApiEnuVector3::new(1.0, 0.0, 0.0),
                up: ApiEnuVector3::new(0.0, 0.0, 1.0),
            },
            sources: &sources,
            output_left: &mut left,
            output_right: &mut right,
        })
        .expect("render corner broadband block");
    if let Some(sum) = squared_sum {
        *sum += left
            .into_iter()
            .chain(right)
            .map(|sample| f64::from(sample) * f64::from(sample))
            .sum::<f64>();
    }
}

fn print_table(sweeps: &[&SweepResult]) {
    println!(
        "CORNER_TABLE run\trepeat\tidx\teast_m\tnorth_m\tshadow_m\tocclusion\t\
         smoothed_direct_gain\tdirect_dbfs\tpath_dbfs\treflections_dbfs\tall_dbfs\t\
         path_sh_energy\tpath_eq\tsource_probe\tlistener_probe\trung\trays\tbounces\t\
         ir_s\tobserved_cadence\ttarget_cadence\treflection_level\tpath_quality\t\
         ambi_order\treverb\tsource_quality\treflection_output_gain"
    );
    for sweep in sweeps {
        for row in &sweep.rows {
            let quality = row.quality;
            println!(
                "CORNER_TABLE {}\t{}\t{}\t{:.3}\t{:.3}\t{:+.3}\t{:.9e}\t{:.9e}\t\
                 {:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.9e}\t{:.6}/{:.6}/{:.6}\t{}\t{}\t\
                 {}\t{}\t{}\t{:.3}\t{}\t{}\t{:?}\t{:?}\t{}\t{:?}\t{:?}\t{:.6}",
                row.mode.label(),
                row.repeat,
                row.index,
                row.listener.east_m,
                row.listener.north_m,
                row.shadow_distance_m,
                row.direct_occlusion,
                row.smoothed_direct_gain,
                row.stages.direct_dbfs,
                row.stages.path_dbfs,
                row.stages.reflections_dbfs,
                row.stages.all_dbfs,
                row.path_sh_energy,
                row.path_eq[0],
                row.path_eq[1],
                row.path_eq[2],
                row.source_has_probe,
                row.listener_has_probe,
                quality.ladder_position,
                quality.reflections.rays,
                quality.reflections.bounces,
                quality.reflections.ir_duration_s,
                quality.reflections.cadence_divisor,
                row.mode.target_cadence_divisor(),
                quality.reflections.level,
                quality.pathing,
                quality.ambisonic_order,
                quality.reverb,
                quality.sources[0].quality,
                quality.reflection_output_gain,
            );
        }
    }
}

fn print_summary(sweep: &SweepResult) {
    let rows = baseline_rows(sweep);
    let direct_death = rows
        .iter()
        .copied()
        .find(|row| row.direct_occlusion == 0.0 || row.stages.direct_dbfs == f64::NEG_INFINITY);
    let path_exact_zero_count = rows.iter().filter(|row| row.path_sh_energy == 0.0).count();
    let los_direct = rows[0].stages.direct_dbfs;
    let deep = rows.last().expect("corner sweep has deep row");
    let path_below_los_direct_db = deep.stages.path_dbfs - los_direct;
    let max_reflections_dbfs = rows
        .iter()
        .map(|row| row.stages.reflections_dbfs)
        .max_by(f64::total_cmp)
        .expect("corner sweep has reflection rows");
    let reflections_contribute = max_reflections_dbfs.is_finite();
    match direct_death {
        Some(row) => {
            let previous = row
                .index
                .checked_sub(1)
                .and_then(|index| rows.get(index).copied())
                .unwrap_or(row);
            let step_m = f64::from(row.shadow_distance_m - previous.shadow_distance_m).max(1.0);
            let measured_slope = (row.stages.direct_dbfs - previous.stages.direct_dbfs) / step_m;
            let floor_limited_slope =
                (floor_db(row.stages.direct_dbfs) - floor_db(previous.stages.direct_dbfs)) / step_m;
            println!(
                "CORNER_SUMMARY run={} recovery_observations={} direct_dies_idx={} \
                 direct_dies_listener=[{:.3},{:.3},{:.3}] direct_dies_shadow_m={:+.3} \
                 direct_dies_occlusion={:.9e} direct_dies_dbfs={:.3} \
                 direct_cliff_idx={}->{} direct_cliff_slope_db_per_m={:.3} \
                 direct_cliff_floor_limited_slope_db_per_m={:.3} floor_dbfs=-200 \
                 path_exact_zero_count={} path_deep_dbfs={:.3} \
                 path_deep_below_los_direct_db={:.3} reflections_contribute={} \
                 reflections_max_dbfs={:.3} reflections_deep_dbfs={:.3}",
                sweep.mode.label(),
                sweep.recovery_observations,
                row.index,
                row.listener.east_m,
                row.listener.north_m,
                row.listener.up_m,
                row.shadow_distance_m,
                row.direct_occlusion,
                row.stages.direct_dbfs,
                previous.index,
                row.index,
                measured_slope,
                floor_limited_slope,
                path_exact_zero_count,
                deep.stages.path_dbfs,
                path_below_los_direct_db,
                reflections_contribute,
                max_reflections_dbfs,
                deep.stages.reflections_dbfs,
            );
        }
        None => println!(
            "CORNER_SUMMARY run={} recovery_observations={} direct_dies_idx=none \
             path_exact_zero_count={} path_deep_dbfs={:.3} \
             path_deep_below_los_direct_db={:.3} reflections_contribute={} \
             reflections_max_dbfs={:.3} reflections_deep_dbfs={:.3}",
            sweep.mode.label(),
            sweep.recovery_observations,
            path_exact_zero_count,
            deep.stages.path_dbfs,
            path_below_los_direct_db,
            reflections_contribute,
            max_reflections_dbfs,
            deep.stages.reflections_dbfs,
        ),
    }
}

fn print_contrast(bottom: &SweepResult, top: &SweepResult) {
    let bottom_deep = baseline_row(bottom, WALK_POSITION_COUNT - 1);
    let top_deep = baseline_row(top, WALK_POSITION_COUNT - 1);
    println!(
        "CORNER_CONTRAST deep_idx={} shadow_m={:+.3} \
         startup_reflections_dbfs={:.3} top_reflections_dbfs={:.3} \
         top_minus_startup_reflections_db={} startup_all_dbfs={:.3} top_all_dbfs={:.3} \
         startup_rung={} startup_source_quality={:?} top_rung={} top_source_quality={:?}",
        bottom_deep.index,
        bottom_deep.shadow_distance_m,
        bottom_deep.stages.reflections_dbfs,
        top_deep.stages.reflections_dbfs,
        db_difference(
            top_deep.stages.reflections_dbfs,
            bottom_deep.stages.reflections_dbfs
        ),
        bottom_deep.stages.all_dbfs,
        top_deep.stages.all_dbfs,
        bottom_deep.quality.ladder_position,
        bottom_deep.quality.sources[0].quality,
        top_deep.quality.ladder_position,
        top_deep.quality.sources[0].quality,
    );
}

fn print_repeat_spread_table(reduced: &SweepResult, top: &SweepResult) {
    assert_eq!(reduced.mode, SweepMode::ReducedDelivered);
    assert_eq!(top.mode, SweepMode::HeadroomTop);
    assert_eq!(reduced.repeat_count, STOCHASTIC_REPEAT_COUNT);
    assert_eq!(top.repeat_count, STOCHASTIC_REPEAT_COUNT);
    println!(
        "CORNER_REPEAT_SPREAD idx\tshadow_m\t\
         reduced_reflections_mean_dbfs\treduced_reflections_max_minus_min_db\t\
         reduced_all_mean_dbfs\treduced_all_max_minus_min_db\t\
         full_reflections_mean_dbfs\tfull_reflections_max_minus_min_db\t\
         full_all_mean_dbfs\tfull_all_max_minus_min_db"
    );
    for index in 0..WALK_POSITION_COUNT {
        println!(
            "CORNER_REPEAT_SPREAD {}\t{:+.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t\
             {:.3}\t{:.3}\t{:.3}\t{:.3}",
            index,
            baseline_row(reduced, index).shadow_distance_m,
            position_stage_mean(reduced, index, |stages| stages.reflections_dbfs),
            position_stage_spread(reduced, index, |stages| stages.reflections_dbfs),
            position_stage_mean(reduced, index, |stages| stages.all_dbfs),
            position_stage_spread(reduced, index, |stages| stages.all_dbfs),
            position_stage_mean(top, index, |stages| stages.reflections_dbfs),
            position_stage_spread(top, index, |stages| stages.reflections_dbfs),
            position_stage_mean(top, index, |stages| stages.all_dbfs),
            position_stage_spread(top, index, |stages| stages.all_dbfs),
        );
    }
}

fn print_envelope_table(sweeps: &[&SweepResult]) {
    println!(
        "CORNER_M4_METHOD los_positions=shadow_m<=0 shadow_positions=shadow_m>0 \
         position_level=arithmetic-mean-of-repeat-dBFS \
         region_level=arithmetic-mean-of-position-dBFS \
         adjacent_step=absolute-position-mean-difference-per-meter"
    );
    println!(
        "CORNER_ENVELOPE run\trepeats\trays\tbounces\tir_s\torder\tobserved_cadence\t\
         target_cadence\tobserved_reflection_level\tsource_quality\tlos_mean_dbfs\t\
         shadow_mean_dbfs\tshadow_mean_below_los_db\tdeepest_mean_dbfs\t\
         deepest_below_los_db\tmax_adjacent_step_db_per_m\tmax_adjacent_pair\t\
         max_reflections_repeat_spread_db\tmax_reflections_spread_idx\t\
         max_all_repeat_spread_db\tmax_all_spread_idx\tshadow_band_2_to_15\t\
         deepest_band_3_to_15\tcontinuity_le_6"
    );
    for sweep in sweeps {
        let stats = m4_statistics(sweep);
        let quality = baseline_row(sweep, 0).quality;
        println!(
            "CORNER_ENVELOPE {}\t{}\t{}\t{}\t{:.3}\t{}\t{}\t{}\t{:?}\t{:?}\t\
             {:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}->{}\t{:.3}\t{}\t\
             {:.3}\t{}\t{}\t{}\t{}",
            sweep.mode.label(),
            sweep.repeat_count,
            quality.reflections.rays,
            quality.reflections.bounces,
            quality.reflections.ir_duration_s,
            quality.ambisonic_order,
            quality.reflections.cadence_divisor,
            sweep.mode.target_cadence_divisor(),
            quality.reflections.level,
            quality.sources[0].quality,
            stats.los_mean_dbfs,
            stats.shadow_mean_dbfs,
            stats.shadow_mean_below_los_db,
            stats.deepest_mean_dbfs,
            stats.deepest_below_los_db,
            stats.max_adjacent_step_db_per_m,
            stats.max_adjacent_from_index,
            stats.max_adjacent_to_index,
            stats.max_reflections_repeat_spread_db,
            stats.max_reflections_repeat_spread_index,
            stats.max_all_repeat_spread_db,
            stats.max_all_repeat_spread_index,
            (2.0..=15.0).contains(&stats.shadow_mean_below_los_db),
            (3.0..=15.0).contains(&stats.deepest_below_los_db),
            stats.max_adjacent_step_db_per_m <= 6.0,
        );
    }
}

fn m4_statistics(sweep: &SweepResult) -> M4Statistics {
    let all_position_means = (0..WALK_POSITION_COUNT)
        .map(|index| position_stage_mean(sweep, index, |stages| stages.all_dbfs))
        .collect::<Vec<_>>();
    let mut los_levels = Vec::new();
    let mut shadow_levels = Vec::new();
    for (index, level) in all_position_means.iter().copied().enumerate() {
        if baseline_row(sweep, index).shadow_distance_m <= 0.0 {
            los_levels.push(level);
        } else {
            shadow_levels.push(level);
        }
    }
    assert!(!los_levels.is_empty());
    assert!(!shadow_levels.is_empty());
    let los_mean_dbfs = mean_db(&los_levels);
    let shadow_mean_dbfs = mean_db(&shadow_levels);
    let deepest_mean_dbfs = *all_position_means.last().expect("corner deepest mean");
    let (max_adjacent_from_index, max_adjacent_step_db_per_m) = all_position_means
        .windows(2)
        .enumerate()
        .map(|(index, levels)| {
            (
                index,
                (levels[1] - levels[0]).abs() / f64::from(WALK_STEP_M),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .expect("corner adjacent levels");
    let (max_reflections_repeat_spread_index, max_reflections_repeat_spread_db) = (0
        ..WALK_POSITION_COUNT)
        .map(|index| {
            (
                index,
                position_stage_spread(sweep, index, |stages| stages.reflections_dbfs),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .expect("corner reflection spreads");
    let (max_all_repeat_spread_index, max_all_repeat_spread_db) = (0..WALK_POSITION_COUNT)
        .map(|index| {
            (
                index,
                position_stage_spread(sweep, index, |stages| stages.all_dbfs),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .expect("corner all-stage spreads");
    M4Statistics {
        los_mean_dbfs,
        shadow_mean_dbfs,
        shadow_mean_below_los_db: los_mean_dbfs - shadow_mean_dbfs,
        deepest_mean_dbfs,
        deepest_below_los_db: los_mean_dbfs - deepest_mean_dbfs,
        max_adjacent_step_db_per_m,
        max_adjacent_from_index,
        max_adjacent_to_index: max_adjacent_from_index + 1,
        max_reflections_repeat_spread_db,
        max_reflections_repeat_spread_index,
        max_all_repeat_spread_db,
        max_all_repeat_spread_index,
    }
}

fn baseline_rows(sweep: &SweepResult) -> Vec<&CornerObservation> {
    let rows = sweep
        .rows
        .iter()
        .filter(|row| row.repeat == 0)
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), WALK_POSITION_COUNT);
    rows
}

fn baseline_row(sweep: &SweepResult, index: usize) -> &CornerObservation {
    sweep
        .rows
        .iter()
        .find(|row| row.repeat == 0 && row.index == index)
        .unwrap_or_else(|| panic!("missing {} baseline position {index}", sweep.mode.label()))
}

fn position_stage_mean(
    sweep: &SweepResult,
    index: usize,
    stage: impl Fn(&StageLevels) -> f64,
) -> f64 {
    let levels = sweep
        .rows
        .iter()
        .filter(|row| row.index == index)
        .map(|row| stage(&row.stages))
        .collect::<Vec<_>>();
    assert_eq!(levels.len(), sweep.repeat_count);
    assert!(levels.iter().all(|level| level.is_finite()));
    mean_db(&levels)
}

fn position_stage_spread(
    sweep: &SweepResult,
    index: usize,
    stage: impl Fn(&StageLevels) -> f64,
) -> f64 {
    let levels = sweep
        .rows
        .iter()
        .filter(|row| row.index == index)
        .map(|row| stage(&row.stages))
        .collect::<Vec<_>>();
    assert_eq!(levels.len(), sweep.repeat_count);
    if levels.windows(2).all(|pair| pair[0] == pair[1]) {
        return 0.0;
    }
    assert!(levels.iter().all(|level| level.is_finite()));
    let minimum = levels
        .iter()
        .copied()
        .min_by(f64::total_cmp)
        .expect("corner stage minimum");
    let maximum = levels
        .iter()
        .copied()
        .max_by(f64::total_cmp)
        .expect("corner stage maximum");
    maximum - minimum
}

fn mean_db(levels: &[f64]) -> f64 {
    levels.iter().sum::<f64>() / levels.len() as f64
}

fn observation_is_valid(row: &CornerObservation) -> bool {
    row.direct_occlusion.is_finite()
        && row.smoothed_direct_gain.is_finite()
        && row.path_sh_energy.is_finite()
        && row.path_eq.into_iter().all(f32::is_finite)
        && [
            row.stages.direct_dbfs,
            row.stages.path_dbfs,
            row.stages.reflections_dbfs,
            row.stages.all_dbfs,
        ]
        .into_iter()
        .all(|level| !level.is_nan() && level <= 6.0)
}

fn shadow_tangent_north_m() -> f32 {
    SOURCE.north_m
        + (OCCLUDER_SOUTH_M - SOURCE.north_m) * (WALK_EAST_M - SOURCE.east_m)
            / (OCCLUDER_EAST_M - SOURCE.east_m)
}

fn block_period_ns() -> u64 {
    BLOCK_FRAMES as u64 * 1_000_000_000 / SAMPLE_RATE as u64
}

fn coefficient_energy<const N: usize>(coefficients: [f32; N]) -> f32 {
    coefficients
        .into_iter()
        .map(|coefficient| coefficient * coefficient)
        .sum()
}

fn rms_to_dbfs(rms: f64) -> f64 {
    if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn floor_db(level: f64) -> f64 {
    if level.is_finite() {
        level.max(-200.0)
    } else {
        -200.0
    }
}

fn db_difference(high: f64, low: f64) -> String {
    if high.is_finite() && low.is_finite() {
        format!("{:.3}", high - low)
    } else if high.is_finite() {
        "infinite(startup-exact-zero)".to_owned()
    } else {
        "not-finite".to_owned()
    }
}

fn one_source_update(
    source_position: ApiEnuVector3,
    listener_position: ApiEnuVector3,
) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
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

fn assert_compiled_occluder(mesh: &SceneMesh) {
    for (east_m, north_m) in [
        (OCCLUDER_WEST_M, OCCLUDER_SOUTH_M),
        (OCCLUDER_EAST_M, OCCLUDER_SOUTH_M),
        (OCCLUDER_EAST_M, OCCLUDER_NORTH_M),
        (OCCLUDER_WEST_M, OCCLUDER_NORTH_M),
    ] {
        assert!(
            mesh.vertices_enu_m.iter().any(|vertex| {
                (vertex.x - east_m).abs() <= 1.0e-4
                    && (vertex.y - north_m).abs() <= 1.0e-4
                    && vertex.z.abs() <= 1.0e-4
            }),
            "compiled mesh is missing ground corner [{east_m},{north_m}]"
        );
        assert!(
            mesh.vertices_enu_m.iter().any(|vertex| {
                (vertex.x - east_m).abs() <= 1.0e-4
                    && (vertex.y - north_m).abs() <= 1.0e-4
                    && (vertex.z - OCCLUDER_HEIGHT_M).abs() <= 1.0e-3
            }),
            "compiled mesh is missing roof corner [{east_m},{north_m},{OCCLUDER_HEIGHT_M}]"
        );
    }
}

pub(super) fn load_megablock_corner_fixture(
    package: &Path,
    bake: &Path,
) -> (SceneMesh, BakedProbeBatch) {
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    assert_compiled_occluder(&mesh);
    let baked = load_baked(bake);
    (mesh, baked)
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
        vertices.push(EnuVector3::new(
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
    let digits = json_field_tail(json, field)
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().unwrap_or_else(|_| panic!("numeric {field}"))
}

fn json_string(json: &str, field: &str) -> String {
    json_field_tail(json, field)
        .trim_start()
        .strip_prefix('"')
        .and_then(|tail| tail.split('"').next())
        .unwrap_or_else(|| panic!("string {field}"))
        .to_owned()
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
