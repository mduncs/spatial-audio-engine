//! Phase D v2.4 percept 7: Tom's Diner coloration qualification.
//!
//! The ignored test renders the sha256-pinned mono recording as one fixed point
//! source through the retained direct + path + reflections pipeline. The
//! listener approaches, completes one orbit, and recedes at fixture walking
//! speed while always facing the source. Comb, pump, and zipper assertions are
//! made only on the summed binaural output.

use fightbox_evidence::{
    DEFAULT_APPROACH_DROP_TOLERANCE_DB, DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
    DEFAULT_PUMP_MODULATION_THRESHOLD, WavSpec, sha256_hex, summed_output_continuity,
    summed_output_pump, time_varying_spectral_notches, write_wav,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, ListenerPose, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3RenderRequest, S3SimulationConfig,
    S3TrajectoryRenderRequest, SceneMesh, bake_s3, render_s3_trajectory,
};
use serde_json::Value;
use std::collections::BTreeMap;
use std::f32::consts::TAU;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: usize = 2_048;
const COMB_WINDOW_FRAMES: usize = 16_384;
const COMB_HOP_FRAMES: usize = 24_000;
const PUMP_ENVELOPE_FRAMES: usize = 2_400;
const MONOTONIC_TOLERANCE_WINDOW_FRAMES: usize = 96_000;
const SAFETY_CEILING_DBFS: f32 = -1.0;
const ZIPPER_CLICK_RATIO_THRESHOLD: f32 = 1.0;
const ZIPPER_MAX_ADDED_INTER_BLOCK_STEP_DB: f32 = 1.0;

#[derive(Clone)]
struct WalkFixture {
    id: String,
    mesh: SceneMesh,
    source: EnuVector3,
    initial_listener: ListenerPose,
    speed_mps: f32,
    approach_from: EnuVector3,
    approach_to: EnuVector3,
    orbit_center: EnuVector3,
    orbit_radius_m: f32,
    orbit_revolutions: f32,
    recede_to: EnuVector3,
    probes: ProbeVolume,
    simulation: S3SimulationConfig,
    asset_id: String,
}

struct WalkTrajectory {
    poses: Vec<ListenerPose>,
    approach_end_frame: usize,
    total_distance_m: f32,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and local Tom's Diner WAV"]
fn toms_diner_approach_orbit_recede_has_no_coloration_corruption() {
    let fixture = parse_fixture(include_str!(
        "../../../fixtures/toms-diner-walk/fixture.json"
    ));
    assert_eq!(fixture.id, "toms-diner-coloration-walk");
    assert_eq!(fixture.asset_id, "toms-diner");
    assert!((fixture.speed_mps - 1.5).abs() < f32::EPSILON);
    assert!((distance(fixture.approach_from, fixture.approach_to) - 37.5).abs() < 1.0e-4);
    assert!((fixture.orbit_radius_m - 2.5).abs() < f32::EPSILON);
    assert!((fixture.orbit_revolutions - 1.0).abs() < f32::EPSILON);
    assert!((distance(fixture.approach_to, fixture.recede_to) - 37.5).abs() < 1.0e-4);

    let trajectory = build_trajectory(&fixture);
    let required_frames = trajectory.poses.len() * BLOCK_FRAMES;
    let input_mono = load_pinned_asset(&fixture.asset_id, required_frames);
    let baked = bake_s3(&S3BakeRequest {
        mesh: fixture.mesh.clone(),
        probes: fixture.probes,
        elevated_probe_layers: Vec::new(),
        pathing: PathBakeConfig {
            num_visibility_samples: 1,
            probe_visibility_radius_m: 0.5,
            visibility_threshold: 0.1,
            visibility_range_m: 50.0,
            path_range_m: 100.0,
            num_threads: 1,
        },
    })
    .expect("bake Tom's Diner walk with pathing enabled");
    let render = render_s3_trajectory(
        &S3TrajectoryRenderRequest {
            base: S3RenderRequest {
                mesh: fixture.mesh,
                audio: AudioConfig {
                    sample_rate_hz: SAMPLE_RATE,
                    frame_size: BLOCK_FRAMES as i32,
                },
                simulation: fixture.simulation,
                source_position_enu: fixture.source,
                listener: fixture.initial_listener,
                input_mono: input_mono.clone(),
                calibration_gain: 0.25,
            },
            listener_trajectory: trajectory.poses,
        },
        &baked,
    )
    .expect("render Tom's Diner walk through retained full pipeline");

    assert_eq!(render.summed.frame_count, required_frames);
    assert!(
        render
            .summed
            .interleaved
            .iter()
            .all(|sample| sample.is_finite()),
        "full pipeline must remain finite; pathing is intentionally never skipped"
    );

    let spec = stereo_spec();
    let comb = time_varying_spectral_notches(
        spec,
        &render.summed.interleaved,
        &input_mono,
        COMB_WINDOW_FRAMES,
        COMB_HOP_FRAMES,
    )
    .expect("summed-output moving-notch metric");
    let pump = summed_output_pump(
        spec,
        &render.summed.interleaved,
        &input_mono,
        trajectory.approach_end_frame,
        PUMP_ENVELOPE_FRAMES,
        MONOTONIC_TOLERANCE_WINDOW_FRAMES,
        SAFETY_CEILING_DBFS,
        DEFAULT_APPROACH_DROP_TOLERANCE_DB,
    )
    .expect("summed-output pump and monotonic-approach metrics");
    let zipper = summed_output_continuity(
        spec,
        &render.summed.interleaved,
        BLOCK_FRAMES,
        64,
        ZIPPER_CLICK_RATIO_THRESHOLD,
    )
    .expect("summed-output zipper continuity");
    let reference_stereo = input_mono
        .iter()
        .flat_map(|sample| [*sample, *sample])
        .collect::<Vec<_>>();
    let reference_step = summed_output_continuity(
        spec,
        &reference_stereo,
        BLOCK_FRAMES,
        64,
        ZIPPER_CLICK_RATIO_THRESHOLD,
    )
    .expect("source-program continuity reference");
    let added_inter_block_step_db = (zipper.max_inter_block_level_step_db
        - reference_step.max_inter_block_level_step_db)
        .max(0.0);

    let output_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/toms-diner-walk");
    fs::create_dir_all(&output_dir).expect("create target/toms-diner-walk");
    let wav_path = output_dir.join("walk-summed.wav");
    let wav = write_wav(spec, &render.summed.interleaved).expect("encode finite summed WAV");
    fs::write(&wav_path, wav)
        .unwrap_or_else(|error| panic!("write {}: {error}", wav_path.display()));

    println!(
        "Tom's Diner walk distance={:.3}m duration={:.3}s blocks={} retained={:?}",
        trajectory.total_distance_m,
        required_frames as f32 / SAMPLE_RATE as f32,
        render.blocks.len(),
        render.retained,
    );
    println!(
        "comb moving_notch={:.3}<{}dB deepest_regular={:.3}dB moving_pairs={} regular_windows={}/{} method=source-normalized Hann spectra, >=5 regularly-spaced moving notches",
        comb.max_moving_notch_depth_db,
        DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
        comb.deepest_regular_notch_db,
        comb.moving_window_pair_count,
        comb.regularly_spaced_window_count,
        comb.analyzed_windows,
    );
    println!(
        "pump modulation={:.4}<{} envelope_windows={} monotonic_approach max_drop={:.3}<={}dB violations={} windows={} safety_ceiling={:.1}dBFS reached={}",
        pump.modulation_depth,
        DEFAULT_PUMP_MODULATION_THRESHOLD,
        pump.eligible_envelope_windows,
        pump.max_approach_drop_db,
        DEFAULT_APPROACH_DROP_TOLERANCE_DB,
        pump.approach_violation_count,
        pump.approach_window_count,
        pump.safety_ceiling_dbfs,
        pump.safety_ceiling_reached,
    );
    println!(
        "zipper clicks={} max_boundary_ratio={:.4}<={} max_inter_block_step={:.3}dB source_step={:.3}dB added={:.3}<={}dB",
        zipper.detected_click_count,
        zipper.max_boundary_step_to_peak_ratio,
        ZIPPER_CLICK_RATIO_THRESHOLD,
        zipper.max_inter_block_level_step_db,
        reference_step.max_inter_block_level_step_db,
        added_inter_block_step_db,
        ZIPPER_MAX_ADDED_INTER_BLOCK_STEP_DB,
    );
    println!("Tom's Diner WAV output={}", wav_path.display());

    assert!(
        comb.max_moving_notch_depth_db < DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
        "moving regularly spaced spectral notches must remain below the coherent-comb ceiling: {:.3} < {:.3} dB; {comb:?}",
        comb.max_moving_notch_depth_db,
        DEFAULT_MOVING_NOTCH_THRESHOLD_DB
    );
    assert!(
        pump.modulation_depth < DEFAULT_PUMP_MODULATION_THRESHOLD,
        "0.5-8 Hz program-compensated envelope modulation must remain bounded: {:.4} < {:.4}; a pumping limiter is corruption, not tuning",
        pump.modulation_depth,
        DEFAULT_PUMP_MODULATION_THRESHOLD
    );
    assert_eq!(
        pump.approach_violation_count, 0,
        "approach level must be monotonic in two-second windows within {:.2} dB until the {:.1} dBFS safety ceiling; max drop {:.3} dB",
        DEFAULT_APPROACH_DROP_TOLERANCE_DB, SAFETY_CEILING_DBFS, pump.max_approach_drop_db
    );
    assert_eq!(
        zipper.detected_click_count, 0,
        "summed walk must contain zero render-block boundary clicks"
    );
    assert!(
        added_inter_block_step_db <= ZIPPER_MAX_ADDED_INTER_BLOCK_STEP_DB,
        "summed inter-block level step must add no more than {:.3} dB over the source program: output {:.3} dB, source {:.3} dB",
        ZIPPER_MAX_ADDED_INTER_BLOCK_STEP_DB,
        zipper.max_inter_block_level_step_db,
        reference_step.max_inter_block_level_step_db,
    );
}

fn build_trajectory(fixture: &WalkFixture) -> WalkTrajectory {
    let approach_distance = distance(fixture.approach_from, fixture.approach_to);
    let orbit_distance = TAU * fixture.orbit_radius_m * fixture.orbit_revolutions;
    let recede_distance = distance(fixture.approach_to, fixture.recede_to);
    let total_distance_m = approach_distance + orbit_distance + recede_distance;
    let meters_per_block = fixture.speed_mps * BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
    let block_count = (total_distance_m / meters_per_block).ceil() as usize + 1;
    let poses = (0..block_count)
        .map(|block| {
            let traveled = (block as f32 * meters_per_block).min(total_distance_m);
            let position = if traveled <= approach_distance {
                lerp(
                    fixture.approach_from,
                    fixture.approach_to,
                    traveled / approach_distance,
                )
            } else if traveled <= approach_distance + orbit_distance {
                let orbit_travel = traveled - approach_distance;
                let angle = orbit_travel / fixture.orbit_radius_m;
                EnuVector3::new(
                    fixture.orbit_center.x + fixture.orbit_radius_m * angle.cos(),
                    fixture.orbit_center.y + fixture.orbit_radius_m * angle.sin(),
                    fixture.orbit_center.z,
                )
            } else {
                let recede_travel = traveled - approach_distance - orbit_distance;
                lerp(
                    fixture.approach_to,
                    fixture.recede_to,
                    recede_travel / recede_distance,
                )
            };
            face_source(position, fixture.source)
        })
        .collect::<Vec<_>>();
    let approach_end_frame = (approach_distance / meters_per_block).floor() as usize * BLOCK_FRAMES;
    assert_eq!(poses[0], fixture.initial_listener);
    assert!(distance(poses.last().unwrap().position_enu, fixture.recede_to) < 1.0e-4);
    WalkTrajectory {
        poses,
        approach_end_frame,
        total_distance_m,
    }
}

fn face_source(position: EnuVector3, source: EnuVector3) -> ListenerPose {
    let delta = EnuVector3::new(source.x - position.x, source.y - position.y, 0.0);
    let length = (delta.x * delta.x + delta.y * delta.y).sqrt();
    ListenerPose {
        position_enu: position,
        ahead_enu: EnuVector3::new(delta.x / length, delta.y / length, 0.0),
        up_enu: EnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn lerp(from: EnuVector3, to: EnuVector3, fraction: f32) -> EnuVector3 {
    let fraction = fraction.clamp(0.0, 1.0);
    EnuVector3::new(
        from.x + (to.x - from.x) * fraction,
        from.y + (to.y - from.y) * fraction,
        from.z + (to.z - from.z) * fraction,
    )
}

fn distance(left: EnuVector3, right: EnuVector3) -> f32 {
    let x = left.x - right.x;
    let y = left.y - right.y;
    let z = left.z - right.z;
    (x * x + y * y + z * z).sqrt()
}

fn parse_fixture(json: &str) -> WalkFixture {
    let root: Value = serde_json::from_str(json).expect("fixture JSON parses");
    let id = string(&root["fixture_id"]).to_owned();
    assert_eq!(string(&root["gate"]), "S3", "{id} compatibility gate");
    let source = vec3(&root["source"]["position_m"]);
    assert_eq!(string(&root["source"]["spatial_extent"]), "point");
    let initial_position = vec3(&root["listener"]["position_m"]);
    let initial_listener = ListenerPose {
        position_enu: initial_position,
        ahead_enu: vec3(&root["listener"]["forward_enu"]),
        up_enu: vec3(&root["listener"]["up_enu"]),
    };
    assert_eq!(string(&root["listener"]["orientation"]), "face_source");
    let trajectory = &root["listener"]["trajectory"];
    let segments = array(trajectory, "segments");
    assert_eq!(segments.len(), 3);
    assert_eq!(string(&segments[0]["kind"]), "line");
    assert_eq!(string(&segments[0]["name"]), "approach");
    assert_eq!(string(&segments[1]["kind"]), "orbit");
    assert_eq!(string(&segments[1]["name"]), "orbit");
    assert_eq!(string(&segments[1]["direction"]), "counterclockwise");
    assert_eq!(number(&segments[1]["start_angle_degrees"]), 0.0);
    assert_eq!(string(&segments[2]["kind"]), "line");
    assert_eq!(string(&segments[2]["name"]), "recede");
    let approach_from = vec3(&segments[0]["from_m"]);
    let approach_to = vec3(&segments[0]["to_m"]);
    assert_eq!(approach_to, vec3(&segments[2]["from_m"]));

    let geometry = &root["geometry"];
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
    let mesh = SceneMesh {
        vertices_enu_m: array(geometry, "vertices_m").iter().map(vec3).collect(),
        triangles: triangle_values
            .iter()
            .map(|triangle| {
                let indices = triangle["indices"].as_array().expect("triangle indices");
                [
                    integer(&indices[0]) as i32,
                    integer(&indices[1]) as i32,
                    integer(&indices[2]) as i32,
                ]
            })
            .collect(),
        material_indices: triangle_values
            .iter()
            .map(|triangle| {
                *material_indices_by_name
                    .get(string(&triangle["material"]))
                    .expect("triangle material exists") as i32
            })
            .collect(),
        materials,
    };

    let simulation_json = &root["simulation"];
    let reflections = &simulation_json["reflections"];
    let pathing = &simulation_json["pathing"];
    assert!(reflections["enabled"].as_bool().expect("reflections bool"));
    assert!(pathing["enabled"].as_bool().expect("pathing bool"));
    assert_eq!(
        array(pathing, "runtime_order")
            .iter()
            .map(string)
            .collect::<Vec<_>>(),
        ["direct", "path", "reflections"]
    );
    let mut simulation = S3SimulationConfig::default();
    simulation.reflection_rays = integer(&reflections["rays"]) as i32;
    simulation.reflection_bounces = integer(&reflections["bounces"]) as i32;
    simulation.reflection_duration_s = number(&reflections["duration_s"]);
    simulation.reflection_effect = ReflectionEffectConfig::CONVOLUTION;
    simulation.pathing_order = integer(&pathing["order"]) as i32;
    simulation.validate_paths = pathing["validation"].as_bool().expect("validation bool");
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

    WalkFixture {
        id,
        mesh,
        source,
        initial_listener,
        speed_mps: number(&trajectory["speed_mps"]),
        approach_from,
        approach_to,
        orbit_center: vec3(&segments[1]["center_m"]),
        orbit_radius_m: number(&segments[1]["radius_m"]),
        orbit_revolutions: number(&segments[1]["revolutions"]),
        recede_to: vec3(&segments[2]["to_m"]),
        probes,
        simulation,
        asset_id: string(&root["source"]["asset_id"]).to_owned(),
    }
}

fn load_pinned_asset(asset_id: &str, required_frames: usize) -> Vec<f32> {
    let root = repository_root();
    let descriptor_path = root.join(format!("fixtures/assets/{asset_id}.json"));
    let descriptor_bytes = fs::read(&descriptor_path)
        .unwrap_or_else(|error| panic!("read {}: {error}", descriptor_path.display()));
    let descriptor: Value =
        serde_json::from_slice(&descriptor_bytes).expect("asset descriptor JSON");
    assert_eq!(string(&descriptor["asset_id"]), asset_id);
    assert_eq!(string(&descriptor["kind"]), "wav");
    assert_eq!(integer(&descriptor["channels"]), 1);
    assert_eq!(
        integer(&descriptor["sample_rate_hz"]),
        i64::from(SAMPLE_RATE)
    );
    let wav = &descriptor["generator"]["wav"];
    assert!(wav["loop"].as_bool().expect("asset loop bool"));
    let wav_path = root.join(string(&wav["path"]));
    let wav_bytes =
        fs::read(&wav_path).unwrap_or_else(|error| panic!("read {}: {error}", wav_path.display()));
    assert_eq!(
        sha256_hex(&wav_bytes),
        string(&wav["sha256"]),
        "pinned Tom's Diner WAV bytes"
    );
    let mut decoded = decode_mono_wav(&wav_bytes);
    normalize_rms(&mut decoded, number(&descriptor["target_rms_dbfs"]));
    let start_frame = integer(&wav["start_frame"]) as usize;
    assert!(start_frame < decoded.len());
    (0..required_frames)
        .map(|frame| decoded[(start_frame + frame) % decoded.len()])
        .collect()
}

fn decode_mono_wav(bytes: &[u8]) -> Vec<f32> {
    assert!(bytes.len() >= 12);
    assert_eq!(&bytes[..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let mut format = None;
    let mut data = None;
    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size =
            u32::from_le_bytes(bytes[position + 4..position + 8].try_into().unwrap()) as usize;
        position += 8;
        let end = position.checked_add(size).expect("WAV chunk size");
        assert!(end <= bytes.len(), "truncated WAV chunk");
        if id == b"fmt " && size >= 16 {
            let body = &bytes[position..end];
            format = Some((
                u16::from_le_bytes(body[0..2].try_into().unwrap()),
                u16::from_le_bytes(body[2..4].try_into().unwrap()),
                u32::from_le_bytes(body[4..8].try_into().unwrap()),
                u16::from_le_bytes(body[14..16].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(&bytes[position..end]);
        }
        position = end + (size & 1);
    }
    let (tag, channels, sample_rate, bits) = format.expect("WAV fmt chunk");
    assert_eq!((tag, channels, sample_rate, bits), (1, 1, 48_000, 16));
    data.expect("WAV data chunk")
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32_768.0)
        .collect()
}

fn normalize_rms(samples: &mut [f32], target_rms_dbfs: f32) {
    let rms = (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32;
    assert!(rms > 0.0);
    let gain = 10.0_f32.powf((target_rms_dbfs - 20.0 * rms.log10()) / 20.0);
    assert!(
        samples.iter().all(|sample| sample.abs() * gain <= 1.0),
        "descriptor normalization must not clip"
    );
    for sample in samples {
        *sample *= gain;
    }
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE as u32,
        channels: 2,
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
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
