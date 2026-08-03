//! Ignored, debug-only Wave 14 measurement of the megablock ballistic tail.
//!
//! This diagnostic keeps the production graph untouched. It drives the Wave 12
//! ballistic pair through the retained four-slot transient session, isolates the
//! reflections stage, and reports decay, tick cost, discrete-return candidates,
//! and the crack-send null. Run it sequentially with the acquired SDK and the
//! real megablock package/bake.

use super::*;
use crate::{
    BakedProbeBatch, BallisticEventLevels, BallisticShot, DirectOcclusionMode,
    QualityGovernorTelemetry, ReflectionEffectConfig, ReflectionQualityLevel, SceneMesh,
    SourcePriorityClass, SourceQualityLevel, StageOutputGains, plan_ballistic_shot,
    synthesize_crack_stem,
};
use fightbox_api::{EnuVector3 as ApiEnuVector3, ImpulseClass, ReferenceLevel};
use fightbox_runtime::SnapshotWriter;
use fightbox_runtime::backend::{
    BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES, PropagationRenderBlock,
    SimulationUpdate, SourceMotion,
};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const DEFAULT_PACKAGE: &str = "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox";
const DEFAULT_BAKE: &str = "/Users/md/fightbox-runs/megablock-seed1/megablock.baked";

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const TIMING_REPEATS: usize = 7;
const SETTLE_BLOCKS: usize = 320;
const CAPTURE_MARGIN_S: f64 = 0.35;
const TAIL_SIGNAL_FLOOR_DBFS: f64 = -140.0;
const FULL_REFLECTION_RAYS: i32 = 4_096;
const FULL_REFLECTION_BOUNCES: i32 = 3;
const FULL_REFLECTION_ORDER: i32 = 1;

// The transient workbench rebuild retains two ordinary sources and adds the
// crack/blast pair. All four participate in reflection simulation cost; only
// the event pair receives PCM in this diagnostic.
const RETAINED_SOURCE_A: usize = 0;
const RETAINED_SOURCE_B: usize = 1;
const CRACK_SOURCE: usize = 2;
const BLAST_SOURCE: usize = 3;
const RETAINED_A_POSITION: ApiEnuVector3 = ApiEnuVector3::new(102.5, 102.5, 1.5);
const RETAINED_B_POSITION: ApiEnuVector3 = ApiEnuVector3::new(245.0, 245.0, 1.5);

const MUZZLE: ApiEnuVector3 = ApiEnuVector3::new(30.0, 288.0, 1.5);
const SHOT_DIRECTION: ApiEnuVector3 = ApiEnuVector3::new(1.0, 0.0, 0.0);
const SHOT_MACH: f64 = 2.5;
const BLAST_SPL_AT_ONE_METER_DB: f64 = 155.0;
const CRACK_OVER_BLAST_DB_AT_REFERENCE: f64 = 3.0;

const REFLECTIONS_ONLY: StageOutputGains = StageOutputGains {
    direct: 0.0,
    pathing: 0.0,
    reflections: 1.0,
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

const LISTENERS: [ListenerCase; 3] = [
    ListenerCase {
        label: "firing-street-los",
        position: ApiEnuVector3::new(197.5, 292.5, 1.5),
        inspect_echoes: true,
    },
    ListenerCase {
        label: "around-one-corner",
        position: ApiEnuVector3::new(292.5, 340.0, 1.5),
        inspect_echoes: true,
    },
    ListenerCase {
        label: "far-firing-street-los",
        position: ApiEnuVector3::new(482.5, 292.5, 1.5),
        inspect_echoes: true,
    },
];

const CONFIGS: [ConfigCase; 4] = [
    ConfigCase {
        label: "honest-reduced-a",
        requested_ir_s: 1.5,
        force_full: false,
        honest_repeat: 1,
    },
    ConfigCase {
        label: "honest-reduced-b",
        requested_ir_s: 1.5,
        force_full: false,
        honest_repeat: 2,
    },
    ConfigCase {
        label: "forced-full-1.5s",
        requested_ir_s: 1.5,
        force_full: true,
        honest_repeat: 0,
    },
    ConfigCase {
        label: "forced-full-2.0s",
        requested_ir_s: 2.0,
        force_full: true,
        honest_repeat: 0,
    },
];

#[derive(Clone, Copy, Debug)]
struct ListenerCase {
    label: &'static str,
    position: ApiEnuVector3,
    inspect_echoes: bool,
}

#[derive(Clone, Copy, Debug)]
struct ConfigCase {
    label: &'static str,
    requested_ir_s: f32,
    force_full: bool,
    honest_repeat: usize,
}

#[derive(Clone, Copy, Debug)]
struct TickCost {
    median: Duration,
    p95: Duration,
    maximum: Duration,
}

#[derive(Clone, Debug)]
struct EchoCandidate {
    delay_s: f64,
    level_below_tail_peak_db: f64,
    prominence_above_local_floor_db: f64,
    facade_range: bool,
}

#[derive(Clone, Debug)]
struct TailMetrics {
    energy: f64,
    peak_dbfs: f64,
    above_signal_floor: bool,
    first_arrival_s: Option<f64>,
    first_arrival_after_blast_ms: Option<f64>,
    decay_20_s: Option<f64>,
    decay_40_s: Option<f64>,
    decay_60_s: Option<f64>,
    echoes: Vec<EchoCandidate>,
}

#[derive(Clone, Debug)]
struct MeasurementRow {
    config: ConfigCase,
    listener: ListenerCase,
    boot: QualityGovernorTelemetry,
    delivered: QualityGovernorTelemetry,
    direct_occlusion: f32,
    path_eq: [f32; 3],
    path_sh_energy: f32,
    source_has_probe: bool,
    listener_has_probe: bool,
    sdk_ir_frames: i32,
    sdk_reverb_times_s: [f32; 3],
    tick_cost: TickCost,
    direct_peak_dbfs: f64,
    path: TailMetrics,
    all_stages: TailMetrics,
    tail: TailMetrics,
    tail_below_direct_db: f64,
    crack_window_energy: f64,
    blast_window_energy: f64,
    crack_total_energy: f64,
    crack_below_blast_db: f64,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock dual bake"]
fn wave14_echo_truth_megablock_diagnostic() {
    let package = env_path("FIGHTBOX_DIAG_PACKAGE", DEFAULT_PACKAGE);
    let bake = env_path("FIGHTBOX_DIAG_BAKE", DEFAULT_BAKE);
    let (mesh, baked) =
        super::megablock_corner_diagnostic::load_megablock_corner_fixture(&package, &bake);

    print_configuration(&package, &bake, &mesh);
    let mut rows = Vec::with_capacity(CONFIGS.len() * LISTENERS.len());
    for config in CONFIGS {
        for listener in LISTENERS {
            println!(
                "WAVE14_RUN_START config={} listener={}",
                config.label, listener.label
            );
            rows.push(measure_case(&mesh, &baked, config, listener));
            println!(
                "WAVE14_RUN_DONE config={} listener={}",
                config.label, listener.label
            );
        }
    }

    print_measurement_table(&rows);
    print_echo_table(&rows);
    print_crack_table(&rows);
    print_determinism_table(&rows);
    print_verdict(&rows);

    assert!(
        rows.iter()
            .all(|row| row.crack_total_energy <= f64::EPSILON),
        "the reflection-send-disabled crack produced nonzero reflection energy"
    );
}

fn measure_case(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    config_case: ConfigCase,
    listener_case: ListenerCase,
) -> MeasurementRow {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let simulation_config = simulation_config(config_case.requested_ir_s);
    let shot = ballistic_shot();
    let plan = plan_ballistic_shot(shot, listener_case.position)
        .expect("Wave 14 listener must admit the megablock ballistic shot");
    let descriptors = descriptors(&plan);
    let (mut simulation, mut render) =
        build_multi_source_session(mesh, baked, audio, simulation_config, &descriptors)
            .expect("build Wave 14 megablock echo-truth session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take Wave 14 stage output control");
    stage_gains.publish(REFLECTIONS_ONLY);

    let boot = simulation.quality_governor_telemetry();
    if !config_case.force_full {
        assert_eq!(
            boot.reflections.level,
            ReflectionQualityLevel::Reduced,
            "the four-slot 1.5 s transient session should honestly boot Reduced"
        );
    } else {
        recover_governor_to_full(&mut simulation, simulation_config);
    }
    let delivered = simulation.quality_governor_telemetry();
    if config_case.force_full {
        assert_eq!(delivered.reflections.level, ReflectionQualityLevel::Full);
        assert_eq!(
            delivered.sources[BLAST_SOURCE].quality,
            SourceQualityLevel::Full
        );
        assert!(
            (delivered.reflections.ir_duration_s - config_case.requested_ir_s).abs()
                <= f32::EPSILON,
            "forced Full did not preserve the requested IR duration"
        );
    }

    simulation.update_inputs(&ballistic_update(&plan, listener_case.position));
    simulation.run_direct().expect("Wave 14 direct simulation");
    simulation.run_pathing().expect("Wave 14 path simulation");
    simulation
        .run_reflections()
        .expect("Wave 14 warm reflection simulation");

    let cadence_divisor = u64::from(delivered.reflections.cadence_divisor);
    let mut tick_samples = Vec::with_capacity(TIMING_REPEATS);
    let mut scheduled_calls = 0;
    while tick_samples.len() < TIMING_REPEATS {
        let executes_tick = simulation
            .reflection_cadence_tick
            .is_multiple_of(cadence_divisor);
        let started = Instant::now();
        simulation
            .run_reflections()
            .expect("Wave 14 timed reflection simulation");
        let elapsed = started.elapsed();
        scheduled_calls += 1;
        if executes_tick {
            tick_samples.push(elapsed);
        }
    }
    let tick_cost = summarize_ticks(&tick_samples);

    settle_event_sources(&mut render, &mut stage_gains, REFLECTIONS_ONLY);
    let propagation = simulation.snapshot.sources[BLAST_SOURCE];
    let reflection = propagation.reflections;
    let direct_occlusion = propagation.direct.occlusion;
    let path_eq = propagation.path_eq;
    let path_sh_energy = coefficient_energy(propagation.path_sh);
    let source_has_probe = simulation
        .world
        .has_influencing_probe(simulation.frame.sources[BLAST_SOURCE].position);
    let listener_has_probe = simulation
        .world
        .has_influencing_probe(simulation.frame.listener.position);

    let delivered_ir_s = f64::from(delivered.reflections.ir_duration_s);
    let capture_s = plan.blast.arrival_time_s + delivered_ir_s + CAPTURE_MARGIN_S;
    let capture_frames = round_up_to_block(seconds_to_frames(capture_s));

    let (crack_program, _) = synthesize_crack_stem(&plan, SAMPLE_RATE as u32, capture_frames)
        .expect("synthesize Wave 14 crack stem through ballistic_event.rs");
    let crack_capture = render_source_program(&mut render, CRACK_SOURCE, &crack_program);
    let crack_total_energy = stereo_energy(&crack_capture);

    let mut blast_program = vec![0.0_f32; capture_frames];
    blast_program[0] = 1.0;
    let blast_capture = render_source_program(&mut render, BLAST_SOURCE, &blast_program);
    let tail = analyze_tail(
        &blast_capture,
        plan.blast.arrival_time_s,
        delivered_ir_s,
        listener_case.inspect_echoes,
    );

    settle_event_sources(&mut render, &mut stage_gains, PATH_ONLY);
    let path_capture = render_source_program(&mut render, BLAST_SOURCE, &blast_program);
    let path = analyze_tail(
        &path_capture,
        plan.blast.arrival_time_s,
        delivered_ir_s,
        false,
    );

    settle_event_sources(&mut render, &mut stage_gains, StageOutputGains::UNITY);
    let all_stages_capture = render_source_program(&mut render, BLAST_SOURCE, &blast_program);
    let all_stages = analyze_tail(
        &all_stages_capture,
        plan.blast.arrival_time_s,
        delivered_ir_s,
        false,
    );

    // Measure the direct stage with the identical source, unit impulse, distance
    // delay, and impulse-class shaping used for the reflection capture. This is
    // deliberately last: the direct stimulus also enters Steam Audio's muted
    // reflection effect, so measuring it first would contaminate a later tail.
    settle_event_sources(&mut render, &mut stage_gains, DIRECT_ONLY);
    let direct_capture = render_source_program(&mut render, BLAST_SOURCE, &blast_program);
    let direct_peak_dbfs = peak_dbfs(&direct_capture);
    let tail_below_direct_db = tail.peak_dbfs - direct_peak_dbfs;

    let crack_window_energy = plan.crack.map_or(0.0, |crack| {
        centered_window_energy(&crack_capture, crack.arrival_time_s, 0.010)
    });
    let blast_window_center_s = tail.first_arrival_s.unwrap_or(plan.blast.arrival_time_s) + 0.025;
    let blast_window_energy = centered_window_energy(&blast_capture, blast_window_center_s, 0.050);
    let crack_below_blast_db = energy_ratio_db(crack_window_energy, blast_window_energy);

    println!(
        "WAVE14_TICKS config={} listener={} cadence_divisor={} scheduled_calls={} executed_samples_ms={:?}",
        config_case.label,
        listener_case.label,
        cadence_divisor,
        scheduled_calls,
        tick_samples
            .iter()
            .map(|duration| duration_ms(*duration))
            .collect::<Vec<_>>()
    );

    MeasurementRow {
        config: config_case,
        listener: listener_case,
        boot,
        delivered,
        direct_occlusion,
        path_eq,
        path_sh_energy,
        source_has_probe,
        listener_has_probe,
        sdk_ir_frames: reflection.ir_size,
        sdk_reverb_times_s: reflection.reverb_times,
        tick_cost,
        direct_peak_dbfs,
        path,
        all_stages,
        tail,
        tail_below_direct_db,
        crack_window_energy,
        blast_window_energy,
        crack_total_energy,
        crack_below_blast_db,
    }
}

fn simulation_config(duration_s: f32) -> S3SimulationConfig {
    S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 64,
        },
        reflection_rays: FULL_REFLECTION_RAYS,
        diffuse_samples: 32,
        reflection_bounces: FULL_REFLECTION_BOUNCES,
        reflection_duration_s: duration_s,
        reflection_order: FULL_REFLECTION_ORDER,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        pathing_visibility_range_m: 10.0,
        validate_paths: true,
        find_alternate_paths: true,
        ..S3SimulationConfig::default()
    }
}

fn ballistic_shot() -> BallisticShot {
    BallisticShot {
        muzzle_position_enu: MUZZLE,
        direction_enu: SHOT_DIRECTION,
        mach: SHOT_MACH,
        levels: BallisticEventLevels {
            blast_spl_at_one_meter_db: BLAST_SPL_AT_ONE_METER_DB,
            crack_over_blast_db_at_reference: CRACK_OVER_BLAST_DB_AT_REFERENCE,
        },
    }
}

fn descriptors(plan: &crate::BallisticShotPlan) -> [crate::MultiSourceDescriptor; 4] {
    let crack = plan.crack.unwrap_or(plan.blast);
    [
        crate::MultiSourceDescriptor::at(RETAINED_A_POSITION)
            .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 155.0 }),
        crate::MultiSourceDescriptor::at(RETAINED_B_POSITION)
            .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 118.0 }),
        crate::MultiSourceDescriptor::at(crack.position_enu)
            .with_reference_level(ReferenceLevel::SplAtOneMeter {
                db_spl: crack.spl_at_one_meter_db as f32,
            })
            .with_initially_active(false)
            .with_source_priority(SourcePriorityClass::TransientEvent)
            .with_reflection_send(false),
        crate::MultiSourceDescriptor::at(plan.blast.position_enu)
            .with_reference_level(ReferenceLevel::SplAtOneMeter {
                db_spl: plan.blast.spl_at_one_meter_db as f32,
            })
            .with_impulse_class(ImpulseClass::ArtilleryThunder)
            .with_initially_active(false)
            .with_source_priority(SourcePriorityClass::TransientEvent),
    ]
}

fn ballistic_update(plan: &crate::BallisticShotPlan, listener: ApiEnuVector3) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[RETAINED_SOURCE_A] = active_source(RETAINED_A_POSITION);
    sources[RETAINED_SOURCE_B] = active_source(RETAINED_B_POSITION);
    if let Some(crack) = plan.crack {
        sources[CRACK_SOURCE] = active_source(crack.position_enu);
    }
    sources[BLAST_SOURCE] = active_source(plan.blast.position_enu);
    SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: default_api_pose(listener),
            linear_velocity_mps: ApiEnuVector3::default(),
        },
        sources,
    }
}

fn active_source(position: ApiEnuVector3) -> SourceMotion {
    SourceMotion {
        active: true,
        pose: default_api_pose(position),
        linear_velocity_mps: ApiEnuVector3::default(),
    }
}

fn recover_governor_to_full(simulation: &mut MultiSourceSimulation, config: S3SimulationConfig) {
    for _ in 0..30_000 {
        simulation.observe_render_timing(100_000);
        let quality = simulation.quality_governor_telemetry();
        if quality.reflections.level == ReflectionQualityLevel::Full
            && quality.sources[..4]
                .iter()
                .all(|source| source.quality == SourceQualityLevel::Full)
            && (quality.reflections.ir_duration_s - config.reflection_duration_s).abs()
                <= f32::EPSILON
        {
            return;
        }
    }
    panic!(
        "Wave 14 governor did not recover to Full: {:?}",
        simulation.quality_governor_telemetry()
    );
}

fn settle_event_sources(
    render: &mut MultiSourceRenderGraph,
    stage_gains: &mut SnapshotWriter<StageOutputGains>,
    gains: StageOutputGains,
) {
    stage_gains.publish(gains);
    let zeros = [0.0_f32; BLOCK_FRAMES as usize];
    for _ in 0..SETTLE_BLOCKS {
        render_event_block(render, &zeros, &zeros, None);
    }
}

fn render_source_program(
    render: &mut MultiSourceRenderGraph,
    source_index: usize,
    input: &[f32],
) -> Vec<f32> {
    assert_eq!(input.len() % BLOCK_FRAMES as usize, 0);
    let mut stereo = Vec::with_capacity(input.len() * 2);
    for input_block in input.chunks_exact(BLOCK_FRAMES as usize) {
        let source = [BackendSourceBlock {
            source_index,
            input_mono: input_block,
        }];
        let mut left = [0.0_f32; BLOCK_FRAMES as usize];
        let mut right = [0.0_f32; BLOCK_FRAMES as usize];
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: listener_orientation(),
                sources: &source,
                output_left: &mut left,
                output_right: &mut right,
            })
            .expect("render Wave 14 isolated source block");
        stereo.extend(
            left.into_iter()
                .zip(right)
                .flat_map(|(left, right)| [left, right]),
        );
    }
    stereo
}

fn render_event_block(
    render: &mut MultiSourceRenderGraph,
    crack: &[f32],
    blast: &[f32],
    output: Option<&mut Vec<f32>>,
) {
    let sources = [
        BackendSourceBlock {
            source_index: CRACK_SOURCE,
            input_mono: crack,
        },
        BackendSourceBlock {
            source_index: BLAST_SOURCE,
            input_mono: blast,
        },
    ];
    let mut left = [0.0_f32; BLOCK_FRAMES as usize];
    let mut right = [0.0_f32; BLOCK_FRAMES as usize];
    render
        .render_block(PropagationRenderBlock {
            listener_orientation: listener_orientation(),
            sources: &sources,
            output_left: &mut left,
            output_right: &mut right,
        })
        .expect("render Wave 14 event settle block");
    if let Some(output) = output {
        output.extend(
            left.into_iter()
                .zip(right)
                .flat_map(|(left, right)| [left, right]),
        );
    }
}

const fn listener_orientation() -> ListenerOrientation {
    ListenerOrientation {
        forward: ApiEnuVector3::new(1.0, 0.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn summarize_ticks(samples: &[Duration]) -> TickCost {
    assert!(!samples.is_empty());
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = sorted[sorted.len() / 2];
    let p95_index = ((sorted.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    TickCost {
        median,
        p95: sorted[p95_index],
        maximum: *sorted.last().expect("nonempty tick timings"),
    }
}

fn analyze_tail(
    stereo: &[f32],
    blast_arrival_s: f64,
    delivered_ir_s: f64,
    inspect_echoes: bool,
) -> TailMetrics {
    let frame_energy = stereo
        .chunks_exact(2)
        .map(|frame| {
            let left = f64::from(frame[0]);
            let right = f64::from(frame[1]);
            left * left + right * right
        })
        .collect::<Vec<_>>();
    let blast_frame = seconds_to_frames(blast_arrival_s).min(frame_energy.len());
    let tail = &frame_energy[blast_frame..];
    let energy = tail.iter().sum::<f64>();
    let peak_energy = tail.iter().copied().fold(0.0_f64, f64::max);
    if energy <= 0.0 || peak_energy <= 0.0 {
        return TailMetrics {
            energy,
            peak_dbfs: f64::NEG_INFINITY,
            above_signal_floor: false,
            first_arrival_s: None,
            first_arrival_after_blast_ms: None,
            decay_20_s: None,
            decay_40_s: None,
            decay_60_s: None,
            echoes: Vec::new(),
        };
    }

    let first_threshold = peak_energy * 1.0e-8;
    let first_offset = tail
        .iter()
        .position(|sample_energy| *sample_energy >= first_threshold)
        .expect("positive tail has an arrival above its -80 dB threshold");
    let first_frame = blast_frame + first_offset;
    let first_arrival_s = first_frame as f64 / SAMPLE_RATE as f64;
    let echoes = if inspect_echoes {
        find_echoes(&frame_energy, first_frame, delivered_ir_s)
    } else {
        Vec::new()
    };

    let peak_dbfs = 10.0 * (peak_energy / 2.0).log10();
    TailMetrics {
        energy,
        peak_dbfs,
        above_signal_floor: peak_dbfs >= TAIL_SIGNAL_FLOOR_DBFS,
        first_arrival_s: Some(first_arrival_s),
        first_arrival_after_blast_ms: Some((first_arrival_s - blast_arrival_s) * 1_000.0),
        decay_20_s: decay_crossing(tail, energy, -20.0),
        decay_40_s: decay_crossing(tail, energy, -40.0),
        decay_60_s: decay_crossing(tail, energy, -60.0),
        echoes,
    }
}

fn decay_crossing(energy: &[f64], total: f64, target_db: f64) -> Option<f64> {
    if total <= 0.0 {
        return None;
    }
    let target = total * 10.0_f64.powf(target_db / 10.0);
    let mut remaining = total;
    for (index, sample) in energy.iter().copied().enumerate() {
        remaining = (remaining - sample).max(0.0);
        if remaining <= target {
            return Some((index + 1) as f64 / SAMPLE_RATE as f64);
        }
    }
    None
}

fn find_echoes(
    frame_energy: &[f64],
    first_frame: usize,
    delivered_ir_s: f64,
) -> Vec<EchoCandidate> {
    const WINDOW_S: f64 = 0.020;
    const HOP_S: f64 = 0.005;
    const LOCAL_RADIUS_S: f64 = 0.075;
    const EXCLUSION_S: f64 = 0.015;
    const MIN_DELAY_S: f64 = 0.050;
    const MIN_SEPARATION_S: f64 = 0.050;
    const MIN_PROMINENCE_DB: f64 = 6.0;
    const MIN_LEVEL_DB: f64 = -50.0;

    let window = seconds_to_frames(WINDOW_S).max(1);
    let hop = seconds_to_frames(HOP_S).max(1);
    let end_frame = (first_frame + seconds_to_frames(delivered_ir_s))
        .min(frame_energy.len())
        .saturating_sub(window);
    if end_frame <= first_frame {
        return Vec::new();
    }
    let mut envelope = Vec::new();
    let mut frame = first_frame;
    while frame <= end_frame {
        let mean = frame_energy[frame..frame + window].iter().sum::<f64>() / window as f64;
        envelope.push((frame, mean));
        frame = frame.saturating_add(hop);
    }
    if envelope.len() < 3 {
        return Vec::new();
    }
    let peak = envelope
        .iter()
        .map(|(_, value)| *value)
        .fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return Vec::new();
    }
    let local_radius = (LOCAL_RADIUS_S / HOP_S).round() as usize;
    let exclusion = (EXCLUSION_S / HOP_S).round() as usize;
    let mut candidates = Vec::new();
    for index in 1..envelope.len() - 1 {
        let (candidate_frame, value) = envelope[index];
        let delay_s = (candidate_frame - first_frame) as f64 / SAMPLE_RATE as f64;
        if delay_s < MIN_DELAY_S || value < envelope[index - 1].1 || value <= envelope[index + 1].1
        {
            continue;
        }
        let start = index.saturating_sub(local_radius);
        let end = (index + local_radius + 1).min(envelope.len());
        let mut floor_samples = envelope[start..end]
            .iter()
            .enumerate()
            .filter_map(|(offset, (_, sample))| {
                let absolute = start + offset;
                (absolute.abs_diff(index) > exclusion).then_some(*sample)
            })
            .filter(|sample| *sample > 0.0)
            .collect::<Vec<_>>();
        if floor_samples.is_empty() {
            continue;
        }
        floor_samples.sort_by(f64::total_cmp);
        let floor = floor_samples[floor_samples.len() / 2];
        let level_db = 10.0 * (value / peak).log10();
        let prominence_db = 10.0 * (value / floor).log10();
        if level_db >= MIN_LEVEL_DB && prominence_db >= MIN_PROMINENCE_DB {
            candidates.push(EchoCandidate {
                delay_s,
                level_below_tail_peak_db: level_db,
                prominence_above_local_floor_db: prominence_db,
                facade_range: (0.3..=1.2).contains(&delay_s),
            });
        }
    }

    candidates.sort_by(|left, right| {
        right
            .prominence_above_local_floor_db
            .total_cmp(&left.prominence_above_local_floor_db)
    });
    let mut selected: Vec<EchoCandidate> = Vec::new();
    for candidate in candidates {
        if selected
            .iter()
            .all(|kept| (kept.delay_s - candidate.delay_s).abs() >= MIN_SEPARATION_S)
        {
            selected.push(candidate);
        }
    }
    selected.sort_by(|left, right| left.delay_s.total_cmp(&right.delay_s));
    selected
}

fn centered_window_energy(stereo: &[f32], center_s: f64, width_s: f64) -> f64 {
    let half = seconds_to_frames(width_s) / 2;
    let center = seconds_to_frames(center_s);
    let frame_count = stereo.len() / 2;
    let start = center.saturating_sub(half).min(frame_count);
    let end = center.saturating_add(half).min(frame_count);
    stereo[start * 2..end * 2]
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum()
}

fn stereo_energy(stereo: &[f32]) -> f64 {
    stereo
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum()
}

fn coefficient_energy(coefficients: [f32; 16]) -> f32 {
    coefficients
        .into_iter()
        .map(|coefficient| coefficient * coefficient)
        .sum()
}

fn peak_dbfs(stereo: &[f32]) -> f64 {
    let peak_frame_energy = stereo
        .chunks_exact(2)
        .map(|frame| {
            let left = f64::from(frame[0]);
            let right = f64::from(frame[1]);
            left * left + right * right
        })
        .fold(0.0_f64, f64::max);
    if peak_frame_energy > 0.0 {
        10.0 * (peak_frame_energy / 2.0).log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn energy_ratio_db(numerator: f64, denominator: f64) -> f64 {
    if numerator <= 0.0 && denominator > 0.0 {
        f64::NEG_INFINITY
    } else if numerator > 0.0 && denominator > 0.0 {
        10.0 * (numerator / denominator).log10()
    } else {
        f64::NAN
    }
}

fn print_configuration(package: &Path, bake: &Path, mesh: &SceneMesh) {
    let roof_heights = roof_heights_above_muzzle(mesh);
    println!(
        "WAVE14_CONFIG package={} bake={} sample_rate_hz={SAMPLE_RATE} block_frames={BLOCK_FRAMES} debug_only=true slots=4 retained_ordinary=2 event_slots=2 rays={FULL_REFLECTION_RAYS} diffuse_samples=32 bounces={FULL_REFLECTION_BOUNCES} order={FULL_REFLECTION_ORDER} effect=Convolution direct=Volumetric(1m,64) tick_repeats={TIMING_REPEATS} settle_blocks={SETTLE_BLOCKS}",
        package.display(),
        bake.display(),
    );
    println!(
        "WAVE14_SHOT muzzle=[{:.3},{:.3},{:.3}] direction=[{:.1},{:.1},{:.1}] mach={SHOT_MACH:.1} blast_db_spl_at_1m={BLAST_SPL_AT_ONE_METER_DB:.1} crack_over_blast_db_at_reference={CRACK_OVER_BLAST_DB_AT_REFERENCE:.1} ballistic_api=plan_ballistic_shot+synthesize_crack_stem blast_stimulus=unit_impulse_through_ArtilleryThunder",
        MUZZLE.east_m,
        MUZZLE.north_m,
        MUZZLE.up_m,
        SHOT_DIRECTION.east_m,
        SHOT_DIRECTION.north_m,
        SHOT_DIRECTION.up_m,
    );
    println!(
        "WAVE14_GEOMETRY muzzle_roof_hits={} roof_heights_m={roof_heights:?} muzzle_enclosed_below_roof={} nominal_los_labels_require_direct_occlusion_check=true",
        roof_heights.len(),
        !roof_heights.is_empty(),
    );
    println!(
        "WAVE14_ANALYSIS edc=reverse_integrated_stereo_energy thresholds_db=-20/-40/-60 first_arrival_threshold_db=-80 tail_signal_floor_dbfs={TAIL_SIGNAL_FLOOR_DBFS:.1} envelope_window_ms=20 envelope_hop_ms=5 echo_min_delay_ms=50 echo_min_separation_ms=50 echo_min_local_prominence_db=6 echo_min_tail_level_db=-50 expected_facade_range_s=0.3..1.2"
    );
}

fn print_measurement_table(rows: &[MeasurementRow]) {
    println!(
        "WAVE14_TABLE config\trepeat\tlistener\tposition\tdistance_m\tdirect_occlusion\tdirect_stage_peak_dbfs\tpath_energy\tpath_peak_dbfs\tpath_first_after_blast_ms\tpath_below_direct_db\tall_stages_energy\tall_stages_peak_dbfs\tall_stages_first_after_blast_ms\tpath_sh_energy\tpath_eq\ttail_below_direct_db\tsource_probe\tlistener_probe\tboot_level\tdelivered_level\trays\tbounces\trequested_ir_s\tdelivered_ir_s\tcadence\tsdk_ir_frames\tsdk_ir_s\tcapacity_clamped\treverb_times_s\texecuted_tick_median_ms\texecuted_tick_p95_ms\texecuted_tick_max_ms\ttail_energy\ttail_peak_dbfs\ttail_above_signal_floor\tfirst_after_blast_ms\tedc_t20_s\tedc_t40_s\tedc_t60_s\tdiscrete_count"
    );
    for row in rows {
        let delivered = row.delivered.reflections;
        let sdk_ir_s = row.sdk_ir_frames as f64 / SAMPLE_RATE as f64;
        let capacity_clamped = row.config.force_full
            && (sdk_ir_s - f64::from(row.config.requested_ir_s)).abs() > 1.0 / SAMPLE_RATE as f64;
        println!(
            "WAVE14_TABLE {}\t{}\t{}\t[{:.1},{:.1},{:.1}]\t{:.3}\t{:.9e}\t{}\t{:.9e}\t{}\t{}\t{}\t{:.9e}\t{}\t{}\t{:.9e}\t{:.6}/{:.6}/{:.6}\t{}\t{}\t{}\t{:?}\t{:?}\t{}\t{}\t{:.3}\t{:.3}\t{}\t{}\t{sdk_ir_s:.6}\t{}\t{:.3}/{:.3}/{:.3}\t{:.6}\t{:.6}\t{:.6}\t{:.9e}\t{:.3}\t{}\t{}\t{}\t{}\t{}\t{}",
            row.config.label,
            row.config.honest_repeat,
            row.listener.label,
            row.listener.position.east_m,
            row.listener.position.north_m,
            row.listener.position.up_m,
            distance(MUZZLE, row.listener.position),
            row.direct_occlusion,
            format_db(row.direct_peak_dbfs),
            row.path.energy,
            format_db(row.path.peak_dbfs),
            format_option(row.path.first_arrival_after_blast_ms, 3),
            format_db(row.path.peak_dbfs - row.direct_peak_dbfs),
            row.all_stages.energy,
            format_db(row.all_stages.peak_dbfs),
            format_option(row.all_stages.first_arrival_after_blast_ms, 3),
            row.path_sh_energy,
            row.path_eq[0],
            row.path_eq[1],
            row.path_eq[2],
            format_db(row.tail_below_direct_db),
            row.source_has_probe,
            row.listener_has_probe,
            row.boot.reflections.level,
            delivered.level,
            delivered.rays,
            delivered.bounces,
            row.config.requested_ir_s,
            delivered.ir_duration_s,
            delivered.cadence_divisor,
            row.sdk_ir_frames,
            capacity_clamped,
            row.sdk_reverb_times_s[0],
            row.sdk_reverb_times_s[1],
            row.sdk_reverb_times_s[2],
            duration_ms(row.tick_cost.median),
            duration_ms(row.tick_cost.p95),
            duration_ms(row.tick_cost.maximum),
            row.tail.energy,
            row.tail.peak_dbfs,
            row.tail.above_signal_floor,
            format_option(row.tail.first_arrival_after_blast_ms, 3),
            format_option(row.tail.decay_20_s, 6),
            format_option(row.tail.decay_40_s, 6),
            format_option(row.tail.decay_60_s, 6),
            row.tail.echoes.len(),
        );
    }
}

fn print_echo_table(rows: &[MeasurementRow]) {
    println!(
        "WAVE14_ECHO config\tlistener\treturn\tdelay_from_first_s\tlevel_below_tail_peak_db\tprominence_above_local_floor_db\tin_facade_range"
    );
    for row in rows.iter().filter(|row| row.listener.inspect_echoes) {
        if row.tail.echoes.is_empty() {
            println!(
                "WAVE14_ECHO {}\t{}\tnone\t-\t-\t-\tfalse",
                row.config.label, row.listener.label
            );
        }
        for (index, echo) in row.tail.echoes.iter().enumerate() {
            println!(
                "WAVE14_ECHO {}\t{}\t{}\t{:.6}\t{:.3}\t{:.3}\t{}",
                row.config.label,
                row.listener.label,
                index + 1,
                echo.delay_s,
                echo.level_below_tail_peak_db,
                echo.prominence_above_local_floor_db,
                echo.facade_range,
            );
        }
    }
}

fn print_crack_table(rows: &[MeasurementRow]) {
    println!(
        "WAVE14_CRACK config\tlistener\tcrack_window_energy\tblast_window_energy\tcrack_below_blast_db\tcrack_total_reflection_energy\treflection_send_pinned_off"
    );
    for row in rows {
        println!(
            "WAVE14_CRACK {}\t{}\t{:.9e}\t{:.9e}\t{}\t{:.9e}\t{}",
            row.config.label,
            row.listener.label,
            row.crack_window_energy,
            row.blast_window_energy,
            format_db(row.crack_below_blast_db),
            row.crack_total_energy,
            row.crack_total_energy <= f64::EPSILON,
        );
    }
}

fn print_determinism_table(rows: &[MeasurementRow]) {
    println!(
        "WAVE14_DETERMINISM listener\tt20_spread_ms\tt40_spread_ms\tt60_spread_ms\ttick_median_spread_ms\ttick_median_spread_pct\ttail_peak_spread_db\techo_count_a\techo_count_b"
    );
    for listener in LISTENERS {
        let first = rows
            .iter()
            .find(|row| row.config.honest_repeat == 1 && row.listener.label == listener.label)
            .expect("honest repeat A row");
        let second = rows
            .iter()
            .find(|row| row.config.honest_repeat == 2 && row.listener.label == listener.label)
            .expect("honest repeat B row");
        let tick_spread_ms =
            (duration_ms(first.tick_cost.median) - duration_ms(second.tick_cost.median)).abs();
        let tick_mean_ms =
            (duration_ms(first.tick_cost.median) + duration_ms(second.tick_cost.median)) / 2.0;
        println!(
            "WAVE14_DETERMINISM {}\t{}\t{}\t{}\t{tick_spread_ms:.6}\t{:.3}\t{:.3}\t{}\t{}",
            listener.label,
            format_option(
                option_spread_ms(first.tail.decay_20_s, second.tail.decay_20_s),
                3
            ),
            format_option(
                option_spread_ms(first.tail.decay_40_s, second.tail.decay_40_s),
                3
            ),
            format_option(
                option_spread_ms(first.tail.decay_60_s, second.tail.decay_60_s),
                3
            ),
            if tick_mean_ms > 0.0 {
                tick_spread_ms * 100.0 / tick_mean_ms
            } else {
                0.0
            },
            (first.tail.peak_dbfs - second.tail.peak_dbfs).abs(),
            first.tail.echoes.len(),
            second.tail.echoes.len(),
        );
    }
}

fn print_verdict(rows: &[MeasurementRow]) {
    let inspected = rows.iter().filter(|row| row.listener.inspect_echoes);
    let any_discrete = inspected.clone().any(|row| !row.tail.echoes.is_empty());
    let any_facade = inspected
        .clone()
        .flat_map(|row| &row.tail.echoes)
        .any(|echo| echo.facade_range);
    let max_t60_s = rows
        .iter()
        .filter_map(|row| row.tail.decay_60_s)
        .reduce(f64::max);
    let street_los_all_blocked = rows
        .iter()
        .filter(|row| row.listener.label.contains("los"))
        .all(|row| row.direct_occlusion <= 1.0e-6);
    let any_tail_above_signal_floor = rows.iter().any(|row| row.tail.above_signal_floor);
    println!(
        "WAVE14_VERDICT any_tail_above_signal_floor={} any_discrete_above_diffuse_floor={} any_facade_range_return={} maximum_formal_edc_t60_s={} street_los_cases_all_directly_blocked={} crack_reflection_null={}",
        any_tail_above_signal_floor,
        any_discrete,
        any_facade,
        format_option(max_t60_s, 6),
        street_los_all_blocked,
        rows.iter()
            .all(|row| row.crack_total_energy <= f64::EPSILON),
    );
}

fn roof_heights_above_muzzle(mesh: &SceneMesh) -> Vec<f32> {
    let mut heights = mesh
        .triangles
        .iter()
        .filter_map(|triangle| {
            let vertices = triangle.map(|index| mesh.vertices_enu_m[index as usize]);
            let height = vertices[0].z;
            ((height - vertices[1].z).abs() <= 1.0e-4
                && (height - vertices[2].z).abs() <= 1.0e-4
                && height > MUZZLE.up_m
                && point_in_triangle_xy(MUZZLE.east_m, MUZZLE.north_m, vertices))
            .then_some(height)
        })
        .collect::<Vec<_>>();
    heights.sort_by(f32::total_cmp);
    heights.dedup_by(|left, right| (*left - *right).abs() <= 1.0e-3);
    heights
}

fn point_in_triangle_xy(east_m: f32, north_m: f32, vertices: [EnuVector3; 3]) -> bool {
    let sign =
        |a: EnuVector3, b: EnuVector3| (east_m - b.x) * (a.y - b.y) - (a.x - b.x) * (north_m - b.y);
    let d1 = sign(vertices[0], vertices[1]);
    let d2 = sign(vertices[1], vertices[2]);
    let d3 = sign(vertices[2], vertices[0]);
    let has_negative = d1 < -1.0e-5 || d2 < -1.0e-5 || d3 < -1.0e-5;
    let has_positive = d1 > 1.0e-5 || d2 > 1.0e-5 || d3 > 1.0e-5;
    !(has_negative && has_positive)
}

fn distance(left: ApiEnuVector3, right: ApiEnuVector3) -> f64 {
    let east = f64::from(left.east_m - right.east_m);
    let north = f64::from(left.north_m - right.north_m);
    let up = f64::from(left.up_m - right.up_m);
    (east * east + north * north + up * up).sqrt()
}

fn option_spread_ms(left: Option<f64>, right: Option<f64>) -> Option<f64> {
    left.zip(right)
        .map(|(left, right)| (left - right).abs() * 1_000.0)
}

fn seconds_to_frames(seconds: f64) -> usize {
    (seconds * SAMPLE_RATE as f64).round().max(0.0) as usize
}

fn round_up_to_block(frames: usize) -> usize {
    frames.div_ceil(BLOCK_FRAMES as usize) * BLOCK_FRAMES as usize
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn format_option(value: Option<f64>, precision: usize) -> String {
    value.map_or_else(
        || "not-reached".to_owned(),
        |value| format!("{value:.precision$}"),
    )
}

fn format_db(value: f64) -> String {
    if value == f64::NEG_INFINITY {
        "-inf".to_owned()
    } else if value.is_finite() {
        format!("{value:.3}")
    } else {
        "undefined".to_owned()
    }
}

fn env_path(variable: &str, default: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}
