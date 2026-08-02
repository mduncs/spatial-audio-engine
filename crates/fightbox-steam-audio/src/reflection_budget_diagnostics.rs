//! Ignored, release-only measurements for the reflection-budget design study.
//!
//! These tests deliberately live beside `multi_source.rs`: they need to time
//! the exact retained simulator/effect handles without exposing diagnostic API
//! through the production crate. Run them with the locally acquired 4.8.1 SDK:
//!
//! ```text
//! STEAM_AUDIO_SDK_DIR=/absolute/path/to/steamaudio \
//! cargo test --release -p fightbox-steam-audio --features linked-sdk \
//!   reflection_budget_ -- --ignored --nocapture --test-threads=1
//! ```

use super::*;
use crate::{AcousticMaterial, ReflectionQualityLevel};
use fightbox_runtime::backend::{BackendSourceBlock, SimulationRunner, SourceMotion};
use fightbox_runtime::{SimulationCadences, SimulationWorker};
use std::env;
use std::ffi::c_void;
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const BLOCK_DEADLINE_NS: u64 = BLOCK_FRAMES as u64 * 1_000_000_000 / SAMPLE_RATE as u64;
const SOURCE: ApiEnuVector3 = ApiEnuVector3::new(0.5, 0.0, 2.0);
const LISTENER: ApiEnuVector3 = ApiEnuVector3::new(0.0, 0.0, 2.0);
const MEGABLOCK_PROBE_COUNT: u64 = 19_881;

#[derive(Clone, Copy, Debug)]
struct ExactReflectionOutput {
    elapsed: Duration,
    reflection: SteamReflectionParams,
}

#[test]
#[ignore = "release-only Steam Audio reflection simulation matrix"]
fn reflection_budget_simulation_matrix_diagnostic() {
    require_release();
    let repeats = env_usize("FIGHTBOX_DIAG_REPEATS", 5).max(3);
    let audio = audio_config();
    let config = reflection_config(16_384, 16, 3.0, 2);
    let descriptors = [crate::MultiSourceDescriptor::at(SOURCE)];
    let (mut simulation, _render) = build_multi_source_generation(
        &controlled_canyon_mesh(),
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build max-capacity reflection matrix session");
    simulation.update_inputs(&source_update(&[SOURCE], LISTENER));
    earn_full_quality(&mut simulation, 2);

    println!(
        "SIM_MATRIX rays,bounces,duration_s,order,channels,ir_samples,median_ms,worst_ms,repeats"
    );
    for rays in [2_048, 4_096, 8_192, 16_384] {
        for bounces in [4, 8, 16] {
            for duration_s in [1.0, 2.0, 3.0] {
                for order in [1, 2] {
                    let warmup = run_exact_reflection_tick(
                        &mut simulation,
                        rays,
                        bounces,
                        duration_s,
                        order,
                        &[0],
                    );
                    verify_reflection_shape(warmup.reflection, duration_s, order);
                    let mut samples = Vec::with_capacity(repeats);
                    for _ in 0..repeats {
                        let sample = run_exact_reflection_tick(
                            &mut simulation,
                            rays,
                            bounces,
                            duration_s,
                            order,
                            &[0],
                        );
                        verify_reflection_shape(sample.reflection, duration_s, order);
                        samples.push(sample.elapsed);
                    }
                    let median = median_duration(&samples);
                    let worst = *samples.iter().max().expect("nonempty timing samples");
                    let last = run_exact_reflection_tick(
                        &mut simulation,
                        rays,
                        bounces,
                        duration_s,
                        order,
                        &[0],
                    );
                    println!(
                        "SIM_MATRIX {rays},{bounces},{duration_s:.1},{order},{},{},{:.6},{:.6},{repeats}",
                        last.reflection.num_channels,
                        last.reflection.ir_size,
                        duration_ms(median),
                        duration_ms(worst),
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "release-only Steam Audio reflection render matrix"]
fn reflection_budget_convolution_matrix_diagnostic() {
    require_release();
    let measured_blocks = env_usize("FIGHTBOX_DIAG_RENDER_BLOCKS", 1_000).max(100);
    let warmup_blocks = env_usize("FIGHTBOX_DIAG_RENDER_WARMUP_BLOCKS", 96).max(16);
    println!(
        "CONV_MATRIX duration_s,order,channels,sources,median_us,worst_us,median_deadline_pct,worst_deadline_pct,median_blocks_per_s,required_blocks_per_s,headroom_x,blocks"
    );
    for duration_s in [1.0, 2.0, 3.0] {
        for order in [1, 2] {
            let audio = audio_config();
            let config = reflection_config(4_096, 8, duration_s, order);
            let positions = [
                SOURCE,
                ApiEnuVector3::new(-0.5, 0.0, 2.0),
                ApiEnuVector3::new(0.0, 0.5, 2.0),
                ApiEnuVector3::new(0.0, -0.5, 2.0),
            ];
            let descriptors = positions.map(crate::MultiSourceDescriptor::at);
            let (mut simulation, mut render) = build_multi_source_generation(
                &controlled_canyon_mesh(),
                None,
                audio,
                config,
                &descriptors,
                1,
                QualityTier::Desktop,
            )
            .expect("build reflection convolution session");
            simulation.update_inputs(&source_update(&positions, LISTENER));
            earn_full_quality(&mut simulation, order);
            simulation
                .run_reflections()
                .expect("populate reflection IRs for convolution timing");
            for source in 0..positions.len() {
                verify_reflection_shape(
                    simulation.snapshot.sources[source].reflections,
                    duration_s,
                    order,
                );
            }
            prepare_reflection_inputs(&mut render);

            for source_count in [1, 4] {
                for _ in 0..warmup_blocks {
                    run_reflection_render_block(
                        &mut render,
                        &simulation.snapshot.sources,
                        source_count,
                        order,
                        duration_s,
                    );
                }
                let mut samples = Vec::with_capacity(measured_blocks);
                for _ in 0..measured_blocks {
                    let started = Instant::now();
                    run_reflection_render_block(
                        &mut render,
                        &simulation.snapshot.sources,
                        source_count,
                        order,
                        duration_s,
                    );
                    samples.push(started.elapsed());
                }
                let median = median_duration(&samples);
                let worst = *samples.iter().max().expect("nonempty timing samples");
                let median_ns = median.as_nanos() as f64;
                let worst_ns = worst.as_nanos() as f64;
                let deadline_ns = BLOCK_DEADLINE_NS as f64;
                let required_blocks_per_s = SAMPLE_RATE as f64 / BLOCK_FRAMES as f64;
                let blocks_per_s = 1.0e9 / median_ns;
                println!(
                    "CONV_MATRIX {duration_s:.1},{order},{},{source_count},{:.3},{:.3},{:.4},{:.4},{:.3},{required_blocks_per_s:.3},{:.3},{measured_blocks}",
                    ambisonics_channel_count(order).expect("validated order"),
                    median_ns / 1.0e3,
                    worst_ns / 1.0e3,
                    median_ns * 100.0 / deadline_ns,
                    worst_ns * 100.0 / deadline_ns,
                    blocks_per_s,
                    blocks_per_s / required_blocks_per_s,
                );
            }
        }
    }
}

#[test]
#[ignore = "release-only mixed cinematic/standard reflection budget measurement"]
fn reflection_budget_mixed_tier_diagnostic() {
    require_release();
    let cinematic = cinematic_setting();
    let standard = ReflectionSetting {
        rays: 2_048,
        bounces: 4,
        duration_s: 1.0,
        order: 1,
    };
    let audio = audio_config();
    let max_order = cinematic.order.max(standard.order);
    let max_duration = cinematic.duration_s.max(standard.duration_s);
    let max_rays = cinematic.rays.max(standard.rays);
    let max_bounces = cinematic.bounces.max(standard.bounces);
    let config = reflection_config(max_rays, max_bounces, max_duration, max_order);
    let positions = [
        SOURCE,
        ApiEnuVector3::new(-0.5, 0.0, 2.0),
        ApiEnuVector3::new(0.0, 0.5, 2.0),
        ApiEnuVector3::new(0.0, -0.5, 2.0),
    ];
    let descriptors = positions.map(crate::MultiSourceDescriptor::at);
    let (mut simulation, mut render) = build_multi_source_generation(
        &controlled_canyon_mesh(),
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build mixed-tier diagnostic session");
    simulation.update_inputs(&source_update(&positions, LISTENER));
    earn_full_quality(&mut simulation, max_order);

    let repeats = env_usize("FIGHTBOX_DIAG_REPEATS", 5).max(3);
    let mut cinematic_ticks = Vec::with_capacity(repeats);
    let mut standard_ticks = Vec::with_capacity(repeats);
    let mut reflections = [SteamReflectionParams::default(); MAX_ACTIVE_SOURCES];
    for _ in 0..repeats {
        let cinematic_tick = run_exact_reflection_tick(
            &mut simulation,
            cinematic.rays,
            cinematic.bounces,
            cinematic.duration_s,
            cinematic.order,
            &[0],
        );
        cinematic_ticks.push(cinematic_tick.elapsed);
        reflections[0] = cinematic_tick.reflection;

        let standard_tick = run_exact_reflection_tick(
            &mut simulation,
            standard.rays,
            standard.bounces,
            standard.duration_s,
            standard.order,
            &[1, 2, 3],
        );
        standard_ticks.push(standard_tick.elapsed);
        for source in 1..4 {
            reflections[source] = read_reflection_output(&simulation, source);
        }
    }
    verify_reflection_shape(reflections[0], cinematic.duration_s, cinematic.order);
    for reflection in &reflections[1..4] {
        verify_reflection_shape(*reflection, standard.duration_s, standard.order);
    }

    prepare_reflection_inputs(&mut render);
    for _ in 0..128 {
        run_mixed_reflection_render_block(
            &mut render,
            &reflections,
            cinematic.order,
            cinematic.duration_s,
        );
    }
    let blocks = env_usize("FIGHTBOX_DIAG_RENDER_BLOCKS", 1_000).max(100);
    let mut render_samples = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let started = Instant::now();
        run_mixed_reflection_render_block(
            &mut render,
            &reflections,
            cinematic.order,
            cinematic.duration_s,
        );
        render_samples.push(started.elapsed());
    }
    let cinematic_median = median_duration(&cinematic_ticks);
    let cinematic_worst = *cinematic_ticks.iter().max().unwrap();
    let standard_median = median_duration(&standard_ticks);
    let standard_worst = *standard_ticks.iter().max().unwrap();
    let render_median = median_duration(&render_samples);
    let render_worst = *render_samples.iter().max().unwrap();
    println!(
        "MIXED_TIER cinematic={}/{}/{:.1}/{} standard={}/{}/{:.1}/{} cinematic_tick_median_ms={:.6} cinematic_tick_worst_ms={:.6} standard_3src_tick_median_ms={:.6} standard_3src_tick_worst_ms={:.6} combined_tick_median_ms={:.6} combined_tick_worst_sum_ms={:.6} render_median_us={:.3} render_worst_us={:.3} render_median_deadline_pct={:.4} render_worst_deadline_pct={:.4} render_headroom_x={:.3}",
        cinematic.rays,
        cinematic.bounces,
        cinematic.duration_s,
        cinematic.order,
        standard.rays,
        standard.bounces,
        standard.duration_s,
        standard.order,
        duration_ms(cinematic_median),
        duration_ms(cinematic_worst),
        duration_ms(standard_median),
        duration_ms(standard_worst),
        duration_ms(cinematic_median + standard_median),
        duration_ms(cinematic_worst + standard_worst),
        render_median.as_nanos() as f64 / 1.0e3,
        render_worst.as_nanos() as f64 / 1.0e3,
        render_median.as_nanos() as f64 * 100.0 / BLOCK_DEADLINE_NS as f64,
        render_worst.as_nanos() as f64 * 100.0 / BLOCK_DEADLINE_NS as f64,
        BLOCK_DEADLINE_NS as f64 / render_median.as_nanos() as f64,
    );
}

#[test]
#[ignore = "release-only reflection IR staleness measurement"]
fn reflection_budget_staleness_diagnostic() {
    require_release();
    let setting = cinematic_setting();
    let audio = audio_config();
    let config = reflection_config(
        setting.rays,
        setting.bounces,
        setting.duration_s,
        setting.order,
    );
    let descriptors = [crate::MultiSourceDescriptor::at(SOURCE)];
    let initial = source_update(&[SOURCE], LISTENER);
    let (mut simulation, mut render) = build_multi_source_generation(
        &controlled_canyon_mesh(),
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build staleness diagnostic session");
    simulation.update_inputs(&initial);
    earn_full_quality(&mut simulation, setting.order);
    let reflection_epoch = Arc::new(AtomicU64::new(0));
    let mut worker = SimulationWorker::new(
        Box::new(DiagnosticRunner {
            simulation,
            reflection_epoch: Arc::clone(&reflection_epoch),
        }),
        initial,
        SimulationCadences::default(),
    )
    .expect("start reflection simulation worker");
    wait_for_initial_reflection(&mut render);

    let repeats = env_usize("FIGHTBOX_DIAG_STALENESS_REPEATS", 7).max(3);
    let mut periodic_samples = Vec::with_capacity(repeats);
    for iteration in 0..repeats {
        let east = if iteration.is_multiple_of(2) {
            0.4
        } else {
            0.0
        };
        let listener = ApiEnuVector3::new(east, 0.0, 2.0);
        let update = source_update(&[SOURCE], listener);
        periodic_samples.push(measure_worker_application_lag(
            &mut worker,
            &mut render,
            &reflection_epoch,
            update,
            listener,
        ));
    }

    let mut motion_samples = Vec::with_capacity(repeats);
    for iteration in 0..repeats {
        let east = if iteration.is_multiple_of(2) {
            2.0
        } else {
            4.0
        };
        let listener = ApiEnuVector3::new(east, 0.0, 2.0);
        let update = source_update(&[SOURCE], listener);
        motion_samples.push(measure_worker_application_lag(
            &mut worker,
            &mut render,
            &reflection_epoch,
            update,
            listener,
        ));
    }
    let telemetry = worker.telemetry();
    let periodic_median = median_duration(&periodic_samples);
    let periodic_worst = *periodic_samples.iter().max().unwrap();
    let motion_median = median_duration(&motion_samples);
    let motion_worst = *motion_samples.iter().max().unwrap();
    println!(
        "STALENESS setting={}/{}/{:.1}/{} periodic_5hz_subthreshold_median_ms={:.6} periodic_5hz_subthreshold_worst_ms={:.6} motion_25hz_over_threshold_median_ms={:.6} motion_25hz_over_threshold_worst_ms={:.6} worker_reflection_samples={} worker_reflection_failures={} block_deadline_ms={:.6}",
        setting.rays,
        setting.bounces,
        setting.duration_s,
        setting.order,
        duration_ms(periodic_median),
        duration_ms(periodic_worst),
        duration_ms(motion_median),
        duration_ms(motion_worst),
        telemetry.reflections.timings.len(),
        telemetry.reflections.failures,
        BLOCK_DEADLINE_NS as f64 / 1.0e6,
    );
}

#[test]
#[ignore = "release-only evaluation of the 400 m slapback bin"]
fn reflection_budget_long_arrival_diagnostic() {
    require_release();
    const FAR_WALL_X_M: f32 = 400.0;
    const SPEED_OF_SOUND_MPS: f32 = 343.0;
    const LONG_TAIL_RAYS: i32 = 131_072;
    let expected_path_m =
        (FAR_WALL_X_M - SOURCE.east_m).abs() + (FAR_WALL_X_M - LISTENER.east_m).abs();
    let expected_s = expected_path_m / SPEED_OF_SOUND_MPS;
    let window_half_width_s = 0.06;
    println!(
        "LONG_TAIL expected_path_m={expected_path_m:.3} expected_arrival_s={expected_s:.6} window_half_width_s={window_half_width_s:.3}"
    );

    let short = capture_reflection_response(1.0, 1, LONG_TAIL_RAYS, 16, 3.2, FAR_WALL_X_M);
    let long = capture_reflection_response(3.0, 1, LONG_TAIL_RAYS, 16, 3.2, FAR_WALL_X_M);
    let start = ((expected_s - window_half_width_s) * SAMPLE_RATE as f32) as usize;
    let end = ((expected_s + window_half_width_s) * SAMPLE_RATE as f32) as usize;
    let short_window = window_stats(&short, start, end);
    let long_window = window_stats(&long, start, end);
    let long_peak = long
        .iter()
        .copied()
        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
    let peak_index = long[start..end]
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.abs().total_cmp(&right.abs()))
        .map(|(index, _)| start + index)
        .expect("nonempty expected-arrival window");
    let peak_s = peak_index as f64 / SAMPLE_RATE as f64;
    println!(
        "LONG_TAIL duration_s=1.0 ir_samples={} window_start_sample={start} window_end_sample={end} window_energy={:.9e} window_peak={:.9e}",
        SAMPLE_RATE, short_window.0, short_window.1,
    );
    println!(
        "LONG_TAIL duration_s=3.0 ir_samples={} window_start_sample={start} window_end_sample={end} window_energy={:.9e} window_peak={:.9e} peak_sample={peak_index} peak_s={peak_s:.6} response_peak={long_peak:.9e}",
        SAMPLE_RATE * 3,
        long_window.0,
        long_window.1,
    );
    for distance_m in [100.0_f32, 200.0, 400.0] {
        let expected_path_m =
            (distance_m - SOURCE.east_m).abs() + (distance_m - LISTENER.east_m).abs();
        let expected_s = expected_path_m / SPEED_OF_SOUND_MPS;
        let listener_ray_s = (distance_m - LISTENER.east_m).abs() / SPEED_OF_SOUND_MPS;
        let response = if distance_m == FAR_WALL_X_M {
            long.clone()
        } else {
            capture_reflection_response(3.0, 1, LONG_TAIL_RAYS, 16, 3.2, distance_m)
        };
        let start = ((expected_s - window_half_width_s).max(0.0) * SAMPLE_RATE as f32) as usize;
        let end = ((expected_s + window_half_width_s) * SAMPLE_RATE as f32) as usize;
        let (energy, peak) = window_stats(&response, start, end);
        let ray_start =
            ((listener_ray_s - window_half_width_s).max(0.0) * SAMPLE_RATE as f32) as usize;
        let ray_end = ((listener_ray_s + window_half_width_s) * SAMPLE_RATE as f32) as usize;
        let (ray_energy, ray_peak) = window_stats(&response, ray_start, ray_end);
        let global_peak_index = response
            .iter()
            .enumerate()
            .max_by(|(_, left), (_, right)| left.total_cmp(right))
            .map(|(index, _)| index)
            .unwrap();
        println!(
            "LONG_TAIL_DISTANCE reflector_m={distance_m:.1} round_trip_s={expected_s:.6} round_trip_energy={energy:.9e} round_trip_peak={peak:.9e} listener_ray_s={listener_ray_s:.6} listener_ray_energy={ray_energy:.9e} listener_ray_peak={ray_peak:.9e} global_peak_s={:.6} global_peak={:.9e}",
            global_peak_index as f64 / SAMPLE_RATE as f64,
            response[global_peak_index],
        );
    }
    assert!(
        short_window.0 <= f64::EPSILON,
        "1 s convolution unexpectedly retained energy around the 2.3 s arrival: {short_window:?}"
    );
    assert!(
        long_window.0 > 0.0,
        "3 s effect did not expose its allocated tail"
    );
    assert!(
        long_window.1 <= 1.1e-9,
        "400 m return unexpectedly rose above the measured effect floor; reassess the report"
    );
}

#[test]
#[ignore = "release-only sampled Steam Audio reflection bake on the real megablock mesh"]
fn reflection_budget_megablock_bake_sample_diagnostic() {
    require_release();
    let setting = cinematic_setting();
    let package = env::var_os("FIGHTBOX_DIAG_PACKAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox")
        });
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let source = ApiEnuVector3::new(292.5, 292.5, 1.5);
    let descriptors = [crate::MultiSourceDescriptor::at(source)];
    let (simulation, _render) = build_multi_source_generation(
        &mesh,
        None,
        audio_config(),
        reflection_config(
            setting.rays,
            setting.bounces,
            setting.duration_s,
            setting.order,
        ),
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build real megablock bake sample world");
    let probe_batch = simulation.world.probe_batch();
    let probes = [
        ApiEnuVector3::new(292.5, 292.5, 1.5),
        ApiEnuVector3::new(197.5, 292.5, 1.5),
        ApiEnuVector3::new(292.5, 387.5, 1.5),
        ApiEnuVector3::new(387.5, 292.5, 1.5),
        ApiEnuVector3::new(102.5, 197.5, 1.5),
        ApiEnuVector3::new(482.5, 292.5, 1.5),
        ApiEnuVector3::new(292.5, 102.5, 1.5),
        ApiEnuVector3::new(482.5, 482.5, 1.5),
    ];
    for probe in probes {
        ffi::probe_batch_add_probe(
            probe_batch,
            ffi::IPLSphere {
                center: raw_vector(EnuVector3::new(probe.east_m, probe.north_m, probe.up_m)),
                radius: 8.0,
            },
        );
    }
    ffi::probe_batch_commit(probe_batch);
    assert_eq!(
        ffi::probe_batch_get_num_probes(probe_batch),
        probes.len() as i32
    );

    let mut identifier = ffi::IPLBakedDataIdentifier {
        type_: 0,     // IPL_BAKEDDATATYPE_REFLECTIONS
        variation: 1, // IPL_BAKEDDATAVARIATION_STATICSOURCE
        endpointInfluence: ffi::IPLSphere {
            center: raw_vector(EnuVector3::new(source.east_m, source.north_m, source.up_m)),
            radius: 1_000.0,
        },
    };
    let mut params = IPLReflectionsBakeParams {
        scene: handle(simulation.world.scene),
        probe_batch,
        scene_type: ffi::IPL_SCENETYPE_DEFAULT,
        identifier,
        bake_flags: 1, // IPL_REFLECTIONSBAKEFLAGS_BAKECONVOLUTION
        num_rays: setting.rays,
        num_diffuse_samples: 32,
        num_bounces: setting.bounces,
        simulated_duration: setting.duration_s,
        saved_duration: setting.duration_s,
        order: setting.order,
        num_threads: 1,
        ray_batch_size: 64,
        irradiance_min_distance: 1.0,
        bake_batch_size: 1,
        opencl_device: core::ptr::null_mut(),
        radeon_rays_device: core::ptr::null_mut(),
    };
    let progress = AtomicU32::new(0);
    let started = Instant::now();
    unsafe {
        reflections_baker_bake(
            simulation.world.context(),
            &mut params,
            Some(report_bake_progress),
            (&progress as *const AtomicU32).cast_mut().cast(),
        );
    }
    let elapsed = started.elapsed();
    let layer_bytes = ffi::probe_batch_get_data_size(probe_batch, &mut identifier) as u64;
    let per_probe_s = elapsed.as_secs_f64() / probes.len() as f64;
    let estimated_total_s = per_probe_s * MEGABLOCK_PROBE_COUNT as f64;
    let estimated_layer_bytes =
        layer_bytes as f64 / probes.len() as f64 * MEGABLOCK_PROBE_COUNT as f64;
    println!(
        "BAKE_SAMPLE setting={}/{}/{:.1}/{} mesh_vertices={} mesh_triangles={} sample_probes={} sample_elapsed_s={:.6} per_probe_s={per_probe_s:.6} sample_layer_bytes={layer_bytes} estimated_megablock_seconds={estimated_total_s:.3} estimated_megablock_hours={:.3} estimated_layer_gib={:.3} progress_pct={} probe_count={MEGABLOCK_PROBE_COUNT}",
        setting.rays,
        setting.bounces,
        setting.duration_s,
        setting.order,
        mesh.vertices_enu_m.len(),
        mesh.triangles.len(),
        probes.len(),
        elapsed.as_secs_f64(),
        estimated_total_s / 3_600.0,
        estimated_layer_bytes / 1024.0_f64.powi(3),
        progress.load(Ordering::Relaxed),
    );
    assert!(
        layer_bytes > 0,
        "reflection bake produced an empty data layer"
    );
}

#[derive(Clone, Copy, Debug)]
struct ReflectionSetting {
    rays: i32,
    bounces: i32,
    duration_s: f32,
    order: i32,
}

struct DiagnosticRunner {
    simulation: MultiSourceSimulation,
    reflection_epoch: Arc<AtomicU64>,
}

impl SimulationRunner for DiagnosticRunner {
    fn update_inputs(&mut self, update: &SimulationUpdate) {
        self.simulation.update_inputs(update);
    }

    fn run_direct(&mut self) -> Result<(), SimulationError> {
        self.simulation.run_direct()
    }

    fn run_pathing(&mut self) -> Result<(), SimulationError> {
        self.simulation.run_pathing()
    }

    fn run_reflections(&mut self) -> Result<(), SimulationError> {
        let result = self.simulation.run_reflections();
        if result.is_ok() {
            self.reflection_epoch.fetch_add(1, Ordering::Release);
        }
        result
    }
}

fn cinematic_setting() -> ReflectionSetting {
    ReflectionSetting {
        rays: env_i32("FIGHTBOX_DIAG_CINEMATIC_RAYS", 8_192),
        bounces: env_i32("FIGHTBOX_DIAG_CINEMATIC_BOUNCES", 8),
        duration_s: env_f32("FIGHTBOX_DIAG_CINEMATIC_DURATION_S", 3.0),
        order: env_i32("FIGHTBOX_DIAG_CINEMATIC_ORDER", 2),
    }
}

fn audio_config() -> AudioConfig {
    AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    }
}

fn reflection_config(rays: i32, bounces: i32, duration_s: f32, order: i32) -> S3SimulationConfig {
    S3SimulationConfig {
        reflection_rays: rays,
        diffuse_samples: 32,
        reflection_bounces: bounces,
        reflection_duration_s: duration_s,
        reflection_order: order,
        simulation_threads: 1,
        ray_batch_size: 64,
        ..S3SimulationConfig::default()
    }
}

fn require_release() {
    assert!(
        !cfg!(debug_assertions),
        "reflection budget diagnostics must run under cargo test --release"
    );
}

fn earn_full_quality(simulation: &mut MultiSourceSimulation, requested_order: i32) {
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
    }
    let telemetry = simulation.quality_governor_telemetry();
    assert_eq!(telemetry.reflections.level, ReflectionQualityLevel::Full);
    assert_eq!(telemetry.ambisonic_order, requested_order);
    assert_eq!(telemetry.reflections.cadence_divisor, 1);
}

fn run_exact_reflection_tick(
    simulation: &mut MultiSourceSimulation,
    rays: i32,
    bounces: i32,
    duration_s: f32,
    order: i32,
    active_reflection_sources: &[usize],
) -> ExactReflectionOutput {
    let quality = simulation.governor.render_quality();
    let started = Instant::now();
    let mut shared = shared_inputs(simulation.frame.listener, quality).expect("valid listener");
    shared.numRays = rays;
    shared.numBounces = bounces;
    shared.duration = duration_s;
    shared.order = order;
    let input_flags = ffi::IPL_SIMULATIONFLAGS_REFLECTIONS | ffi::IPL_SIMULATIONFLAGS_PATHING;
    ffi::simulator_set_shared_inputs(simulation.world.simulator(), input_flags, &mut shared);
    for index in 0..simulation.world.source_count {
        let enabled = active_reflection_sources.contains(&index);
        let flags = if enabled {
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS
        } else {
            0
        };
        let mut inputs = source_inputs(
            simulation.frame.sources[index],
            simulation.source_directivities[index],
            simulation.source_occlusion_modes[index],
            simulation.world.probe_batch(),
            simulation.config,
            quality,
            flags,
        )
        .expect("valid source inputs");
        inputs.flags = flags;
        ffi::source_set_inputs(simulation.world.source(index), input_flags, &mut inputs);
    }
    ffi::simulator_run_reflections(simulation.world.simulator());
    let reflection = read_reflection_output(simulation, active_reflection_sources[0]);
    let elapsed = started.elapsed();
    ExactReflectionOutput {
        elapsed,
        reflection,
    }
}

fn read_reflection_output(
    simulation: &MultiSourceSimulation,
    source_index: usize,
) -> SteamReflectionParams {
    let mut outputs = ffi::IPLSimulationOutputs::zeroed();
    ffi::source_get_outputs(
        simulation.world.source(source_index),
        ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
        &mut outputs,
    );
    SteamReflectionParams {
        ir: outputs.reflections.ir as usize,
        reverb_times: outputs.reflections.reverbTimes,
        eq: outputs.reflections.eq,
        delay: outputs.reflections.delay,
        num_channels: outputs.reflections.numChannels,
        ir_size: outputs.reflections.irSize,
        tan_slot: outputs.reflections.tanSlot,
    }
}

fn verify_reflection_shape(reflection: SteamReflectionParams, duration_s: f32, order: i32) {
    assert_ne!(reflection.ir, 0, "reflection simulation returned a null IR");
    assert_eq!(
        reflection.num_channels,
        ambisonics_channel_count(order).expect("valid diagnostic order")
    );
    assert_eq!(
        reflection.ir_size,
        reflection_ir_size(duration_s, SAMPLE_RATE).expect("valid diagnostic duration")
    );
}

fn prepare_reflection_inputs(render: &mut MultiSourceRenderGraph) {
    for (index, source) in render.sources.iter_mut().enumerate() {
        let mut samples = (0..BLOCK_FRAMES as usize)
            .map(|frame| {
                let phase = (frame * 17 + index * 31) as f32;
                (phase * 0.071).sin() * 0.1
            })
            .collect::<Vec<_>>();
        source.input.write_mono(&mut samples);
    }
}

fn run_reflection_render_block(
    render: &mut MultiSourceRenderGraph,
    sources: &[SteamSourcePropagation; MAX_ACTIVE_SOURCES],
    source_count: usize,
    order: i32,
    duration_s: f32,
) {
    let reflections = std::array::from_fn(|index| sources[index].reflections);
    run_reflection_render_block_with_params(render, &reflections, source_count, order, duration_s);
}

fn run_mixed_reflection_render_block(
    render: &mut MultiSourceRenderGraph,
    reflections: &[SteamReflectionParams; MAX_ACTIVE_SOURCES],
    mixer_order: i32,
    mixer_duration_s: f32,
) {
    run_reflection_render_block_with_params(render, reflections, 4, mixer_order, mixer_duration_s);
}

fn run_reflection_render_block_with_params(
    render: &mut MultiSourceRenderGraph,
    reflections: &[SteamReflectionParams; MAX_ACTIVE_SOURCES],
    source_count: usize,
    mixer_order: i32,
    mixer_duration_s: f32,
) {
    for (index, reflection) in reflections.iter().copied().enumerate().take(source_count) {
        let state = &mut render.sources[index];
        let mut params = reflection_effect_params(reflection, render.config);
        params.numChannels = reflection.num_channels;
        params.irSize = reflection.ir_size;
        let mut input = state.input.raw();
        let mut scratch = state.reflection_scratch.raw();
        ffi::reflection_effect_apply_to_mixer(
            handle(state.reflection_effect),
            &mut params,
            &mut input,
            &mut scratch,
            handle(render.reflection_mixer),
        );
    }
    let mut mixer_params = ffi::IPLReflectionEffectParams {
        type_: ffi::IPL_REFLECTIONEFFECTTYPE_CONVOLUTION,
        ir: core::ptr::null_mut(),
        reverbTimes: [0.0; 3],
        eq: [1.0; 3],
        delay: 0,
        numChannels: ambisonics_channel_count(mixer_order).expect("valid mixer order"),
        irSize: reflection_ir_size(mixer_duration_s, SAMPLE_RATE).expect("valid mixer duration"),
        tanDevice: core::ptr::null_mut(),
        tanSlot: 0,
    };
    let mut mix = render.reflection_mix.raw();
    ffi::reflection_mixer_apply(handle(render.reflection_mixer), &mut mixer_params, &mut mix);
    let listener = SteamPose::from_api(default_api_pose(LISTENER)).expect("valid listener");
    let mut decode_params = ffi::IPLAmbisonicsDecodeEffectParams {
        order: mixer_order,
        hrtf: handle(render.hrtf),
        orientation: coordinate_space(listener).expect("valid listener orientation"),
        binaural: ffi::IPL_TRUE,
    };
    let mut stereo = render.reflection_stereo.raw();
    ffi::ambisonics_decode_effect_apply(
        handle(render.ambisonics_decode),
        &mut decode_params,
        &mut mix,
        &mut stereo,
    );
    render
        .reflection_stereo
        .read_interleaved(&mut render.stereo_work);
    black_box(render.stereo_work[0]);
}

fn capture_reflection_response(
    duration_s: f32,
    order: i32,
    rays: i32,
    bounces: i32,
    capture_s: f32,
    reflector_distance_m: f32,
) -> Vec<f32> {
    let audio = audio_config();
    let config = reflection_config(rays, bounces, duration_s, order);
    let descriptors = [crate::MultiSourceDescriptor::at(SOURCE)];
    let (mut simulation, render) = build_multi_source_generation(
        &far_reflector_mesh(reflector_distance_m),
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build long-arrival diagnostic session");
    simulation.update_inputs(&source_update(&[SOURCE], LISTENER));
    earn_full_quality(&mut simulation, order);
    simulation
        .run_reflections()
        .expect("simulate long-arrival IR");
    let reflection = simulation.snapshot.sources[0].reflections;
    verify_reflection_shape(reflection, duration_s, order);

    let context = render.world.context();
    let mut audio_settings = raw_audio_settings(audio);
    let mut settings = ffi::IPLReflectionEffectSettings {
        type_: ffi::IPL_REFLECTIONEFFECTTYPE_CONVOLUTION,
        irSize: reflection.ir_size,
        numChannels: reflection.num_channels,
    };
    let mut effect = core::ptr::null_mut();
    assert_eq!(
        ffi::reflection_effect_create(context, &mut audio_settings, &mut settings, &mut effect,),
        ffi::IPL_STATUS_SUCCESS
    );
    let mut input = OwnedAudioBuffer::allocate(context, 1, BLOCK_FRAMES).unwrap();
    let mut output =
        OwnedAudioBuffer::allocate(context, reflection.num_channels, BLOCK_FRAMES).unwrap();
    let blocks = (capture_s * SAMPLE_RATE as f32 / BLOCK_FRAMES as f32).ceil() as usize;
    let mut interleaved = vec![0.0; (reflection.num_channels * BLOCK_FRAMES) as usize];
    let mut response = Vec::with_capacity(blocks * BLOCK_FRAMES as usize);
    for block in 0..blocks {
        let mut mono = vec![0.0; BLOCK_FRAMES as usize];
        if block == 0 {
            mono[0] = 1.0;
        }
        input.write_mono(&mut mono);
        let mut input_raw = input.raw();
        let mut output_raw = output.raw();
        let mut params = reflection_effect_params(reflection, config);
        ffi::reflection_effect_apply(effect, &mut params, &mut input_raw, &mut output_raw);
        output.read_interleaved(&mut interleaved);
        response.extend(
            interleaved
                .chunks_exact(reflection.num_channels as usize)
                .map(|frame| {
                    frame
                        .iter()
                        .copied()
                        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
                }),
        );
    }
    ffi::reflection_effect_release(&mut effect);
    response
}

fn window_stats(response: &[f32], start: usize, end: usize) -> (f64, f32) {
    response[start..end]
        .iter()
        .copied()
        .fold((0.0, 0.0_f32), |(energy, peak), sample| {
            (
                energy + f64::from(sample) * f64::from(sample),
                peak.max(sample.abs()),
            )
        })
}

fn wait_for_initial_reflection(render: &mut MultiSourceRenderGraph) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if render.publication.read().sources[0].reflections.ir != 0 {
            return;
        }
        thread::sleep(Duration::from_millis(1));
    }
    panic!("simulation worker did not publish its initial reflection IR");
}

fn measure_worker_application_lag(
    worker: &mut SimulationWorker,
    render: &mut MultiSourceRenderGraph,
    reflection_epoch: &AtomicU64,
    update: SimulationUpdate,
    target_listener: ApiEnuVector3,
) -> Duration {
    let target = api_enu_to_steam(target_listener);
    let starting_epoch = reflection_epoch.load(Ordering::Acquire);
    let started = Instant::now();
    worker.publish_update(update);
    let mut next_block = started;
    let timeout = started + Duration::from_secs(3);
    let zeros = vec![0.0; BLOCK_FRAMES as usize];
    loop {
        assert!(
            Instant::now() < timeout,
            "timed out waiting for updated reflection IR"
        );
        let now = Instant::now();
        if next_block > now {
            thread::sleep(next_block - now);
        }
        let published = render.publication.read();
        if reflection_epoch.load(Ordering::Acquire) > starting_epoch
            && same_position(published.listener_position, target)
            && published.sources[0].reflections.ir != 0
        {
            render_full_block(render, &zeros);
            return started.elapsed();
        }
        render_full_block(render, &zeros);
        next_block += Duration::from_nanos(BLOCK_DEADLINE_NS);
    }
}

fn render_full_block(render: &mut MultiSourceRenderGraph, input: &[f32]) {
    let sources = [BackendSourceBlock {
        source_index: 0,
        input_mono: input,
    }];
    let mut left = vec![0.0; BLOCK_FRAMES as usize];
    let mut right = vec![0.0; BLOCK_FRAMES as usize];
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
        .expect("render reflection application block");
}

fn source_update(positions: &[ApiEnuVector3], listener: ApiEnuVector3) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    for (index, position) in positions.iter().copied().enumerate() {
        sources[index] = SourceMotion {
            active: true,
            pose: default_api_pose(position),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
    }
    SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: default_api_pose(listener),
            linear_velocity_mps: ApiEnuVector3::default(),
        },
        sources,
    }
}

fn controlled_canyon_mesh() -> SceneMesh {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    add_double_sided_quad(
        &mut vertices,
        &mut triangles,
        [
            EnuVector3::new(-120.0, -260.0, 0.0),
            EnuVector3::new(500.0, -260.0, 0.0),
            EnuVector3::new(500.0, 260.0, 0.0),
            EnuVector3::new(-120.0, 260.0, 0.0),
        ],
    );
    for y in [-30.0, 30.0] {
        add_double_sided_quad(
            &mut vertices,
            &mut triangles,
            [
                EnuVector3::new(-120.0, y, 0.0),
                EnuVector3::new(500.0, y, 0.0),
                EnuVector3::new(500.0, y, 120.0),
                EnuVector3::new(-120.0, y, 120.0),
            ],
        );
    }
    add_double_sided_quad(
        &mut vertices,
        &mut triangles,
        [
            EnuVector3::new(400.0, -260.0, 0.0),
            EnuVector3::new(400.0, 260.0, 0.0),
            EnuVector3::new(400.0, 260.0, 300.0),
            EnuVector3::new(400.0, -260.0, 300.0),
        ],
    );
    add_double_sided_quad(
        &mut vertices,
        &mut triangles,
        [
            EnuVector3::new(-100.0, -100.0, 0.0),
            EnuVector3::new(-100.0, 100.0, 0.0),
            EnuVector3::new(-100.0, 100.0, 160.0),
            EnuVector3::new(-100.0, -100.0, 160.0),
        ],
    );
    SceneMesh {
        material_indices: vec![0; triangles.len()],
        vertices_enu_m: vertices,
        triangles,
        materials: vec![AcousticMaterial::MASONRY],
    }
}

fn far_reflector_mesh(distance_m: f32) -> SceneMesh {
    let mut vertices = Vec::new();
    let mut triangles = Vec::new();
    // A controlled 200 m × 200 m tower facade centered on the source/listener
    // height. Its nearest reflected path is the 799.5 m path asserted by the
    // diagnostic; its finite extent keeps arrivals concentrated near that bin.
    add_double_sided_quad(
        &mut vertices,
        &mut triangles,
        [
            EnuVector3::new(distance_m, -100.0, -98.0),
            EnuVector3::new(distance_m, 100.0, -98.0),
            EnuVector3::new(distance_m, 100.0, 102.0),
            EnuVector3::new(distance_m, -100.0, 102.0),
        ],
    );
    SceneMesh {
        material_indices: vec![0; triangles.len()],
        vertices_enu_m: vertices,
        triangles,
        materials: vec![AcousticMaterial::MASONRY],
    }
}

fn add_double_sided_quad(
    vertices: &mut Vec<EnuVector3>,
    triangles: &mut Vec<[i32; 3]>,
    quad: [EnuVector3; 4],
) {
    let base = vertices.len() as i32;
    vertices.extend(quad);
    triangles.extend([
        [base, base + 1, base + 2],
        [base, base + 2, base + 3],
        [base + 2, base + 1, base],
        [base + 3, base + 2, base],
    ]);
}

fn median_duration(samples: &[Duration]) -> Duration {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn env_usize(name: &str, default: usize) -> usize {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_i32(name: &str, default: i32) -> i32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn env_f32(name: &str, default: f32) -> f32 {
    env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
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

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(read_u32(bytes, offset))
}

#[repr(C)]
struct IPLReflectionsBakeParams {
    scene: ffi::IPLScene,
    probe_batch: ffi::IPLProbeBatch,
    scene_type: i32,
    identifier: ffi::IPLBakedDataIdentifier,
    bake_flags: i32,
    num_rays: i32,
    num_diffuse_samples: i32,
    num_bounces: i32,
    simulated_duration: f32,
    saved_duration: f32,
    order: i32,
    num_threads: i32,
    ray_batch_size: i32,
    irradiance_min_distance: f32,
    bake_batch_size: i32,
    opencl_device: ffi::IPLOpenCLDevice,
    radeon_rays_device: ffi::IPLRadeonRaysDevice,
}

type ProgressCallback = Option<unsafe extern "system" fn(f32, *mut c_void)>;

unsafe extern "system" {
    #[link_name = "iplReflectionsBakerBake"]
    fn reflections_baker_bake(
        context: ffi::IPLContext,
        params: *mut IPLReflectionsBakeParams,
        progress_callback: ProgressCallback,
        user_data: *mut c_void,
    );
}

unsafe extern "system" fn report_bake_progress(progress: f32, user_data: *mut c_void) {
    let percent = (progress.clamp(0.0, 1.0) * 100.0).round() as u32;
    let progress_state = unsafe { &*user_data.cast::<AtomicU32>() };
    let previous = progress_state.swap(percent, Ordering::Relaxed);
    if percent == 100 || percent / 10 > previous / 10 {
        println!("BAKE_PROGRESS percent={percent}");
    }
}
