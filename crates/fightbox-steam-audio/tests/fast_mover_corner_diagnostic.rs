//! Ignored, env-gated measurements for sharp-corner and fast-mover artifacts.
//!
//! This is diagnosis-only. It loads the caller-supplied city package/bake,
//! performs retained offline simulation passes, and writes no capture files.

use fightbox_api::{EnuVector3, ExtentDescriptor, ListenerState, Pose};
use fightbox_runtime::backend::{SimulationRunner, SimulationUpdate, SourceMotion};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, BakedProbeBatch, DirectOcclusionMode, MultiSourceDescriptor,
    PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata, ReflectionEffectConfig, S3SimulationConfig,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, SceneMesh, build_multi_source_session,
};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: f32 = 48_000.0;
const BLOCK_FRAMES: f32 = 128.0;
const SPEED_OF_SOUND_MPS: f32 = 343.0;
const DIRECT_HZ: usize = 60;
const PATH_HZ: usize = 15;
const REFLECTION_MAX_HZ: usize = 25;
const SOURCE: EnuVector3 = EnuVector3::new(102.5, 102.5, 1.5);
const WALK_EAST_M: f32 = 148.5;
const WALK_START_NORTH_M: f32 = 108.0;
const WALK_END_NORTH_M: f32 = 120.0;
const WALK_SPEED_MPS: f32 = 6.0;
const MI8_SPEED_MPS: f32 = 30.0;
const MI8_ALTITUDE_M: f32 = 55.0;
const MI8_LINE_LENGTH_M: f32 = 21.0;
const CHECKPOINT_LISTENER: EnuVector3 = EnuVector3::new(197.5, 292.5, 1.5);

#[derive(Clone, Copy, Debug)]
struct Tick {
    distance_m: f32,
    occlusion: f32,
    path_energy: f32,
    path_eq: [f32; 3],
}

#[derive(Debug)]
struct CornerSweep {
    label: &'static str,
    ticks: Vec<Tick>,
    path_ticks: Vec<Tick>,
    stationary_distinct: usize,
    stationary_min: f32,
    stationary_max: f32,
    stationary_at_north_m: f32,
}

#[test]
#[ignore = "requires FIGHTBOX_DIAG_PACKAGE/FIGHTBOX_DIAG_BAKE and the linked Steam Audio SDK"]
fn fast_mover_corner_measurements() {
    let package = required_env_path("FIGHTBOX_DIAG_PACKAGE");
    let bake = required_env_path("FIGHTBOX_DIAG_BAKE");
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let baked = load_baked(&bake);

    println!(
        "FAST_DIAG input package={} bake={} vertices={} triangles={} probes={}",
        package.display(),
        bake.display(),
        mesh.vertices_enu_m.len(),
        mesh.triangles.len(),
        baked.metadata.probe_count,
    );

    measure_delay_models();
    measure_cadence_staleness();
    measure_probe_envelope(&baked.bytes, baked.metadata.probe_count);

    let point_alternates = corner_sweep(
        &mesh,
        &baked,
        "point-alternates",
        ExtentDescriptor::Point,
        true,
    );
    print_corner_summary(&point_alternates);

    let point_primary = corner_sweep(
        &mesh,
        &baked,
        "point-primary-only",
        ExtentDescriptor::Point,
        false,
    );
    print_corner_summary(&point_primary);
    print_alternate_contrast(&point_alternates, &point_primary);

    let mi8_extent = corner_sweep(
        &mesh,
        &baked,
        "line21-alternates",
        ExtentDescriptor::LineSegment {
            length_m: MI8_LINE_LENGTH_M,
        },
        true,
    );
    print_corner_summary(&mi8_extent);
    print_extent_contrast(&point_alternates, &mi8_extent);

    measure_mi8_waypoint(&mesh, &baked);
}

fn measure_delay_models() {
    let sample_count = (4.0 * 380.0 / MI8_SPEED_MPS * SAMPLE_RATE) as usize;
    let samples_per_tick = (SAMPLE_RATE / DIRECT_HZ as f32) as usize;
    let retention = (-1.0 / (0.080 * SAMPLE_RATE)).exp();
    let mut legacy_delay = None::<f32>;
    let mut actual_applied = None::<f32>;
    let mut actual_target = 0.0_f32;
    let mut actual_anchor = 0.0_f32;
    let mut actual_rate = 0.0_f32;
    let mut held_raw = 0.0_f32;
    let mut legacy_squared_error = 0.0_f64;
    let mut actual_squared_error = 0.0_f64;
    let mut actual_steady_squared_error = 0.0_f64;
    let mut actual_steady_count = 0_usize;
    let mut legacy_max_error = 0.0_f32;
    let mut actual_max_error = 0.0_f32;
    let mut actual_steady_max_error = 0.0_f32;
    let mut max_raw_tick_step = 0.0_f32;
    let mut max_anchor_reset_step = 0.0_f32;
    let mut max_rate_step = 0.0_f32;
    let mut max_applied_step = 0.0_f32;
    let mut max_abs_radial = 0.0_f32;
    let mut min_radial = f32::INFINITY;
    let mut max_radial = f32::NEG_INFINITY;
    let mut previous_raw = None::<f32>;
    let mut previous_source_velocity = None::<EnuVector3>;
    let mut last_turn_sample = None::<usize>;
    let mut recovery_started = None::<usize>;
    let mut maximum_recovery_samples = 0_usize;

    for sample in 0..sample_count {
        if sample % samples_per_tick == 0 {
            let seconds = sample as f32 / SAMPLE_RATE;
            let (source, source_velocity) = square_motion(
                [
                    EnuVector3::new(102.5, 102.5, MI8_ALTITUDE_M),
                    EnuVector3::new(482.5, 102.5, MI8_ALTITUDE_M),
                    EnuVector3::new(482.5, 482.5, MI8_ALTITUDE_M),
                    EnuVector3::new(102.5, 482.5, MI8_ALTITUDE_M),
                ],
                MI8_SPEED_MPS,
                seconds,
            );
            let (listener, listener_velocity) = square_motion(
                [
                    EnuVector3::new(197.5, 292.5, 1.5),
                    EnuVector3::new(292.5, 292.5, 1.5),
                    EnuVector3::new(292.5, 387.5, 1.5),
                    EnuVector3::new(197.5, 387.5, 1.5),
                ],
                1.5,
                seconds,
            );
            let distance_m = distance(source, listener);
            let radial = radial_velocity(source, source_velocity, listener, listener_velocity);
            let raw = distance_m * SAMPLE_RATE / SPEED_OF_SOUND_MPS;
            held_raw = raw;
            let ratio = (1.0 / (1.0 + radial / SPEED_OF_SOUND_MPS)).clamp(2.0 / 3.0, 2.0);
            let new_rate = (1.0 - ratio).clamp(-0.5, 0.5);
            let new_anchor = raw * ratio;
            if actual_applied.is_some() {
                max_anchor_reset_step =
                    max_anchor_reset_step.max((new_anchor - actual_anchor).abs());
                max_rate_step = max_rate_step.max((new_rate - actual_rate).abs());
            }
            if previous_source_velocity.is_some_and(|previous| {
                dot(previous, source_velocity) < MI8_SPEED_MPS * MI8_SPEED_MPS * 0.5
            }) {
                last_turn_sample = Some(sample);
                recovery_started = Some(sample);
            }
            previous_source_velocity = Some(source_velocity);
            actual_rate = new_rate;
            actual_anchor = new_anchor;
            if actual_applied.is_none() {
                actual_applied = Some(actual_anchor);
                actual_target = actual_anchor;
                legacy_delay = Some(raw);
            }
            if let Some(previous) = previous_raw.replace(raw) {
                max_raw_tick_step = max_raw_tick_step.max((raw - previous).abs());
            }
            min_radial = min_radial.min(radial);
            max_radial = max_radial.max(radial);
            max_abs_radial = max_abs_radial.max(radial.abs());
        }

        let seconds = sample as f32 / SAMPLE_RATE;
        let (source, source_velocity) = square_motion(
            [
                EnuVector3::new(102.5, 102.5, MI8_ALTITUDE_M),
                EnuVector3::new(482.5, 102.5, MI8_ALTITUDE_M),
                EnuVector3::new(482.5, 482.5, MI8_ALTITUDE_M),
                EnuVector3::new(102.5, 482.5, MI8_ALTITUDE_M),
            ],
            MI8_SPEED_MPS,
            seconds,
        );
        let (listener, listener_velocity) = square_motion(
            [
                EnuVector3::new(197.5, 292.5, 1.5),
                EnuVector3::new(292.5, 292.5, 1.5),
                EnuVector3::new(292.5, 387.5, 1.5),
                EnuVector3::new(197.5, 387.5, 1.5),
            ],
            1.5,
            seconds,
        );
        let distance_m = distance(source, listener);
        let radial = radial_velocity(source, source_velocity, listener, listener_velocity);
        let raw = distance_m * SAMPLE_RATE / SPEED_OF_SOUND_MPS;
        let exact_ratio = (1.0 / (1.0 + radial / SPEED_OF_SOUND_MPS)).clamp(2.0 / 3.0, 2.0);
        let ideal_reception_delay = raw * exact_ratio;

        let legacy = legacy_delay.as_mut().expect("initialized legacy model");
        *legacy += (held_raw - *legacy).clamp(-0.01, 0.01);
        let legacy_error = *legacy - raw;
        legacy_squared_error += f64::from(legacy_error * legacy_error);
        legacy_max_error = legacy_max_error.max(legacy_error.abs());

        actual_anchor += actual_rate;
        let integrated = actual_target + actual_rate;
        actual_target = actual_anchor + (integrated - actual_anchor) * retention;
        let applied = actual_applied.as_mut().expect("initialized actual model");
        let previous_applied = *applied;
        let one_pole = actual_target + (*applied - actual_target) * retention;
        *applied += (one_pole - *applied).clamp(-0.5, 0.5);
        max_applied_step = max_applied_step.max((*applied - previous_applied).abs());
        let actual_error = *applied - ideal_reception_delay;
        actual_squared_error += f64::from(actual_error * actual_error);
        actual_max_error = actual_max_error.max(actual_error.abs());
        let away_from_turn =
            last_turn_sample.is_none_or(|turn| sample.saturating_sub(turn) >= SAMPLE_RATE as usize);
        if away_from_turn {
            actual_steady_squared_error += f64::from(actual_error * actual_error);
            actual_steady_count += 1;
            actual_steady_max_error = actual_steady_max_error.max(actual_error.abs());
        }
        if let Some(start) = recovery_started
            && actual_error.abs() <= SAMPLE_RATE / 1_000.0
        {
            maximum_recovery_samples = maximum_recovery_samples.max(sample - start);
            recovery_started = None;
        }
    }

    let legacy_rms = (legacy_squared_error / sample_count as f64).sqrt();
    let actual_rms = (actual_squared_error / sample_count as f64).sqrt();
    let actual_steady_rms =
        (actual_steady_squared_error / actual_steady_count.max(1) as f64).sqrt();
    let receding_ratio = 1.0 / (1.0 + MI8_SPEED_MPS / SPEED_OF_SOUND_MPS);
    let approaching_ratio = 1.0 / (1.0 - MI8_SPEED_MPS / SPEED_OF_SOUND_MPS);
    println!(
        "FAST_DIAG H1 orbit_s={:.3} samples={} radial_mps=[{:.3},{:.3}] max_abs_radial_mps={:.3} raw_60hz_step_max_samples={:.3} legacy_cap_per_tick_samples={:.3} legacy_error_rms_samples={:.3} legacy_error_max_samples={:.3} actual_model_error_rms_samples={:.3} actual_model_error_max_samples={:.3} actual_steady_error_rms_samples={:.3} actual_steady_error_max_samples={:.3} max_anchor_reset_step_samples={:.3} max_rate_step={:.6} max_applied_step={:.6} turn_recovery_to_1ms_max_ms={:.3}",
        sample_count as f32 / SAMPLE_RATE,
        sample_count,
        min_radial,
        max_radial,
        max_abs_radial,
        max_raw_tick_step,
        0.01 * samples_per_tick as f32,
        legacy_rms,
        legacy_max_error,
        actual_rms,
        actual_max_error,
        actual_steady_rms,
        actual_steady_max_error,
        max_anchor_reset_step,
        max_rate_step,
        max_applied_step,
        maximum_recovery_samples as f32 * 1_000.0 / SAMPLE_RATE,
    );
    println!(
        "FAST_DIAG H1 extrema receding_rate_samples_per_sample={:.6} receding_1khz={:.3} approaching_rate_samples_per_sample={:.6} approaching_1khz={:.3} actual_cap=0.5 legacy_cap=0.01",
        1.0 - receding_ratio,
        1_000.0 * receding_ratio,
        1.0 - approaching_ratio,
        1_000.0 * approaching_ratio,
    );
}

fn measure_cadence_staleness() {
    for speed in [30.0_f32, 150.0, 250.0] {
        let direct_m = speed / DIRECT_HZ as f32;
        let path_m = speed / PATH_HZ as f32;
        let reflections_m = speed / REFLECTION_MAX_HZ as f32;
        println!(
            "FAST_DIAG H3 speed_mps={speed:.1} direct_max_age_ms={:.3} direct_travel_m={direct_m:.3} path_max_age_ms={:.3} path_travel_m={path_m:.3} reflection_fast_max_age_ms={:.3} reflection_travel_m={reflections_m:.3}",
            1_000.0 / DIRECT_HZ as f32,
            1_000.0 / PATH_HZ as f32,
            1_000.0 / REFLECTION_MAX_HZ as f32,
        );
    }
}

fn corner_sweep(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    label: &'static str,
    extent: ExtentDescriptor,
    alternates: bool,
) -> CornerSweep {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK_FRAMES as i32,
    };
    let config = diagnostic_config(alternates);
    let descriptor = [MultiSourceDescriptor::at(SOURCE).with_extent(extent)];
    let (mut simulation, _render) =
        build_multi_source_session(mesh, baked, audio, config, &descriptor)
            .expect("build corner diagnostic session");
    recover_governor(&mut simulation, label);

    let direct_step_m = WALK_SPEED_MPS / DIRECT_HZ as f32;
    let tick_count = ((WALK_END_NORTH_M - WALK_START_NORTH_M) / direct_step_m).round() as usize;
    let path_stride = DIRECT_HZ / PATH_HZ;
    let mut ticks = Vec::with_capacity(tick_count + 1);
    let mut path_ticks = Vec::with_capacity(tick_count / path_stride + 1);
    for index in 0..=tick_count {
        let listener = EnuVector3::new(
            WALK_EAST_M,
            WALK_START_NORTH_M + index as f32 * direct_step_m,
            1.5,
        );
        simulation.update_inputs(&one_source_update(
            SOURCE,
            EnuVector3::default(),
            EnuVector3::new(0.0, 1.0, 0.0),
            listener,
        ));
        simulation.run_direct().expect("corner direct pass");
        let direct = simulation
            .source_diagnostics(0)
            .expect("source diagnostics");
        let mut tick = Tick {
            distance_m: listener.north_m,
            occlusion: direct.occlusion,
            path_energy: direct.path_sh_energy,
            path_eq: direct.path_eq,
        };
        if index % path_stride == 0 {
            simulation.run_pathing().expect("corner path pass");
            let path = simulation
                .source_diagnostics(0)
                .expect("source diagnostics");
            tick.path_energy = path.path_sh_energy;
            tick.path_eq = path.path_eq;
            path_ticks.push(tick);
        }
        ticks.push(tick);
    }

    let partial = ticks
        .iter()
        .copied()
        .min_by(|left, right| {
            (left.occlusion - 0.5)
                .abs()
                .total_cmp(&(right.occlusion - 0.5).abs())
        })
        .expect("non-empty corner sweep");
    let listener = EnuVector3::new(WALK_EAST_M, partial.distance_m, 1.5);
    simulation.update_inputs(&one_source_update(
        SOURCE,
        EnuVector3::default(),
        EnuVector3::new(0.0, 1.0, 0.0),
        listener,
    ));
    let mut stationary = Vec::with_capacity(256);
    for _ in 0..256 {
        simulation.run_direct().expect("stationary direct repeat");
        stationary.push(
            simulation
                .source_diagnostics(0)
                .expect("source diagnostics")
                .occlusion,
        );
    }
    let distinct = stationary
        .iter()
        .map(|value| value.to_bits())
        .collect::<BTreeSet<_>>()
        .len();
    let stationary_min = stationary.iter().copied().min_by(f32::total_cmp).unwrap();
    let stationary_max = stationary.iter().copied().max_by(f32::total_cmp).unwrap();

    CornerSweep {
        label,
        ticks,
        path_ticks,
        stationary_distinct: distinct,
        stationary_min,
        stationary_max,
        stationary_at_north_m: partial.distance_m,
    }
}

fn print_corner_summary(sweep: &CornerSweep) {
    let max_occlusion_step = max_adjacent(&sweep.ticks, |tick| tick.occlusion);
    let max_path_energy_step = max_adjacent(&sweep.path_ticks, |tick| tick.path_energy);
    let path_zero_transitions = zero_transitions(&sweep.path_ticks, |tick| tick.path_energy);
    let path_direction_changes = direction_changes(&sweep.path_ticks, |tick| tick.path_energy);
    let occlusion_min = sweep
        .ticks
        .iter()
        .map(|tick| tick.occlusion)
        .min_by(f32::total_cmp)
        .unwrap();
    let occlusion_max = sweep
        .ticks
        .iter()
        .map(|tick| tick.occlusion)
        .max_by(f32::total_cmp)
        .unwrap();
    println!(
        "FAST_DIAG CORNER label={} direct_ticks={} path_ticks={} occlusion=[{:.9},{:.9}] max_direct_tick_step={:.9} max_path_energy_tick_step={:.9e} path_zero_transitions={} path_derivative_sign_changes={} stationary_north_m={:.3} stationary_distinct_bits={} stationary_range={:.9e}",
        sweep.label,
        sweep.ticks.len(),
        sweep.path_ticks.len(),
        occlusion_min,
        occlusion_max,
        max_occlusion_step,
        max_path_energy_step,
        path_zero_transitions,
        path_direction_changes,
        sweep.stationary_at_north_m,
        sweep.stationary_distinct,
        sweep.stationary_max - sweep.stationary_min,
    );
    for tick in &sweep.path_ticks {
        println!(
            "FAST_DIAG CORNER_ROW label={} north_m={:.3} occlusion={:.9} path_energy={:.9e} path_eq={:.6}/{:.6}/{:.6}",
            sweep.label,
            tick.distance_m,
            tick.occlusion,
            tick.path_energy,
            tick.path_eq[0],
            tick.path_eq[1],
            tick.path_eq[2],
        );
    }
}

fn print_alternate_contrast(alternates: &CornerSweep, primary: &CornerSweep) {
    assert_eq!(alternates.path_ticks.len(), primary.path_ticks.len());
    let mut changed = 0_usize;
    let mut max_energy_delta = 0.0_f32;
    let mut max_eq_delta = 0.0_f32;
    for (left, right) in alternates.path_ticks.iter().zip(&primary.path_ticks) {
        let energy_delta = (left.path_energy - right.path_energy).abs();
        let eq_delta = left
            .path_eq
            .into_iter()
            .zip(right.path_eq)
            .map(|(left, right)| (left - right).abs())
            .fold(0.0_f32, f32::max);
        if energy_delta > 1.0e-8 || eq_delta > 1.0e-6 {
            changed += 1;
        }
        max_energy_delta = max_energy_delta.max(energy_delta);
        max_eq_delta = max_eq_delta.max(eq_delta);
    }
    println!(
        "FAST_DIAG H2 alternate_contrast path_ticks={} changed_ticks={} max_path_energy_delta={:.9e} max_path_eq_delta={:.9e}",
        alternates.path_ticks.len(),
        changed,
        max_energy_delta,
        max_eq_delta,
    );
}

fn print_extent_contrast(point: &CornerSweep, line: &CornerSweep) {
    assert_eq!(point.ticks.len(), line.ticks.len());
    let point_step = max_adjacent(&point.ticks, |tick| tick.occlusion);
    let line_step = max_adjacent(&line.ticks, |tick| tick.occlusion);
    println!(
        "FAST_DIAG H4 extent_contrast point_max_step={point_step:.9} line21_max_step={line_step:.9} line_over_point={:.6}",
        line_step / point_step.max(f32::MIN_POSITIVE),
    );
}

fn measure_mi8_waypoint(mesh: &SceneMesh, baked: &BakedProbeBatch) {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE as i32,
        frame_size: BLOCK_FRAMES as i32,
    };
    let descriptor = [
        MultiSourceDescriptor::at(EnuVector3::new(422.5, 102.5, MI8_ALTITUDE_M)).with_extent(
            ExtentDescriptor::LineSegment {
                length_m: MI8_LINE_LENGTH_M,
            },
        ),
    ];
    let (mut simulation, _render) =
        build_multi_source_session(mesh, baked, audio, diagnostic_config(true), &descriptor)
            .expect("build Mi-8 waypoint session");
    recover_governor(&mut simulation, "mi8-waypoint");

    let mut direct = Vec::new();
    let mut path = Vec::new();
    for tick in 0..=240 {
        let t = -2.0 + tick as f32 / DIRECT_HZ as f32;
        let (position, velocity, forward) = if t < 0.0 {
            (
                EnuVector3::new(482.5 + MI8_SPEED_MPS * t, 102.5, MI8_ALTITUDE_M),
                EnuVector3::new(MI8_SPEED_MPS, 0.0, 0.0),
                EnuVector3::new(1.0, 0.0, 0.0),
            )
        } else {
            (
                EnuVector3::new(482.5, 102.5 + MI8_SPEED_MPS * t, MI8_ALTITUDE_M),
                EnuVector3::new(0.0, MI8_SPEED_MPS, 0.0),
                EnuVector3::new(0.0, 1.0, 0.0),
            )
        };
        simulation.update_inputs(&one_source_update(
            position,
            velocity,
            forward,
            CHECKPOINT_LISTENER,
        ));
        simulation.run_direct().expect("Mi-8 direct pass");
        let diagnostics = simulation.source_diagnostics(0).unwrap();
        direct.push(Tick {
            distance_m: t,
            occlusion: diagnostics.occlusion,
            path_energy: diagnostics.path_sh_energy,
            path_eq: diagnostics.path_eq,
        });
        if tick % (DIRECT_HZ / PATH_HZ) == 0 {
            simulation.run_pathing().expect("Mi-8 path pass");
            let diagnostics = simulation.source_diagnostics(0).unwrap();
            path.push(Tick {
                distance_m: t,
                occlusion: diagnostics.occlusion,
                path_energy: diagnostics.path_sh_energy,
                path_eq: diagnostics.path_eq,
            });
        }
    }

    println!(
        "FAST_DIAG MI8_WAYPOINT direct_ticks={} path_ticks={} occlusion_range={:.9} max_direct_step={:.9} max_path_energy_step={:.9e} path_zero_transitions={} path_derivative_sign_changes={} heading_step_degrees=90",
        direct.len(),
        path.len(),
        value_range(&direct, |tick| tick.occlusion),
        max_adjacent(&direct, |tick| tick.occlusion),
        max_adjacent(&path, |tick| tick.path_energy),
        zero_transitions(&path, |tick| tick.path_energy),
        direction_changes(&path, |tick| tick.path_energy),
    );
    for tick in path.iter().filter(|tick| tick.distance_m.abs() <= 0.4) {
        println!(
            "FAST_DIAG MI8_ROW relative_s={:+.3} occlusion={:.9} path_energy={:.9e} path_eq={:.6}/{:.6}/{:.6}",
            tick.distance_m,
            tick.occlusion,
            tick.path_energy,
            tick.path_eq[0],
            tick.path_eq[1],
            tick.path_eq[2],
        );
    }
}

fn diagnostic_config(alternates: bool) -> S3SimulationConfig {
    S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Volumetric {
            radius_m: 1.0,
            sample_count: 64,
        },
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 3,
        reflection_duration_s: 1.5,
        reflection_order: 1,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        pathing_visibility_range_m: 10.0,
        validate_paths: true,
        find_alternate_paths: alternates,
        ..S3SimulationConfig::default()
    }
}

fn recover_governor(
    simulation: &mut fightbox_steam_audio::SteamAudioSimulationRunner,
    label: &str,
) {
    for _ in 0..30_000 {
        simulation.observe_render_timing(100_000);
        if simulation
            .quality_governor_telemetry()
            .is_some_and(|telemetry| telemetry.ladder_position == 0)
        {
            return;
        }
    }
    println!(
        "FAST_DIAG GOVERNOR label={label} highest_deliverable={:?}",
        simulation.quality_governor_telemetry()
    );
}

fn one_source_update(
    source: EnuVector3,
    source_velocity: EnuVector3,
    source_forward: EnuVector3,
    listener: EnuVector3,
) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); fightbox_runtime::MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: pose(source, source_forward),
        linear_velocity_mps: source_velocity,
    };
    SimulationUpdate {
        listener: ListenerState {
            pose: pose(listener, EnuVector3::new(1.0, 0.0, 0.0)),
            linear_velocity_mps: EnuVector3::default(),
        },
        sources,
    }
}

fn pose(position: EnuVector3, forward: EnuVector3) -> Pose {
    Pose {
        position,
        forward,
        up: EnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn max_adjacent(values: &[Tick], field: impl Fn(&Tick) -> f32) -> f32 {
    values
        .windows(2)
        .map(|pair| (field(&pair[1]) - field(&pair[0])).abs())
        .fold(0.0_f32, f32::max)
}

fn value_range(values: &[Tick], field: impl Fn(&Tick) -> f32) -> f32 {
    let min = values.iter().map(&field).min_by(f32::total_cmp).unwrap();
    let max = values.iter().map(field).max_by(f32::total_cmp).unwrap();
    max - min
}

fn zero_transitions(values: &[Tick], field: impl Fn(&Tick) -> f32) -> usize {
    values
        .windows(2)
        .filter(|pair| (field(&pair[0]) <= 1.0e-12) != (field(&pair[1]) <= 1.0e-12))
        .count()
}

fn direction_changes(values: &[Tick], field: impl Fn(&Tick) -> f32) -> usize {
    values
        .windows(3)
        .filter(|window| {
            let first = field(&window[1]) - field(&window[0]);
            let second = field(&window[2]) - field(&window[1]);
            first.abs() > 1.0e-12 && second.abs() > 1.0e-12 && first.signum() != second.signum()
        })
        .count()
}

fn square_motion(
    waypoints: [EnuVector3; 4],
    speed_mps: f32,
    seconds: f32,
) -> (EnuVector3, EnuVector3) {
    let lengths = [
        distance(waypoints[0], waypoints[1]),
        distance(waypoints[1], waypoints[2]),
        distance(waypoints[2], waypoints[3]),
        distance(waypoints[3], waypoints[0]),
    ];
    let cycle: f32 = lengths.iter().sum();
    let mut traveled = (seconds * speed_mps).rem_euclid(cycle);
    for index in 0..4 {
        if traveled < lengths[index] {
            let start = waypoints[index];
            let end = waypoints[(index + 1) % 4];
            let direction = scale(subtract(end, start), lengths[index].recip());
            return (
                add(start, scale(direction, traveled)),
                scale(direction, speed_mps),
            );
        }
        traveled -= lengths[index];
    }
    unreachable!()
}

fn radial_velocity(
    source: EnuVector3,
    source_velocity: EnuVector3,
    listener: EnuVector3,
    listener_velocity: EnuVector3,
) -> f32 {
    let offset = subtract(source, listener);
    let length = vector_length(offset);
    if length <= 1.0e-9 {
        return 0.0;
    }
    let relative = subtract(source_velocity, listener_velocity);
    dot(relative, scale(offset, length.recip()))
}

fn distance(left: EnuVector3, right: EnuVector3) -> f32 {
    vector_length(subtract(left, right))
}

fn vector_length(vector: EnuVector3) -> f32 {
    dot(vector, vector).sqrt()
}

fn dot(left: EnuVector3, right: EnuVector3) -> f32 {
    left.east_m * right.east_m + left.north_m * right.north_m + left.up_m * right.up_m
}

fn add(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m + right.east_m,
        left.north_m + right.north_m,
        left.up_m + right.up_m,
    )
}

fn subtract(left: EnuVector3, right: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn scale(vector: EnuVector3, amount: f32) -> EnuVector3 {
    EnuVector3::new(
        vector.east_m * amount,
        vector.north_m * amount,
        vector.up_m * amount,
    )
}

fn measure_probe_envelope(bytes: &[u8], expected_count: u32) {
    let table = read_u32(bytes, 0) as usize;
    let vtable = table - read_i32(bytes, table) as usize;
    let field_offset = read_u16(bytes, vtable + 4) as usize;
    let vector_reference = table + field_offset;
    let vector = vector_reference + read_u32(bytes, vector_reference) as usize;
    let count = read_u32(bytes, vector) as usize;
    assert_eq!(count, expected_count as usize);
    let offset = vector + 4;
    let mut center_max = f32::NEG_INFINITY;
    let mut radius_min = f32::INFINITY;
    let mut radius_max = f32::NEG_INFINITY;
    let mut influence_top = f32::NEG_INFINITY;
    for index in 0..count {
        let sphere = offset + index * 16;
        let center_up = read_f32(bytes, sphere + 4);
        let radius = read_f32(bytes, sphere + 12);
        center_max = center_max.max(center_up);
        radius_min = radius_min.min(radius);
        radius_max = radius_max.max(radius);
        influence_top = influence_top.max(center_up + radius);
    }
    println!(
        "FAST_DIAG PROBES count={count} center_up_max_m={center_max:.3} radius_m=[{radius_min:.3},{radius_max:.3}] influence_top_max_m={influence_top:.3} coverage_at_100m=false coverage_at_150m=false coverage_at_300m=false"
    );
}

fn required_env_path(variable: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{variable} must name the diagnostic input"))
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
    json_field_tail(json, field)
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|_| panic!("numeric {field}"))
}

fn json_string(json: &str, field: &str) -> String {
    json_field_tail(json, field)
        .trim_start()
        .strip_prefix('"')
        .unwrap_or_else(|| panic!("string {field}"))
        .split('"')
        .next()
        .unwrap()
        .to_owned()
}

fn json_field_tail<'a>(json: &'a str, field: &str) -> &'a str {
    let needle = format!("\"{field}\":");
    json.split_once(&needle)
        .unwrap_or_else(|| panic!("metadata field {field}"))
        .1
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(read_u32(bytes, offset))
}
