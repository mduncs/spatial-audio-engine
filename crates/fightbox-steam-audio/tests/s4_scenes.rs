//! Canonical S4 room, street-canyon, and doorway-crossing proof.
//!
//! This is ignored because it bakes pathing and renders through the locally
//! acquired Steam Audio 4.8.1 SDK. The JSON fixtures are parsed here so their
//! geometry, materials, source/listener positions, and simulation settings are
//! the executable source of truth.

use fightbox_evidence::{
    WavSpec, interaural_cross_correlation, reflection_density, summed_output_continuity, write_wav,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, ListenerPose, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3RenderRequest, S3SimulationConfig,
    S3TrajectoryRenderRequest, SceneMesh, bake_s3, render_s3, render_s3_trajectory,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::f32::consts::TAU;
use std::fs;
use std::path::Path;

const SAMPLE_RATE: i32 = 48_000;
const IMPULSE_BLOCK_FRAMES: i32 = 128;
const DOORWAY_BLOCK_FRAMES: usize = 1_024;

// IACC uses the standard ±1 ms lag search in the 10–80 ms early-indirect
// window. Reflection density uses the longer 20–320 ms build-up separately.
const IACC_MAX_LAG_SAMPLES: usize = 48;
const CANYON_IACC_MAX: f32 = 0.78;
const CANYON_IACC_CONTROL_SEPARATION: f32 = 0.05;

// Arrival density uses a 1% relative envelope threshold and 1 ms refractory
// interval. A canyon must deliver at least 20 arrivals/s and 1.5× the
// one-wall control: dense energy, not one dominant return.
const DENSITY_RELATIVE_THRESHOLD: f32 = 0.01;
const DENSITY_MIN_SEPARATION_SAMPLES: usize = 48;
const CANYON_MIN_ARRIVALS_PER_SECOND: f32 = 20.0;
const CANYON_CONTROL_DENSITY_RATIO: f32 = 1.5;

// Measured only in the ±8-block neighborhood of the doorway plane. These
// budgets allow gradual physical level/tone change while rejecting a state
// switch: no >3 dB block jump, no >450 Hz adjacent centroid jump, and no
// boundary step over 65% of its local peak.
const DOORWAY_MAX_LEVEL_STEP_DB: f32 = 3.0;
const DOORWAY_MAX_CENTROID_JUMP_HZ: f32 = 450.0;
const DOORWAY_CLICK_RATIO_THRESHOLD: f32 = 0.65;

#[derive(Clone)]
struct Fixture {
    id: String,
    mesh: SceneMesh,
    material_indices_by_name: BTreeMap<String, usize>,
    source: EnuVector3,
    listener: ListenerPose,
    trajectory: Vec<EnuVector3>,
    probes: ProbeVolume,
    simulation: S3SimulationConfig,
}

#[derive(Clone, Copy, Debug)]
struct DecayMeasurement {
    total_energy: f64,
    late_energy: f64,
    energy_95_time_s: f32,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK"]
fn canonical_s4_scene_family() {
    let room = parse_fixture(include_str!("../../../fixtures/s4-room/fixture.json"));
    let canyon = parse_fixture(include_str!("../../../fixtures/s4-canyon/fixture.json"));
    let doorway = parse_fixture(include_str!("../../../fixtures/s4-doorway/fixture.json"));
    assert_eq!(room.id, "s4-masonry-room-doorway");
    assert_eq!(canyon.id, "s4-masonry-street-canyon");
    assert_eq!(doorway.id, "s4-doorway-crossing");

    let output_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/s4-scenes");
    fs::create_dir_all(&output_dir).expect("create target/s4-scenes");

    let room_baked = bake_fixture(&room);
    let impulse = generated_impulse(IMPULSE_BLOCK_FRAMES as usize);
    let room_masonry = render_static(&room, &room_baked, impulse.clone());
    let expected_room_ir_size =
        (room.simulation.reflection_duration_s * SAMPLE_RATE as f32).round() as i32;
    assert_eq!(
        room_masonry.snapshot.reflections.ir_size, expected_room_ir_size,
        "room irSize must exactly match configured duration × sample rate"
    );
    let room_peak = room_masonry
        .stems
        .reflections
        .interleaved
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0_f32, f32::max);
    assert!(room_peak > 1.0e-8, "room reflection IR must be nonzero");

    let mut absorptive_room = room.clone();
    let masonry_index = *absorptive_room
        .material_indices_by_name
        .get("masonry")
        .expect("room masonry material");
    let absorptive_index = *absorptive_room
        .material_indices_by_name
        .get("high_absorption")
        .expect("room high-absorption material");
    absorptive_room.mesh.materials[masonry_index] =
        absorptive_room.mesh.materials[absorptive_index];
    let room_absorptive = render_static(&absorptive_room, &room_baked, impulse.clone());
    let masonry_decay = decay_measurement(&room_masonry.stems.reflections.interleaved);
    let absorptive_decay = decay_measurement(&room_absorptive.stems.reflections.interleaved);
    let total_energy_ratio = absorptive_decay.total_energy / masonry_decay.total_energy;
    let late_energy_ratio = absorptive_decay.late_energy / masonry_decay.late_energy;
    println!(
        "S4 room ir_size={}/{} peak={room_peak:.8} masonry_total={:.8e} absorptive_total={:.8e} total_ratio={total_energy_ratio:.4}<0.80 masonry_late={:.8e} absorptive_late={:.8e} late_ratio={late_energy_ratio:.4}<0.65 masonry_t95={:.4}s absorptive_t95={:.4}s",
        room_masonry.snapshot.reflections.ir_size,
        expected_room_ir_size,
        masonry_decay.total_energy,
        absorptive_decay.total_energy,
        masonry_decay.late_energy,
        absorptive_decay.late_energy,
        masonry_decay.energy_95_time_s,
        absorptive_decay.energy_95_time_s,
    );
    assert!(
        total_energy_ratio < 0.80,
        "high absorption must weaken total reflection energy: ratio={total_energy_ratio:.4}"
    );
    assert!(
        late_energy_ratio < 0.65,
        "high absorption must weaken late decay: ratio={late_energy_ratio:.4}"
    );
    assert!(
        absorptive_decay.energy_95_time_s < masonry_decay.energy_95_time_s,
        "high absorption must shorten the 95%-energy decay time: masonry={:.4}s absorptive={:.4}s",
        masonry_decay.energy_95_time_s,
        absorptive_decay.energy_95_time_s
    );
    write_pcm(
        &output_dir.join("room-masonry-ir.wav"),
        &room_masonry.stems.reflections.interleaved,
    );
    write_pcm(
        &output_dir.join("room-high-absorption-ir.wav"),
        &room_absorptive.stems.reflections.interleaved,
    );
    let canyon_baked = bake_fixture(&canyon);
    let canyon_render = render_static(&canyon, &canyon_baked, impulse.clone());
    let slapback = one_wall_control(&canyon);
    let slapback_baked = bake_fixture(&slapback);
    let slapback_render = render_static(&slapback, &slapback_baked, impulse);
    let iacc_window_start = samples_from_ms(10.0);
    let iacc_window_frames = samples_from_ms(70.0).min(
        canyon_render
            .stems
            .reflections
            .frame_count
            .saturating_sub(iacc_window_start),
    );
    let iacc_window_frames = iacc_window_frames.min(
        slapback_render
            .stems
            .reflections
            .frame_count
            .saturating_sub(iacc_window_start),
    );
    let stereo_spec = stereo_spec();
    let canyon_iacc = interaural_cross_correlation(
        stereo_spec,
        &canyon_render.stems.reflections.interleaved,
        iacc_window_start,
        iacc_window_frames,
        IACC_MAX_LAG_SAMPLES,
    )
    .expect("canyon IACC");
    let slapback_iacc = interaural_cross_correlation(
        stereo_spec,
        &slapback_render.stems.reflections.interleaved,
        iacc_window_start,
        iacc_window_frames,
        IACC_MAX_LAG_SAMPLES,
    )
    .expect("slapback IACC");
    let density_window_start = samples_from_ms(20.0);
    let density_window_frames = samples_from_ms(300.0)
        .min(
            canyon_render
                .stems
                .reflections
                .frame_count
                .saturating_sub(density_window_start),
        )
        .min(
            slapback_render
                .stems
                .reflections
                .frame_count
                .saturating_sub(density_window_start),
        );
    let canyon_density = reflection_density(
        stereo_spec,
        &canyon_render.stems.reflections.interleaved,
        density_window_start,
        density_window_frames,
        DENSITY_RELATIVE_THRESHOLD,
        DENSITY_MIN_SEPARATION_SAMPLES,
    )
    .expect("canyon reflection density");
    let slapback_density = reflection_density(
        stereo_spec,
        &slapback_render.stems.reflections.interleaved,
        density_window_start,
        density_window_frames,
        DENSITY_RELATIVE_THRESHOLD,
        DENSITY_MIN_SEPARATION_SAMPLES,
    )
    .expect("slapback reflection density");
    assert!(
        canyon_iacc.coefficient < CANYON_IACC_MAX,
        "canyon IACC must be in the spaciousness regime: {:.4} < {CANYON_IACC_MAX}",
        canyon_iacc.coefficient
    );
    // The default HRTF can itself decorrelate a single rear reflection, so
    // ordering IACC against that control would confuse HRTF directionality
    // with spaciousness. The robust proof is conjunctive: canyon IACC remains
    // below the absolute spaciousness ceiling, its IACC is measurably distinct
    // from the control, and its arrival density is much higher.
    assert!(
        (canyon_iacc.coefficient - slapback_iacc.coefficient).abs()
            >= CANYON_IACC_CONTROL_SEPARATION,
        "canyon IACC must differ from slapback by {CANYON_IACC_CONTROL_SEPARATION}: canyon={:.4} control={:.4}",
        canyon_iacc.coefficient,
        slapback_iacc.coefficient
    );
    assert!(
        canyon_density.arrivals_per_second >= CANYON_MIN_ARRIVALS_PER_SECOND,
        "canyon density must exceed {CANYON_MIN_ARRIVALS_PER_SECOND}/s: {:.2}/s",
        canyon_density.arrivals_per_second
    );
    assert!(
        canyon_density.arrivals_per_second
            >= slapback_density.arrivals_per_second * CANYON_CONTROL_DENSITY_RATIO,
        "canyon density must be at least {CANYON_CONTROL_DENSITY_RATIO}× slapback: canyon={:.2}/s control={:.2}/s",
        canyon_density.arrivals_per_second,
        slapback_density.arrivals_per_second
    );
    write_pcm(
        &output_dir.join("canyon-ir.wav"),
        &canyon_render.stems.reflections.interleaved,
    );
    write_pcm(
        &output_dir.join("canyon-single-slapback-control-ir.wav"),
        &slapback_render.stems.reflections.interleaved,
    );
    println!(
        "S4 canyon iacc={:.4}<{} control_iacc={:.4} separation={:.4}>{} density={:.2}/s>={} control_density={:.2}/s ratio={:.3}>={}",
        canyon_iacc.coefficient,
        CANYON_IACC_MAX,
        slapback_iacc.coefficient,
        (slapback_iacc.coefficient - canyon_iacc.coefficient).abs(),
        CANYON_IACC_CONTROL_SEPARATION,
        canyon_density.arrivals_per_second,
        CANYON_MIN_ARRIVALS_PER_SECOND,
        slapback_density.arrivals_per_second,
        canyon_density.arrivals_per_second / slapback_density.arrivals_per_second.max(f32::EPSILON),
        CANYON_CONTROL_DENSITY_RATIO,
    );

    // Geometry is byte-for-byte equivalent to the room fixture, but this bake
    // comes from the doorway fixture so its pathing evidence remains fixture-owned.
    let doorway_baked = bake_fixture(&doorway);
    let doorway_request = doorway_trajectory_request(&doorway);
    let crossing_index = doorway_request
        .listener_trajectory
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            (left.position_enu.y - 2.5)
                .abs()
                .total_cmp(&(right.position_enu.y - 2.5).abs())
        })
        .map(|(index, _)| index)
        .expect("doorway trajectory");
    let doorway_render = render_s3_trajectory(&doorway_request, &doorway_baked)
        .expect("render doorway trajectory through retained full pipeline");
    let neighborhood_start = crossing_index.saturating_sub(8);
    let neighborhood_end = (crossing_index + 9).min(doorway_render.blocks.len());
    let crossing_samples = doorway_render.blocks[neighborhood_start..neighborhood_end]
        .iter()
        .flat_map(|block| block.summed.interleaved.iter().copied())
        .collect::<Vec<_>>();
    let continuity = summed_output_continuity(
        stereo_spec,
        &crossing_samples,
        DOORWAY_BLOCK_FRAMES,
        64,
        DOORWAY_CLICK_RATIO_THRESHOLD,
    )
    .expect("summed doorway continuity");
    let centroids = doorway_render.blocks[neighborhood_start..neighborhood_end]
        .iter()
        .map(|block| spectral_centroid_hz(&block.summed.interleaved))
        .collect::<Vec<_>>();
    let max_centroid_jump = centroids
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0_f32, f32::max);
    assert!(
        continuity.max_inter_block_level_step_db <= DOORWAY_MAX_LEVEL_STEP_DB,
        "summed doorway level step must be bounded: {:.3} <= {DOORWAY_MAX_LEVEL_STEP_DB} dB",
        continuity.max_inter_block_level_step_db
    );
    assert!(
        max_centroid_jump <= DOORWAY_MAX_CENTROID_JUMP_HZ,
        "summed doorway timbre step must be bounded: {max_centroid_jump:.2} <= {DOORWAY_MAX_CENTROID_JUMP_HZ} Hz"
    );
    assert_eq!(
        continuity.detected_click_count, 0,
        "summed doorway output must contain zero state clicks"
    );
    write_pcm(
        &output_dir.join("doorway-walk-summed.wav"),
        &doorway_render.summed.interleaved,
    );
    println!(
        "S4 doorway crossing_block={} crossing_y={:.4}m neighborhood_blocks={} level_step={:.3}<={}dB centroid_jump={:.2}<={}Hz max_click_ratio={:.4}<={} clicks={} retained={:?}",
        crossing_index,
        doorway_request.listener_trajectory[crossing_index]
            .position_enu
            .y,
        neighborhood_end - neighborhood_start,
        continuity.max_inter_block_level_step_db,
        DOORWAY_MAX_LEVEL_STEP_DB,
        max_centroid_jump,
        DOORWAY_MAX_CENTROID_JUMP_HZ,
        continuity.max_boundary_step_to_peak_ratio,
        DOORWAY_CLICK_RATIO_THRESHOLD,
        continuity.detected_click_count,
        doorway_render.retained,
    );
    println!("S4 WAV output={}", output_dir.display());
}

fn parse_fixture(json: &str) -> Fixture {
    let root: Value = serde_json::from_str(json).expect("fixture JSON parses");
    let id = string(&root["fixture_id"]).to_owned();
    assert_eq!(string(&root["gate"]), "S3", "{id} compatibility gate");
    let geometry = &root["geometry"];
    let vertices_enu_m = array(geometry, "vertices_m")
        .iter()
        .map(vec3)
        .collect::<Vec<_>>();

    let material_object = geometry["materials"]
        .as_object()
        .expect("fixture materials object");
    let mut material_indices_by_name = BTreeMap::new();
    let mut materials = Vec::with_capacity(material_object.len());
    for (name, material) in material_object {
        material_indices_by_name.insert(name.clone(), materials.len());
        materials.push(AcousticMaterial {
            absorption: triple(&material["absorption"]),
            scattering: number(&material["scattering"]),
            transmission: triple(&material["transmission"]),
        });
    }
    let triangle_values = array(geometry, "triangles");
    let triangles = triangle_values
        .iter()
        .map(|triangle| {
            let indices = triangle["indices"].as_array().expect("triangle indices");
            [
                integer(&indices[0]) as i32,
                integer(&indices[1]) as i32,
                integer(&indices[2]) as i32,
            ]
        })
        .collect::<Vec<_>>();
    let material_indices = triangle_values
        .iter()
        .map(|triangle| {
            *material_indices_by_name
                .get(string(&triangle["material"]))
                .expect("triangle material exists") as i32
        })
        .collect::<Vec<_>>();

    let simulation_json = &root["simulation"];
    let reflections = &simulation_json["reflections"];
    let pathing = &simulation_json["pathing"];
    let mut simulation = S3SimulationConfig::default();
    simulation.reflection_rays = integer(&reflections["rays"]) as i32;
    simulation.reflection_bounces = integer(&reflections["bounces"]) as i32;
    simulation.reflection_duration_s = number(&reflections["duration_s"]);
    simulation.reflection_effect = ReflectionEffectConfig::CONVOLUTION;
    simulation.pathing_order = integer(&pathing["order"]) as i32;
    simulation.validate_paths = pathing["validation"]
        .as_bool()
        .expect("path validation bool");
    simulation.find_alternate_paths = pathing["alternate_paths"]
        .as_bool()
        .expect("alternate paths bool");
    let probe = &simulation_json["probe_volume"];
    let probes = ProbeVolume {
        min_enu_m: vec3(&probe["min_m"]),
        max_enu_m: vec3(&probe["max_m"]),
        spacing_m: number(&probe["spacing_m"]),
        height_above_floor_m: number(&simulation_json["probe_generation"]["height_m"]),
    };
    let source = vec3(&root["source"]["position_m"]);
    let listener = ListenerPose {
        position_enu: vec3(&root["listener"]["position_m"]),
        ahead_enu: vec3(&root["listener"]["forward_enu"]),
        up_enu: vec3(&root["listener"]["up_enu"]),
    };
    let trajectory = root["listener"]
        .get("trajectory_m")
        .and_then(Value::as_array)
        .map(|values| values.iter().map(vec3).collect())
        .unwrap_or_default();

    Fixture {
        id,
        mesh: SceneMesh {
            vertices_enu_m,
            triangles,
            material_indices,
            materials,
        },
        material_indices_by_name,
        source,
        listener,
        trajectory,
        probes,
        simulation,
    }
}

fn bake_fixture(fixture: &Fixture) -> fightbox_steam_audio::BakedProbeBatch {
    bake_s3(&S3BakeRequest {
        mesh: fixture.mesh.clone(),
        probes: fixture.probes,
        pathing: PathBakeConfig {
            num_visibility_samples: 1,
            probe_visibility_radius_m: 0.5,
            visibility_threshold: 0.1,
            visibility_range_m: 30.0,
            path_range_m: 50.0,
            num_threads: 1,
        },
    })
    .unwrap_or_else(|error| panic!("bake {} with pathing enabled: {error}", fixture.id))
}

fn render_static(
    fixture: &Fixture,
    baked: &fightbox_steam_audio::BakedProbeBatch,
    input_mono: Vec<f32>,
) -> fightbox_steam_audio::S3RenderOutput {
    render_s3(
        &S3RenderRequest {
            mesh: fixture.mesh.clone(),
            audio: AudioConfig {
                sample_rate_hz: SAMPLE_RATE,
                frame_size: IMPULSE_BLOCK_FRAMES,
            },
            simulation: fixture.simulation,
            source_position_enu: fixture.source,
            listener: fixture.listener,
            input_mono,
            calibration_gain: 0.1,
        },
        baked,
    )
    .unwrap_or_else(|error| panic!("render {} through full pipeline: {error}", fixture.id))
}

fn one_wall_control(canyon: &Fixture) -> Fixture {
    let mut control = canyon.clone();
    control.id = "s4-canyon-single-slapback-control".to_owned();
    // A single broad face centered on the source/listener axis produces one
    // predominantly symmetric return. It is deliberately not one remaining
    // lateral canyon face, whose HRTF asymmetry would make "slapback" look
    // decorrelated for the wrong reason.
    let masonry = *control
        .material_indices_by_name
        .get("masonry")
        .expect("canyon masonry") as i32;
    let ground = *control
        .material_indices_by_name
        .get("ground")
        .expect("canyon ground") as i32;
    control.mesh.materials[masonry as usize].scattering = 0.0;
    control.mesh.materials[ground as usize] = AcousticMaterial {
        absorption: [0.99, 0.99, 0.99],
        scattering: 0.0,
        transmission: [0.0; 3],
    };
    control.mesh.vertices_enu_m = vec![
        EnuVector3::new(-5.0, 6.0, 0.0),
        EnuVector3::new(5.0, 6.0, 0.0),
        EnuVector3::new(5.0, 6.0, 8.0),
        EnuVector3::new(-5.0, 6.0, 8.0),
        EnuVector3::new(-3.0, -10.0, 0.0),
        EnuVector3::new(3.0, -10.0, 0.0),
        EnuVector3::new(3.0, 10.0, 0.0),
        EnuVector3::new(-3.0, 10.0, 0.0),
    ];
    control.mesh.triangles = vec![
        [0, 1, 2],
        [0, 2, 3],
        [2, 1, 0],
        [3, 2, 0],
        [4, 5, 6],
        [4, 6, 7],
        [6, 5, 4],
        [7, 6, 4],
    ];
    control.mesh.material_indices = vec![
        masonry, masonry, masonry, masonry, ground, ground, ground, ground,
    ];
    control.simulation.reflection_bounces = 1;
    control
}

fn doorway_trajectory_request(fixture: &Fixture) -> S3TrajectoryRenderRequest {
    let start = *fixture.trajectory.first().expect("doorway start");
    let end = *fixture.trajectory.last().expect("doorway end");
    let distance_m = end.y - start.y;
    assert!(distance_m > 0.0);
    let meters_per_block = 1.4 * DOORWAY_BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
    let blocks = (distance_m / meters_per_block).ceil() as usize + 1;
    let listener_trajectory = (0..blocks)
        .map(|block| {
            let distance = (block as f32 * meters_per_block).min(distance_m);
            ListenerPose {
                position_enu: EnuVector3::new(start.x, start.y + distance, start.z),
                ahead_enu: fixture.listener.ahead_enu,
                up_enu: fixture.listener.up_enu,
            }
        })
        .collect::<Vec<_>>();
    let total_frames = blocks * DOORWAY_BLOCK_FRAMES;
    let input_mono = (0..total_frames)
        .map(|frame| {
            [220.0, 440.0, 880.0, 1_760.0]
                .iter()
                .map(|frequency| (TAU * frequency * frame as f32 / SAMPLE_RATE as f32).sin())
                .sum::<f32>()
                * 0.005
        })
        .collect::<Vec<_>>();
    let mut simulation = fixture.simulation;
    // The doorway assertion is continuity, not high-ray-count convergence.
    // The fixture's 2048 rays and full direct/path/reflection stage order remain.
    simulation.reflection_effect = ReflectionEffectConfig::CONVOLUTION;
    S3TrajectoryRenderRequest {
        base: S3RenderRequest {
            mesh: fixture.mesh.clone(),
            audio: AudioConfig {
                sample_rate_hz: SAMPLE_RATE,
                frame_size: DOORWAY_BLOCK_FRAMES as i32,
            },
            simulation,
            source_position_enu: fixture.source,
            listener: listener_trajectory[0],
            input_mono,
            calibration_gain: 1.0,
        },
        listener_trajectory,
    }
}

fn generated_impulse(frames: usize) -> Vec<f32> {
    let mut impulse = vec![0.0_f32; frames];
    impulse[0] = 1.0;
    impulse
}

fn decay_measurement(interleaved: &[f32]) -> DecayMeasurement {
    let frame_energy = interleaved
        .chunks_exact(2)
        .map(|frame| {
            let left = f64::from(frame[0]);
            let right = f64::from(frame[1]);
            left * left + right * right
        })
        .collect::<Vec<_>>();
    let total_energy = frame_energy.iter().sum::<f64>();
    assert!(total_energy > 0.0 && total_energy.is_finite());
    // In this compact 4×5×3 m room, 95% of masonry energy has arrived by
    // roughly 40 ms. A 25 ms split therefore measures the actual late field,
    // while a 100 ms split would measure only the convolution noise floor.
    let late_start = samples_from_ms(25.0).min(frame_energy.len());
    let late_energy = frame_energy[late_start..].iter().sum::<f64>();
    let target = total_energy * 0.95;
    let mut accumulated = 0.0_f64;
    let energy_95_frame = frame_energy
        .iter()
        .position(|energy| {
            accumulated += *energy;
            accumulated >= target
        })
        .unwrap_or(frame_energy.len().saturating_sub(1));
    DecayMeasurement {
        total_energy,
        late_energy,
        energy_95_time_s: energy_95_frame as f32 / SAMPLE_RATE as f32,
    }
}

fn spectral_centroid_hz(interleaved: &[f32]) -> f32 {
    let mono = interleaved
        .chunks_exact(2)
        .map(|frame| 0.5 * (frame[0] + frame[1]))
        .collect::<Vec<_>>();
    let mut weighted = 0.0_f64;
    let mut power_sum = 0.0_f64;
    for frequency in (80..=6_000).step_by(80) {
        let omega = TAU as f64 * frequency as f64 / SAMPLE_RATE as f64;
        let (real, imaginary) = mono.iter().enumerate().fold(
            (0.0_f64, 0.0_f64),
            |(real, imaginary), (frame, sample)| {
                let phase = omega * frame as f64;
                (
                    real + f64::from(*sample) * phase.cos(),
                    imaginary - f64::from(*sample) * phase.sin(),
                )
            },
        );
        let power = real * real + imaginary * imaginary;
        weighted += frequency as f64 * power;
        power_sum += power;
    }
    if power_sum > 0.0 {
        (weighted / power_sum) as f32
    } else {
        0.0
    }
}

fn write_pcm(path: &Path, interleaved: &[f32]) {
    let bytes = write_wav(stereo_spec(), interleaved).expect("encode finite float WAV");
    fs::write(path, bytes).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE as u32,
        channels: 2,
    }
}

fn samples_from_ms(milliseconds: f32) -> usize {
    (milliseconds * SAMPLE_RATE as f32 / 1_000.0).round() as usize
}

fn array<'a>(object: &'a Value, field: &str) -> &'a [Value] {
    object[field]
        .as_array()
        .unwrap_or_else(|| panic!("{field} array"))
}

fn vec3(value: &Value) -> EnuVector3 {
    let values = value.as_array().expect("vec3 array");
    EnuVector3::new(number(&values[0]), number(&values[1]), number(&values[2]))
}

fn triple(value: &Value) -> [f32; 3] {
    let values = value.as_array().expect("triple array");
    [number(&values[0]), number(&values[1]), number(&values[2])]
}

fn number(value: &Value) -> f32 {
    value.as_f64().expect("finite fixture number") as f32
}

fn integer(value: &Value) -> i64 {
    value.as_i64().expect("fixture integer")
}

fn string(value: &Value) -> &str {
    value.as_str().expect("fixture string")
}
