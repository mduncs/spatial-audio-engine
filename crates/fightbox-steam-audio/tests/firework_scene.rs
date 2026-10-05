//! §σ elevated firework listening-scene qualification.
//!
//! Ignored because it bakes pathing and renders through the locally acquired
//! Steam Audio SDK. The fixture JSON is the scene source of truth; the source
//! signal comes from the deterministic evidence generator.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use fightbox_evidence::{WavSpec, firework_burst, write_wav};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, ListenerPose, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3RenderRequest, S3SimulationConfig, SceneMesh, bake_s3,
    render_s3,
};
use serde_json::Value;

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const SOURCE_FRAMES: usize = SAMPLE_RATE as usize * 3;
const MIN_CREST_FACTOR: f32 = 6.0;
const MIN_ONSET_TO_PROGRAM_RMS: f32 = 2.0;
const MIN_INDIRECT_TO_DIRECT_ENERGY: f64 = 0.01;

struct Fixture {
    id: String,
    mesh: SceneMesh,
    source: EnuVector3,
    listener: ListenerPose,
    probes: ProbeVolume,
    simulation: S3SimulationConfig,
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK"]
fn elevated_firework_preserves_impulse_and_audible_city_returns() {
    let fixture = parse_fixture(include_str!("../../../fixtures/s-firework/fixture.json"));
    assert_eq!(fixture.id, "s-firework-elevated-megablock");
    assert!((fixture.source.z - 50.0).abs() < f32::EPSILON);
    let horizontal_distance = ((fixture.source.x - fixture.listener.position_enu.x).powi(2)
        + (fixture.source.y - fixture.listener.position_enu.y).powi(2))
    .sqrt();
    assert!((150.0..=300.0).contains(&horizontal_distance));

    let baked = bake_s3(&S3BakeRequest {
        mesh: fixture.mesh.clone(),
        probes: fixture.probes,
        elevated_probe_layers: Vec::new(),
        pathing: PathBakeConfig {
            num_visibility_samples: 1,
            probe_visibility_radius_m: 1.0,
            visibility_threshold: 0.1,
            visibility_range_m: 300.0,
            path_range_m: 300.0,
            num_threads: 1,
        },
    })
    .expect("bake firework megablock with pathing enabled");
    let generated = firework_burst(mono_spec(), SOURCE_FRAMES, -30.0)
        .expect("generate deterministic firework burst");
    let render = render_s3(
        &S3RenderRequest {
            mesh: fixture.mesh,
            audio: AudioConfig {
                sample_rate_hz: SAMPLE_RATE,
                frame_size: BLOCK_FRAMES,
            },
            simulation: fixture.simulation,
            source_position_enu: fixture.source,
            listener: fixture.listener,
            input_mono: generated.samples,
            calibration_gain: 1.0,
        },
        &baked,
    )
    .expect("render firework through direct + validated path + reflections");

    let summed = &render.stems.pathing_on_sum.interleaved;
    assert!(
        summed.iter().all(|sample| sample.is_finite()),
        "full-pipeline firework output must remain finite"
    );
    let output_peak = peak(summed);
    let output_rms = rms(summed);
    assert!(output_peak > 1.0e-9 && output_rms > 0.0);
    let crest_factor = output_peak / output_rms;

    let onset_frames = frames_from_ms(10.0);
    let comparison_start = onset_frames * 2;
    let comparison_end = frames_from_ms(80.0) * 2;
    let onset_peak = peak(&summed[..comparison_start.min(summed.len())]);
    let following_rms =
        rms(&summed[comparison_start.min(summed.len())..comparison_end.min(summed.len())]);
    let onset_to_program_rms = onset_peak / output_rms.max(f32::MIN_POSITIVE);

    let indirect = &render.stems.reflections.interleaved;
    assert!(
        indirect.iter().all(|sample| sample.is_finite()),
        "indirect stem must remain finite"
    );
    let indirect_tail_start = frames_from_ms(20.0) * 2;
    let indirect_tail_energy = energy(&indirect[indirect_tail_start.min(indirect.len())..]);
    let direct_energy = energy(&render.stems.direct.interleaved);
    let indirect_to_direct_energy = indirect_tail_energy / direct_energy.max(f64::MIN_POSITIVE);

    println!(
        "firework horizontal_distance={horizontal_distance:.2}m probes={} path_bytes={} ir_size={} output_peak={output_peak:.8e} rms={output_rms:.8e} crest={crest_factor:.3}>{MIN_CREST_FACTOR} onset_peak={onset_peak:.8e} following_rms={following_rms:.8e} onset/program_rms={onset_to_program_rms:.3}>{MIN_ONSET_TO_PROGRAM_RMS} indirect_tail_energy={indirect_tail_energy:.8e} direct_energy={direct_energy:.8e} indirect/direct={indirect_to_direct_energy:.8e}>{MIN_INDIRECT_TO_DIRECT_ENERGY:.8e}",
        render.loaded_probe_count,
        render.loaded_path_data_size_bytes,
        render.snapshot.reflections.ir_size,
    );
    assert!(
        crest_factor >= MIN_CREST_FACTOR,
        "summed output must retain an impulsive crest: {crest_factor:.3} >= {MIN_CREST_FACTOR}"
    );
    assert!(
        onset_to_program_rms >= MIN_ONSET_TO_PROGRAM_RMS,
        "first 10 ms must stand above the whole firework program RMS: {onset_to_program_rms:.3} >= {MIN_ONSET_TO_PROGRAM_RMS}"
    );
    assert!(
        indirect_to_direct_energy >= MIN_INDIRECT_TO_DIRECT_ENERGY,
        "city reflections after direct onset must carry audible energy: {indirect_to_direct_energy:.8e} >= {MIN_INDIRECT_TO_DIRECT_ENERGY:.8e}"
    );

    let output_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/firework-scene");
    fs::create_dir_all(&output_dir).expect("create target/firework-scene");
    let wav = write_wav(stereo_spec(), summed).expect("encode finite firework WAV");
    fs::write(output_dir.join("firework-summed.wav"), wav)
        .expect("write target/firework-scene/firework-summed.wav");
    println!("firework WAV output={}", output_dir.display());
}

fn parse_fixture(json: &str) -> Fixture {
    let root: Value = serde_json::from_str(json).expect("fixture JSON parses");
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
        .collect();
    let material_indices = triangle_values
        .iter()
        .map(|triangle| {
            *material_indices_by_name
                .get(string(&triangle["material"]))
                .expect("triangle material exists") as i32
        })
        .collect();

    let simulation_json = &root["simulation"];
    let reflections = &simulation_json["reflections"];
    let pathing = &simulation_json["pathing"];
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
    Fixture {
        id: string(&root["fixture_id"]).into(),
        mesh: SceneMesh {
            vertices_enu_m,
            triangles,
            material_indices,
            materials,
        },
        source: vec3(&root["source"]["position_m"]),
        listener: ListenerPose {
            position_enu: vec3(&root["listener"]["position_m"]),
            ahead_enu: vec3(&root["listener"]["forward_enu"]),
            up_enu: vec3(&root["listener"]["up_enu"]),
        },
        probes: ProbeVolume {
            min_enu_m: vec3(&probe["min_m"]),
            max_enu_m: vec3(&probe["max_m"]),
            spacing_m: number(&probe["spacing_m"]),
            height_above_floor_m: number(&simulation_json["probe_generation"]["height_m"]),
        },
        simulation,
    }
}

fn mono_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE as u32,
        channels: 1,
    }
}

fn stereo_spec() -> WavSpec {
    WavSpec {
        sample_rate_hz: SAMPLE_RATE as u32,
        channels: 2,
    }
}

fn frames_from_ms(milliseconds: f32) -> usize {
    (milliseconds * SAMPLE_RATE as f32 / 1_000.0).round() as usize
}

fn peak(samples: &[f32]) -> f32 {
    samples.iter().copied().map(f32::abs).fold(0.0, f32::max)
}

fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (energy(samples) / samples.len() as f64).sqrt() as f32
}

fn energy(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum()
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
