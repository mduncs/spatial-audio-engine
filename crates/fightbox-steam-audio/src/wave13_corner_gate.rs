use super::*;
use crate::{
    DirectOcclusionMode, PathQualityLevel, QualityGovernorTelemetry, ReflectionEffectConfig,
    ReflectionQualityLevel, ReverbStrategy, SceneMesh, SourceQualityLevel, StageOutputGains,
};
use fightbox_api::{EnuVector3 as ApiEnuVector3, ReferenceLevel};
use fightbox_runtime::SnapshotWriter;
use fightbox_runtime::backend::{
    BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES, PropagationRenderBlock,
    SimulationUpdate, SourceMotion,
};
use std::env;
use std::path::{Path, PathBuf};

const DEFAULT_PACKAGE: &str = "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox";
const DEFAULT_BAKE: &str = "/Users/md/fightbox-runs/megablock-seed1/megablock.baked";

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const CORNER_SOURCE_INDEX: usize = 0;
const SOURCE: ApiEnuVector3 = ApiEnuVector3::new(102.5, 102.5, 1.5);
const SOURCE_LEVEL_DB_SPL: f32 = 155.0;

// These are the exact megablock diagnostic coordinates around synth/1/1/0.
const OCCLUDER_EAST_M: f32 = 147.0;
const OCCLUDER_SOUTH_M: f32 = 113.2;
const WALK_EAST_M: f32 = 148.5;
const WALK_START_NORTH_M: f32 = 108.0;
const WALK_POSITION_COUNT: usize = 12;
const WALK_STEP_M: f32 = 1.0;

// Keep the diagnostic's 1.877 s position settle and 0.512 s stage windows.
const POSITION_SETTLE_BLOCKS: usize = 704;
const STAGE_ADOPTION_BLOCKS: usize = 4;
const STAGE_MEASURE_BLOCKS: usize = 192;
const SIMULATION_SETTLE_PASSES: usize = 4;

const FULL_REFLECTION_RAYS: i32 = 4_096;
const FULL_REFLECTION_BOUNCES: i32 = 3;
const FULL_REFLECTION_DURATION_S: f32 = 1.5;
const FULL_REFLECTION_ORDER: i32 = 1;
const WORKBENCH_PATH_VISIBILITY_RANGE_M: f32 = 10.0;

// Reduced measured 4.62 dB; this floor leaves 2.62 dB of measured margin.
const MIN_SHADOW_MEAN_DROP_DB: f64 = 2.0;
const SHADOW_MEAN_REFERENCE_DROP_DB: f64 = 4.62;
const SHADOW_MEAN_REFERENCE_MARGIN_DB: f64 =
    SHADOW_MEAN_REFERENCE_DROP_DB - MIN_SHADOW_MEAN_DROP_DB;
// Reduced measured 10.70 dB at the deepest point; this floor leaves 7.70 dB.
const MIN_DEEPEST_DROP_DB: f64 = 3.0;
const DEEPEST_REFERENCE_DROP_DB: f64 = 10.70;
const DEEPEST_REFERENCE_MARGIN_DB: f64 = DEEPEST_REFERENCE_DROP_DB - MIN_DEEPEST_DROP_DB;
// The 10.70 dB reduced deepest point leaves 4.30 dB before the rejection ceiling.
const MAX_POSITION_DROP_DB: f64 = 15.0;
const MAX_POSITION_REFERENCE_MARGIN_DB: f64 = MAX_POSITION_DROP_DB - DEEPEST_REFERENCE_DROP_DB;
// Reduced measured 3.31 dB/m; this ceiling leaves 2.69 dB/m of repeat margin.
const MAX_ADJACENT_STEP_DB_PER_M: f64 = 6.0;
const ADJACENT_REFERENCE_STEP_DB_PER_M: f64 = 3.31;
const ADJACENT_REFERENCE_MARGIN_DB_PER_M: f64 =
    MAX_ADJACENT_STEP_DB_PER_M - ADJACENT_REFERENCE_STEP_DB_PER_M;
// The re-measurement lane's largest same-position repeat spread was 2.4 dB.
const MAX_DETERMINISM_DELTA_DB: f64 = 2.4;

const FORCE_BOTTOM_DEADLINE_NS: u64 = 10_000_000;
const FORCE_BOTTOM_SETTLE_NS: u64 = 100_000;
const FORCE_BOTTOM_MAX_CYCLES: usize = 64;
const SMOOTHER_BOUND_ULPS: f32 = 16.0;

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
const NO_REFLECTIONS: StageOutputGains = StageOutputGains {
    direct: 1.0,
    pathing: 1.0,
    reflections: 0.0,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunMode {
    Honest,
    HonestRepeat,
    ReflectionsAbsent,
    StartupBottom,
}

impl RunMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Honest => "honest",
            Self::HonestRepeat => "honest-repeat",
            Self::ReflectionsAbsent => "control-a-reflections-absent",
            Self::StartupBottom => "control-b-startup-bottom",
        }
    }

    const fn stage_gains(self, stage: Stage) -> StageOutputGains {
        match (self, stage) {
            (_, Stage::Direct) => DIRECT_ONLY,
            (_, Stage::Path) => PATH_ONLY,
            (Self::ReflectionsAbsent, Stage::Reflections) => MUTED,
            (_, Stage::Reflections) => REFLECTIONS_ONLY,
            (Self::ReflectionsAbsent, Stage::All) => NO_REFLECTIONS,
            (_, Stage::All) => StageOutputGains::UNITY,
        }
    }
}

#[derive(Clone, Copy)]
enum Stage {
    Direct,
    Path,
    Reflections,
    All,
}

#[derive(Clone, Copy, Debug)]
struct StageLevels {
    direct_dbfs: f64,
    path_dbfs: f64,
    reflections_dbfs: f64,
    all_dbfs: f64,
}

#[derive(Clone, Copy, Debug)]
struct CornerRow {
    index: usize,
    listener: ApiEnuVector3,
    shadow_distance_m: f32,
    direct_occlusion: f32,
    raw_path_sh_energy: f32,
    smoothed_path_sh_energy: f32,
    path_eq: [f32; 3],
    source_has_probe: bool,
    listener_has_probe: bool,
    stages: StageLevels,
    quality: QualityGovernorTelemetry,
}

#[derive(Debug)]
struct SweepResult {
    mode: RunMode,
    boot: QualityGovernorTelemetry,
    force_bottom_observations: usize,
    rows: Vec<CornerRow>,
    hysteresis: HysteresisTrace,
}

#[derive(Clone, Copy, Debug)]
struct BandStatistics {
    los_mean_dbfs: f64,
    shadow_mean_dbfs: f64,
    shadow_mean_below_los_db: f64,
    deepest_dbfs: f64,
    deepest_below_los_db: f64,
    max_position_below_los_db: f64,
    max_position_index: usize,
    max_adjacent_step_db_per_m: f64,
    max_adjacent_from_index: usize,
    max_adjacent_to_index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BandChecks {
    shadow_mean: bool,
    deepest: bool,
    every_position: bool,
    adjacent_step: bool,
}

impl BandChecks {
    const fn passes(self) -> bool {
        self.shadow_mean && self.deepest && self.every_position && self.adjacent_step
    }
}

#[derive(Debug)]
struct HysteresisTrace {
    rendered_blocks: usize,
    checked_blocks: usize,
    covered_checked_blocks: usize,
    exact_zero_steps: usize,
    covered_exact_zero_steps: usize,
    max_step_fraction_of_bound: f32,
    max_step_position: usize,
    position_checked_blocks: [usize; WALK_POSITION_COUNT],
    position_covered_blocks: [usize; WALK_POSITION_COUNT],
    position_exact_zero_steps: [usize; WALK_POSITION_COUNT],
    position_max_step_fraction_of_bound: [f32; WALK_POSITION_COUNT],
}

impl Default for HysteresisTrace {
    fn default() -> Self {
        Self {
            rendered_blocks: 0,
            checked_blocks: 0,
            covered_checked_blocks: 0,
            exact_zero_steps: 0,
            covered_exact_zero_steps: 0,
            max_step_fraction_of_bound: 0.0,
            max_step_position: 0,
            position_checked_blocks: [0; WALK_POSITION_COUNT],
            position_covered_blocks: [0; WALK_POSITION_COUNT],
            position_exact_zero_steps: [0; WALK_POSITION_COUNT],
            position_max_step_fraction_of_bound: [0.0; WALK_POSITION_COUNT],
        }
    }
}

impl HysteresisTrace {
    fn observe(
        &mut self,
        position: usize,
        before: [f32; crate::backend_snapshot::MAX_PATH_SH_COEFFS],
        after: [f32; crate::backend_snapshot::MAX_PATH_SH_COEFFS],
        target: [f32; crate::backend_snapshot::MAX_PATH_SH_COEFFS],
        probes_covered: bool,
        retention: f32,
    ) {
        let was_initialized = self.rendered_blocks > 0;
        self.rendered_blocks += 1;
        if !was_initialized {
            return;
        }

        self.checked_blocks += 1;
        self.position_checked_blocks[position] += 1;
        if probes_covered {
            self.covered_checked_blocks += 1;
            self.position_covered_blocks[position] += 1;
        }

        let maximum_fractional_step = 1.0 - retention;
        for coefficient in 0..after.len() {
            let step = (after[coefficient] - before[coefficient]).abs();
            let nominal_bound =
                maximum_fractional_step * (target[coefficient] - before[coefficient]).abs();
            let scale = before[coefficient]
                .abs()
                .max(after[coefficient].abs())
                .max(target[coefficient].abs())
                .max(1.0);
            let numerical_slack = SMOOTHER_BOUND_ULPS * f32::EPSILON * scale;
            let checked_bound = nominal_bound + numerical_slack;
            assert!(
                step <= checked_bound,
                "{} position {position} coefficient {coefficient} stepped {step:.9e}, above the 80 ms smoother bound {nominal_bound:.9e} + slack {numerical_slack:.9e}",
                "wave13 path_sh"
            );

            let fraction = if checked_bound > 0.0 {
                step / checked_bound
            } else {
                0.0
            };
            if fraction > self.max_step_fraction_of_bound {
                self.max_step_fraction_of_bound = fraction;
                self.max_step_position = position;
            }
            self.position_max_step_fraction_of_bound[position] =
                self.position_max_step_fraction_of_bound[position].max(fraction);

            if before[coefficient] != 0.0 && after[coefficient] == 0.0 {
                self.exact_zero_steps += 1;
                self.position_exact_zero_steps[position] += 1;
                if probes_covered {
                    self.covered_exact_zero_steps += 1;
                }
            }
        }
    }

    fn assert_every_position_was_bounded(&self, mode: RunMode) {
        assert_eq!(
            self.rendered_blocks,
            WALK_POSITION_COUNT * blocks_per_position()
        );
        for (index, checked) in self.position_checked_blocks.iter().copied().enumerate() {
            assert!(
                checked > 0,
                "{} position {index} had no path_sh transition observations",
                mode.label()
            );
            assert!(
                self.position_max_step_fraction_of_bound[index] <= 1.0,
                "{} position {index} exceeded the path_sh smoother bound",
                mode.label()
            );
        }
    }
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
fn wave13_corner_envelope_gate() {
    let package = env_path("FIGHTBOX_DIAG_PACKAGE", DEFAULT_PACKAGE);
    let bake = env_path("FIGHTBOX_DIAG_BAKE", DEFAULT_BAKE);
    let (mesh, baked) =
        super::megablock_corner_diagnostic::load_megablock_corner_fixture(&package, &bake);

    print_gate_configuration(&package, &bake);

    let honest = run_walk(&mesh, &baked, RunMode::Honest);
    let honest_repeat = run_walk(&mesh, &baked, RunMode::HonestRepeat);
    let reflections_absent = run_walk(&mesh, &baked, RunMode::ReflectionsAbsent);
    let startup_bottom = run_walk(&mesh, &baked, RunMode::StartupBottom);

    print_position_table(&[&honest, &reflections_absent, &startup_bottom]);

    let honest_stats = band_statistics(&honest);
    let honest_repeat_stats = band_statistics(&honest_repeat);
    let reflections_absent_stats = band_statistics(&reflections_absent);
    let startup_bottom_stats = band_statistics(&startup_bottom);
    let honest_checks = band_checks(honest_stats);
    let honest_repeat_checks = band_checks(honest_repeat_stats);
    let reflections_absent_checks = band_checks(reflections_absent_stats);
    let startup_bottom_checks = band_checks(startup_bottom_stats);

    print_band_table(&[
        (&honest, honest_stats, honest_checks),
        (&honest_repeat, honest_repeat_stats, honest_repeat_checks),
        (
            &reflections_absent,
            reflections_absent_stats,
            reflections_absent_checks,
        ),
        (&startup_bottom, startup_bottom_stats, startup_bottom_checks),
    ]);
    print_hysteresis_table(&honest);

    assert!(
        honest_checks.passes(),
        "honest corner walk missed the M4 envelope: {honest_stats:?} {honest_checks:?}"
    );
    assert!(
        !reflections_absent_checks.passes(),
        "CONTROL A unexpectedly passed the same M4 predicate: {reflections_absent_stats:?}"
    );
    assert!(
        !startup_bottom_checks.passes(),
        "CONTROL B unexpectedly passed the same M4 predicate: {startup_bottom_stats:?}"
    );

    assert_deterministic(
        &honest,
        honest_stats,
        honest_checks,
        &honest_repeat,
        honest_repeat_stats,
        honest_repeat_checks,
    );
}

fn run_walk(mesh: &SceneMesh, baked: &BakedProbeBatch, mode: RunMode) -> SweepResult {
    println!("WAVE13_RUN_START run={}", mode.label());
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = workbench_simulation_config();
    let descriptors = workbench_descriptors();
    let (mut simulation, mut render) =
        build_multi_source_session(mesh, baked, audio, config, &descriptors)
            .expect("build Wave 13 megablock corner session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take Wave 13 stage output control");
    let boot = simulation.quality_governor_telemetry();
    assert_honest_boot(boot);

    let force_bottom_observations = if mode == RunMode::StartupBottom {
        force_governor_to_bottom(&mut simulation)
    } else {
        0
    };
    if mode == RunMode::StartupBottom {
        assert_bottom_quality(simulation.quality_governor_telemetry());
    }

    let mut noise = BroadbandNoise::new(0x6a09_e667_f3bc_c909);
    let mut hysteresis = HysteresisTrace::default();
    let mut rows = Vec::with_capacity(WALK_POSITION_COUNT);

    for index in 0..WALK_POSITION_COUNT {
        let listener = listener_position(index);
        simulation.update_inputs(&one_source_update(listener));
        stage_gains.publish(MUTED);

        for _ in 0..SIMULATION_SETTLE_PASSES {
            simulation.run_direct().expect("Wave 13 direct simulation");
            simulation.run_pathing().expect("Wave 13 path simulation");
            simulation
                .run_reflections()
                .expect("Wave 13 reflection simulation");
            render_checked_noise_block(
                &simulation,
                &mut render,
                &mut noise,
                &mut hysteresis,
                index,
            );
        }
        for _ in SIMULATION_SETTLE_PASSES..POSITION_SETTLE_BLOCKS {
            render_checked_noise_block(
                &simulation,
                &mut render,
                &mut noise,
                &mut hysteresis,
                index,
            );
        }

        let raw = simulation.snapshot.sources[CORNER_SOURCE_INDEX];
        let smoothed = render.sources[CORNER_SOURCE_INDEX]
            .propagation_smoother
            .applied();
        let source_has_probe = simulation
            .world
            .has_influencing_probe(simulation.frame.sources[CORNER_SOURCE_INDEX].position);
        let listener_has_probe = simulation
            .world
            .has_influencing_probe(simulation.frame.listener.position);
        let stages = StageLevels {
            direct_dbfs: measure_stage(
                &simulation,
                &mut render,
                &mut stage_gains,
                &mut noise,
                &mut hysteresis,
                index,
                mode.stage_gains(Stage::Direct),
            ),
            path_dbfs: measure_stage(
                &simulation,
                &mut render,
                &mut stage_gains,
                &mut noise,
                &mut hysteresis,
                index,
                mode.stage_gains(Stage::Path),
            ),
            reflections_dbfs: measure_stage(
                &simulation,
                &mut render,
                &mut stage_gains,
                &mut noise,
                &mut hysteresis,
                index,
                mode.stage_gains(Stage::Reflections),
            ),
            all_dbfs: measure_stage(
                &simulation,
                &mut render,
                &mut stage_gains,
                &mut noise,
                &mut hysteresis,
                index,
                mode.stage_gains(Stage::All),
            ),
        };
        let row = CornerRow {
            index,
            listener,
            shadow_distance_m: listener.north_m - shadow_tangent_north_m(),
            direct_occlusion: raw.direct.occlusion,
            raw_path_sh_energy: coefficient_energy(raw.path_sh),
            smoothed_path_sh_energy: coefficient_energy(smoothed.path_sh),
            path_eq: raw.path_eq,
            source_has_probe,
            listener_has_probe,
            stages,
            quality: simulation.quality_governor_telemetry(),
        };
        assert_row_is_valid(mode, &row);
        rows.push(row);
    }

    hysteresis.assert_every_position_was_bounded(mode);
    if mode != RunMode::StartupBottom {
        for row in &rows {
            assert_honest_delivered_quality(row.quality);
        }
    }
    println!("WAVE13_RUN_DONE run={}", mode.label());
    SweepResult {
        mode,
        boot,
        force_bottom_observations,
        rows,
        hysteresis,
    }
}

fn workbench_simulation_config() -> S3SimulationConfig {
    S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 64,
        },
        reflection_rays: FULL_REFLECTION_RAYS,
        diffuse_samples: 32,
        reflection_bounces: FULL_REFLECTION_BOUNCES,
        reflection_duration_s: FULL_REFLECTION_DURATION_S,
        reflection_order: FULL_REFLECTION_ORDER,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        pathing_visibility_range_m: WORKBENCH_PATH_VISIBILITY_RANGE_M,
        validate_paths: true,
        find_alternate_paths: true,
        ..S3SimulationConfig::default()
    }
}

fn workbench_descriptors() -> [crate::MultiSourceDescriptor; 1] {
    [
        crate::MultiSourceDescriptor::at(SOURCE).with_reference_level(
            ReferenceLevel::SplAtOneMeter {
                db_spl: SOURCE_LEVEL_DB_SPL,
            },
        ),
    ]
}

fn assert_honest_boot(quality: QualityGovernorTelemetry) {
    assert_eq!(quality.source_count, 1);
    assert!(
        is_reduced_or_better(quality.boot_reflection_level),
        "boot reflection rung was below Reduced: {quality:?}"
    );
    assert!(
        is_reduced_or_better(quality.reflections.level),
        "delivered boot reflection rung was below Reduced: {quality:?}"
    );
    assert!(quality.boot_predicted_cost_ns <= quality.boot_cost_limit_ns);
    let sources = &quality.sources[..usize::from(quality.source_count)];
    assert!(sources.iter().all(|source| source.physically_calibrated));
    assert!(
        sources
            .iter()
            .all(|source| source.quality != SourceQualityLevel::DirectOnly),
        "a calibrated source booted DirectOnly: {sources:?}"
    );
}

fn assert_honest_delivered_quality(quality: QualityGovernorTelemetry) {
    assert!(is_reduced_or_better(quality.boot_reflection_level));
    assert!(is_reduced_or_better(quality.reflections.level));
    assert!(
        quality.sources[..usize::from(quality.source_count)]
            .iter()
            .all(|source| source.quality != SourceQualityLevel::DirectOnly)
    );
}

const fn is_reduced_or_better(level: ReflectionQualityLevel) -> bool {
    matches!(
        level,
        ReflectionQualityLevel::Full | ReflectionQualityLevel::Reduced
    )
}

fn force_governor_to_bottom(simulation: &mut MultiSourceSimulation) -> usize {
    let mut observations = 0;
    for _ in 0..FORCE_BOTTOM_MAX_CYCLES {
        if is_bottom_quality(simulation.quality_governor_telemetry()) {
            return observations;
        }
        simulation.observe_render_timing(FORCE_BOTTOM_DEADLINE_NS);
        simulation.observe_render_timing(FORCE_BOTTOM_SETTLE_NS);
        simulation.observe_render_timing(FORCE_BOTTOM_SETTLE_NS);
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

fn measure_stage(
    simulation: &MultiSourceSimulation,
    render: &mut MultiSourceRenderGraph,
    stage_gains: &mut SnapshotWriter<StageOutputGains>,
    noise: &mut BroadbandNoise,
    hysteresis: &mut HysteresisTrace,
    position: usize,
    gains: StageOutputGains,
) -> f64 {
    stage_gains.publish(gains);
    for _ in 0..STAGE_ADOPTION_BLOCKS {
        render_checked_noise_block(simulation, render, noise, hysteresis, position);
    }
    let mut squared_sum = 0.0_f64;
    for _ in 0..STAGE_MEASURE_BLOCKS {
        squared_sum += render_checked_noise_block(simulation, render, noise, hysteresis, position);
    }
    let sample_count = (STAGE_MEASURE_BLOCKS * BLOCK_FRAMES as usize * 2) as f64;
    rms_to_dbfs((squared_sum / sample_count).sqrt())
}

fn render_checked_noise_block(
    simulation: &MultiSourceSimulation,
    render: &mut MultiSourceRenderGraph,
    noise: &mut BroadbandNoise,
    hysteresis: &mut HysteresisTrace,
    position: usize,
) -> f64 {
    let before = render.sources[CORNER_SOURCE_INDEX]
        .propagation_smoother
        .applied()
        .path_sh;
    let target = simulation.snapshot.sources[CORNER_SOURCE_INDEX].path_sh;
    let probes_covered = simulation
        .world
        .has_influencing_probe(simulation.frame.listener.position)
        && simulation
            .world
            .has_influencing_probe(simulation.frame.sources[CORNER_SOURCE_INDEX].position);

    let mut input = [0.0_f32; BLOCK_FRAMES as usize];
    let mut left = [0.0_f32; BLOCK_FRAMES as usize];
    let mut right = [0.0_f32; BLOCK_FRAMES as usize];
    noise.fill(&mut input);
    let sources = [BackendSourceBlock {
        source_index: CORNER_SOURCE_INDEX,
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
        .expect("render Wave 13 continuous broadband block");

    let after = render.sources[CORNER_SOURCE_INDEX]
        .propagation_smoother
        .applied()
        .path_sh;
    hysteresis.observe(
        position,
        before,
        after,
        target,
        probes_covered,
        render.propagation_block_retention,
    );

    left.into_iter()
        .chain(right)
        .map(|sample| f64::from(sample) * f64::from(sample))
        .sum()
}

fn band_statistics(sweep: &SweepResult) -> BandStatistics {
    assert_eq!(sweep.rows.len(), WALK_POSITION_COUNT);
    let levels = sweep
        .rows
        .iter()
        .map(|row| row.stages.all_dbfs)
        .collect::<Vec<_>>();
    let los = sweep
        .rows
        .iter()
        .filter(|row| row.shadow_distance_m <= 0.0)
        .map(|row| row.stages.all_dbfs)
        .collect::<Vec<_>>();
    let shadow = sweep
        .rows
        .iter()
        .filter(|row| row.shadow_distance_m > 0.0)
        .map(|row| row.stages.all_dbfs)
        .collect::<Vec<_>>();
    assert!(!los.is_empty() && !shadow.is_empty());

    let los_mean_dbfs = mean_db(&los);
    let shadow_mean_dbfs = mean_db(&shadow);
    let deepest_dbfs = *levels.last().expect("deepest Wave 13 position");
    let (max_position_index, max_position_below_los_db) = levels
        .iter()
        .copied()
        .enumerate()
        .map(|(index, level)| (index, below_los_db(los_mean_dbfs, level)))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .expect("Wave 13 position drops");
    let (max_adjacent_from_index, max_adjacent_step_db_per_m) = levels
        .windows(2)
        .enumerate()
        .map(|(index, pair)| {
            (
                index,
                db_distance(pair[0], pair[1]) / f64::from(WALK_STEP_M),
            )
        })
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .expect("Wave 13 adjacent steps");

    BandStatistics {
        los_mean_dbfs,
        shadow_mean_dbfs,
        shadow_mean_below_los_db: below_los_db(los_mean_dbfs, shadow_mean_dbfs),
        deepest_dbfs,
        deepest_below_los_db: below_los_db(los_mean_dbfs, deepest_dbfs),
        max_position_below_los_db,
        max_position_index,
        max_adjacent_step_db_per_m,
        max_adjacent_from_index,
        max_adjacent_to_index: max_adjacent_from_index + 1,
    }
}

fn band_checks(statistics: BandStatistics) -> BandChecks {
    BandChecks {
        shadow_mean: statistics.shadow_mean_below_los_db >= MIN_SHADOW_MEAN_DROP_DB,
        deepest: statistics.deepest_below_los_db >= MIN_DEEPEST_DROP_DB,
        every_position: statistics.max_position_below_los_db <= MAX_POSITION_DROP_DB,
        adjacent_step: statistics.max_adjacent_step_db_per_m <= MAX_ADJACENT_STEP_DB_PER_M,
    }
}

fn assert_deterministic(
    first: &SweepResult,
    first_stats: BandStatistics,
    first_checks: BandChecks,
    repeat: &SweepResult,
    repeat_stats: BandStatistics,
    repeat_checks: BandChecks,
) {
    println!("WAVE13_DETERMINISM idx\tnorth_m\tfirst_all_dbfs\trepeat_all_dbfs\tabs_delta_db");
    let mut maximum_delta = 0.0_f64;
    let mut maximum_index = 0;
    for (first_row, repeat_row) in first.rows.iter().zip(&repeat.rows) {
        assert_eq!(first_row.index, repeat_row.index);
        let delta = db_distance(first_row.stages.all_dbfs, repeat_row.stages.all_dbfs);
        println!(
            "WAVE13_DETERMINISM {}\t{:.3}\t{:.3}\t{:.3}\t{:.3}",
            first_row.index,
            first_row.listener.north_m,
            first_row.stages.all_dbfs,
            repeat_row.stages.all_dbfs,
            delta,
        );
        if delta > maximum_delta {
            maximum_delta = delta;
            maximum_index = first_row.index;
        }
        assert!(
            delta <= MAX_DETERMINISM_DELTA_DB,
            "honest repeat position {} differed by {delta:.3} dB (limit {MAX_DETERMINISM_DELTA_DB:.3} dB)",
            first_row.index
        );
    }

    let statistic_deltas = [
        (
            "los_mean",
            db_distance(first_stats.los_mean_dbfs, repeat_stats.los_mean_dbfs),
        ),
        (
            "shadow_mean",
            db_distance(first_stats.shadow_mean_dbfs, repeat_stats.shadow_mean_dbfs),
        ),
        (
            "shadow_drop",
            db_distance(
                first_stats.shadow_mean_below_los_db,
                repeat_stats.shadow_mean_below_los_db,
            ),
        ),
        (
            "deepest",
            db_distance(first_stats.deepest_dbfs, repeat_stats.deepest_dbfs),
        ),
        (
            "deepest_drop",
            db_distance(
                first_stats.deepest_below_los_db,
                repeat_stats.deepest_below_los_db,
            ),
        ),
        (
            "max_position_drop",
            db_distance(
                first_stats.max_position_below_los_db,
                repeat_stats.max_position_below_los_db,
            ),
        ),
        (
            "max_adjacent_step",
            db_distance(
                first_stats.max_adjacent_step_db_per_m,
                repeat_stats.max_adjacent_step_db_per_m,
            ),
        ),
    ];
    for (name, delta) in statistic_deltas {
        println!("WAVE13_DETERMINISM_STAT {name} abs_delta_db={delta:.3}");
        assert!(
            delta <= MAX_DETERMINISM_DELTA_DB,
            "honest repeat statistic {name} differed by {delta:.3} dB"
        );
    }
    assert_eq!(first_checks, repeat_checks, "honest BAND booleans changed");
    println!(
        "WAVE13_DETERMINISM_SUMMARY max_position_delta_db={maximum_delta:.3} max_position_idx={maximum_index} limit_db={MAX_DETERMINISM_DELTA_DB:.3} band_booleans_identical=true"
    );
}

fn print_gate_configuration(package: &Path, bake: &Path) {
    println!(
        "WAVE13_CONFIG package={} bake={} source=[{:.3},{:.3},{:.3}] calibrated_db_spl={:.1} descriptor=Point configured_sources=1 sample_rate_hz={SAMPLE_RATE} block_frames={BLOCK_FRAMES} walk_east_m={WALK_EAST_M:.3} walk_north_m={WALK_START_NORTH_M:.3}..{:.3} positions={WALK_POSITION_COUNT} step_m={WALK_STEP_M:.3} volumetric_radius_m=1.0 volumetric_samples=64 reflections={FULL_REFLECTION_RAYS}/{FULL_REFLECTION_BOUNCES}/{FULL_REFLECTION_DURATION_S:.3}s/order{FULL_REFLECTION_ORDER} pathing=order2/validation/alternates/visibility{WORKBENCH_PATH_VISIBILITY_RANGE_M:.1}m input=continuous-deterministic-white-noise-peak-0.25",
        package.display(),
        bake.display(),
        SOURCE.east_m,
        SOURCE.north_m,
        SOURCE.up_m,
        SOURCE_LEVEL_DB_SPL,
        WALK_START_NORTH_M + WALK_STEP_M * (WALK_POSITION_COUNT - 1) as f32,
    );
    println!(
        "WAVE13_THRESHOLDS shadow_mean_min_db={MIN_SHADOW_MEAN_DROP_DB:.2} reference_db={SHADOW_MEAN_REFERENCE_DROP_DB:.2} margin_db={SHADOW_MEAN_REFERENCE_MARGIN_DB:.2} deepest_min_db={MIN_DEEPEST_DROP_DB:.2} reference_db={DEEPEST_REFERENCE_DROP_DB:.2} margin_db={DEEPEST_REFERENCE_MARGIN_DB:.2} max_position_drop_db={MAX_POSITION_DROP_DB:.2} reference_margin_db={MAX_POSITION_REFERENCE_MARGIN_DB:.2} max_adjacent_db_per_m={MAX_ADJACENT_STEP_DB_PER_M:.2} reference_db_per_m={ADJACENT_REFERENCE_STEP_DB_PER_M:.2} margin_db_per_m={ADJACENT_REFERENCE_MARGIN_DB_PER_M:.2} determinism_limit_db={MAX_DETERMINISM_DELTA_DB:.2}"
    );
    println!(
        "WAVE13_DIVERGENCE one_continuous_point_source_replaces_the_workbench_four-source_session; corner_artillery_is_Point_not_workbench_LineSegment(6m); deterministic_noise_replaces_the_retriggered_asset_and_is_not_Spl-calibrated_at_the_output; explicit_back-to-back_simulation_passes_replace_live_60/15/5Hz_wall-clock_scheduling; debug_offline_render_timings_are_not_fed_back_to_the_governor_so_the_honest_run_remains_at_its_W1_boot_rung; no_cpal_device_callback_or_live_jitter"
    );
}

fn print_position_table(sweeps: &[&SweepResult]) {
    println!(
        "WAVE13_TABLE run\tidx\teast_m\tnorth_m\tshadow_m\tocclusion\tdirect_dbfs\tpath_dbfs\treflections_dbfs\tall_dbfs\traw_path_sh_energy\tsmoothed_path_sh_energy\tpath_eq\tsource_probe\tlistener_probe\trung\tboot_reflection\tdelivered_reflection\trays\tbounces\tir_s\tcadence\tpath_quality\tcorner_quality\tall_source_quality"
    );
    for sweep in sweeps {
        for row in &sweep.rows {
            let quality = row.quality;
            let source_qualities = quality.sources[..usize::from(quality.source_count)]
                .iter()
                .map(|source| source.quality)
                .collect::<Vec<_>>();
            println!(
                "WAVE13_TABLE {}\t{}\t{:.3}\t{:.3}\t{:+.3}\t{:.9e}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.9e}\t{:.9e}\t{:.6}/{:.6}/{:.6}\t{}\t{}\t{}\t{:?}\t{:?}\t{}\t{}\t{:.3}\t{}\t{:?}\t{:?}\t{:?}",
                sweep.mode.label(),
                row.index,
                row.listener.east_m,
                row.listener.north_m,
                row.shadow_distance_m,
                row.direct_occlusion,
                row.stages.direct_dbfs,
                row.stages.path_dbfs,
                row.stages.reflections_dbfs,
                row.stages.all_dbfs,
                row.raw_path_sh_energy,
                row.smoothed_path_sh_energy,
                row.path_eq[0],
                row.path_eq[1],
                row.path_eq[2],
                row.source_has_probe,
                row.listener_has_probe,
                quality.ladder_position,
                quality.boot_reflection_level,
                quality.reflections.level,
                quality.reflections.rays,
                quality.reflections.bounces,
                quality.reflections.ir_duration_s,
                quality.reflections.cadence_divisor,
                quality.pathing,
                quality.sources[CORNER_SOURCE_INDEX].quality,
                source_qualities,
            );
        }
    }
}

fn print_band_table(entries: &[(&SweepResult, BandStatistics, BandChecks)]) {
    println!(
        "WAVE13_BAND run\tlos_mean_dbfs\tshadow_mean_dbfs\tshadow_drop_db\tshadow_margin_db\tdeepest_dbfs\tdeepest_drop_db\tdeepest_margin_db\tmax_position_drop_db\tmax_position_headroom_db\tmax_position_idx\tmax_step_db_per_m\tstep_headroom_db_per_m\tmax_step_pair\tshadow_ok\tdeepest_ok\tevery_position_ok\tstep_ok\tband_passes\tboot_reflection\tdelivered_reflection\tcorner_quality\tforce_bottom_observations"
    );
    for (sweep, statistics, checks) in entries {
        let quality = sweep.rows[0].quality;
        println!(
            "WAVE13_BAND {}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{:.3}\t{}\t{:.3}\t{:.3}\t{}->{}\t{}\t{}\t{}\t{}\t{}\t{:?}\t{:?}\t{:?}\t{}",
            sweep.mode.label(),
            statistics.los_mean_dbfs,
            statistics.shadow_mean_dbfs,
            statistics.shadow_mean_below_los_db,
            statistics.shadow_mean_below_los_db - MIN_SHADOW_MEAN_DROP_DB,
            statistics.deepest_dbfs,
            statistics.deepest_below_los_db,
            statistics.deepest_below_los_db - MIN_DEEPEST_DROP_DB,
            statistics.max_position_below_los_db,
            MAX_POSITION_DROP_DB - statistics.max_position_below_los_db,
            statistics.max_position_index,
            statistics.max_adjacent_step_db_per_m,
            MAX_ADJACENT_STEP_DB_PER_M - statistics.max_adjacent_step_db_per_m,
            statistics.max_adjacent_from_index,
            statistics.max_adjacent_to_index,
            checks.shadow_mean,
            checks.deepest,
            checks.every_position,
            checks.adjacent_step,
            checks.passes(),
            sweep.boot.boot_reflection_level,
            quality.reflections.level,
            quality.sources[CORNER_SOURCE_INDEX].quality,
            sweep.force_bottom_observations,
        );
    }
}

fn print_hysteresis_table(sweep: &SweepResult) {
    let trace = &sweep.hysteresis;
    println!(
        "WAVE13_HYSTERESIS run={} smoother_ms={:.1} block_retention={:.9} max_fractional_step={:.9} rendered_blocks={} checked_blocks={} covered_checked_blocks={} exact_zero_steps={} covered_exact_zero_steps={} max_step_fraction_of_bound={:.9} max_step_position={} bounded=true",
        sweep.mode.label(),
        crate::motion_smoothing::PROPAGATION_SLEW_TIME_SECONDS * 1_000.0,
        block_retention(),
        1.0 - block_retention(),
        trace.rendered_blocks,
        trace.checked_blocks,
        trace.covered_checked_blocks,
        trace.exact_zero_steps,
        trace.covered_exact_zero_steps,
        trace.max_step_fraction_of_bound,
        trace.max_step_position,
    );
    println!(
        "WAVE13_HYSTERESIS_POSITION idx\tchecked_blocks\tcovered_blocks\texact_zero_steps\tmax_step_fraction_of_bound"
    );
    for index in 0..WALK_POSITION_COUNT {
        println!(
            "WAVE13_HYSTERESIS_POSITION {}\t{}\t{}\t{}\t{:.9}",
            index,
            trace.position_checked_blocks[index],
            trace.position_covered_blocks[index],
            trace.position_exact_zero_steps[index],
            trace.position_max_step_fraction_of_bound[index],
        );
    }
}

fn assert_row_is_valid(mode: RunMode, row: &CornerRow) {
    assert!(row.direct_occlusion.is_finite());
    assert!(row.raw_path_sh_energy.is_finite());
    assert!(row.smoothed_path_sh_energy.is_finite());
    assert!(row.path_eq.into_iter().all(f32::is_finite));
    for level in [
        row.stages.direct_dbfs,
        row.stages.path_dbfs,
        row.stages.reflections_dbfs,
        row.stages.all_dbfs,
    ] {
        assert!(!level.is_nan() && level <= 6.0, "{} {row:?}", mode.label());
    }
}

fn one_source_update(listener_position: ApiEnuVector3) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[CORNER_SOURCE_INDEX] = SourceMotion {
        active: true,
        pose: default_api_pose(SOURCE),
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

fn listener_position(index: usize) -> ApiEnuVector3 {
    ApiEnuVector3::new(
        WALK_EAST_M,
        WALK_START_NORTH_M + WALK_STEP_M * index as f32,
        1.5,
    )
}

fn shadow_tangent_north_m() -> f32 {
    SOURCE.north_m
        + (OCCLUDER_SOUTH_M - SOURCE.north_m) * (WALK_EAST_M - SOURCE.east_m)
            / (OCCLUDER_EAST_M - SOURCE.east_m)
}

const fn blocks_per_position() -> usize {
    POSITION_SETTLE_BLOCKS + 4 * (STAGE_ADOPTION_BLOCKS + STAGE_MEASURE_BLOCKS)
}

fn block_retention() -> f32 {
    (-(BLOCK_FRAMES as f32 / SAMPLE_RATE as f32)
        / crate::motion_smoothing::PROPAGATION_SLEW_TIME_SECONDS)
        .exp()
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

fn mean_db(levels: &[f64]) -> f64 {
    assert!(levels.iter().all(|level| !level.is_nan()));
    if levels.iter().any(|level| *level == f64::NEG_INFINITY) {
        f64::NEG_INFINITY
    } else {
        levels.iter().sum::<f64>() / levels.len() as f64
    }
}

fn below_los_db(los_mean_dbfs: f64, level_dbfs: f64) -> f64 {
    if los_mean_dbfs == level_dbfs {
        0.0
    } else if los_mean_dbfs.is_finite() && level_dbfs.is_finite() {
        los_mean_dbfs - level_dbfs
    } else {
        f64::INFINITY
    }
}

fn db_distance(left: f64, right: f64) -> f64 {
    if left == right {
        0.0
    } else if left.is_finite() && right.is_finite() {
        (left - right).abs()
    } else {
        f64::INFINITY
    }
}

fn env_path(variable: &str, default: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
