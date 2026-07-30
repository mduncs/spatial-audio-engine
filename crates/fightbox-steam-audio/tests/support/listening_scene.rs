#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use fightbox_api::{EnuVector3 as ApiVector3, ListenerState, Pose, ReferenceLevel};
use fightbox_evidence::{WavSpec, sha256_hex, write_wav};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, PropagationRenderBlock,
    SimulationRunner, SimulationUpdate, SourceMotion,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, MultiSourceDescriptor, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3SimulationConfig, SceneMesh, bake_s3,
    build_multi_source_session,
};
use serde_json::Value;

pub const SAMPLE_RATE: i32 = 48_000;
pub const BLOCK_FRAMES: usize = 1_024;
const MOVING_SIMULATION_PERIOD_BLOCKS: usize = 5;

pub struct ListeningFixture {
    pub id: String,
    pub mesh: SceneMesh,
    pub source: Source,
    pub listener: ListenerState,
    pub probes: ProbeVolume,
    pub simulation: S3SimulationConfig,
    pub asset_id: String,
    pub reference_level_db_spl: f32,
    path_visibility_range_m: f32,
    path_range_m: f32,
}

pub enum Source {
    Static(ApiVector3),
    ClosedCycle(ClosedCycle),
}

pub struct ClosedCycle {
    pub waypoints: Vec<ApiVector3>,
    pub speed_mps: f32,
}

pub struct TrajectorySample {
    pub position: ApiVector3,
    pub velocity_mps: ApiVector3,
}

pub struct RenderedScene {
    pub interleaved: Vec<f32>,
    pub input_mono: Vec<f32>,
}

impl ListeningFixture {
    pub fn initial_source_position(&self) -> ApiVector3 {
        match &self.source {
            Source::Static(position) => *position,
            Source::ClosedCycle(cycle) => cycle.waypoints[0],
        }
    }

    pub fn source_distance_m(&self) -> f32 {
        distance(self.initial_source_position(), self.listener.pose.position)
    }
}

impl ClosedCycle {
    pub fn total_distance_m(&self) -> f32 {
        self.waypoints
            .iter()
            .copied()
            .zip(self.waypoints.iter().copied().cycle().skip(1))
            .take(self.waypoints.len())
            .map(|(from, to)| distance(from, to))
            .sum()
    }

    pub fn duration_s(&self) -> f32 {
        self.total_distance_m() / self.speed_mps
    }

    pub fn block_samples(&self) -> Vec<TrajectorySample> {
        let duration_s = self.duration_s();
        let block_duration_s = BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
        let block_count = (duration_s / block_duration_s).ceil() as usize + 1;
        (0..block_count)
            .map(|block| {
                let traveled =
                    (block as f32 * block_duration_s * self.speed_mps).min(self.total_distance_m());
                self.sample_at_distance(traveled)
            })
            .collect()
    }

    fn sample_at_distance(&self, traveled_m: f32) -> TrajectorySample {
        let mut remaining = traveled_m;
        for index in 0..self.waypoints.len() {
            let from = self.waypoints[index];
            let to = self.waypoints[(index + 1) % self.waypoints.len()];
            let segment_distance = distance(from, to);
            if remaining <= segment_distance || index + 1 == self.waypoints.len() {
                let direction = normalized(subtract(to, from));
                return TrajectorySample {
                    position: lerp(from, to, remaining / segment_distance),
                    velocity_mps: scale(direction, self.speed_mps),
                };
            }
            remaining -= segment_distance;
        }
        unreachable!("closed trajectory has at least two finite waypoints")
    }
}

pub fn parse_fixture(json: &str) -> ListeningFixture {
    let root: Value = serde_json::from_str(json).expect("fixture JSON parses");
    assert_eq!(string(&root["gate"]), "S3");

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

    let source_json = &root["source"];
    let source = if let Some(trajectory) = source_json.get("trajectory") {
        assert!(
            trajectory["closed_cycle"]
                .as_bool()
                .expect("closed_cycle bool")
        );
        let speed_mps = number(&trajectory["speed_mps"]);
        assert!(speed_mps > 0.0);
        assert!(speed_mps <= number(&trajectory["max_speed_mps"]));
        let waypoints = array(trajectory, "waypoints_m")
            .iter()
            .map(api_vec3)
            .collect::<Vec<_>>();
        assert!(waypoints.len() >= 2);
        Source::ClosedCycle(ClosedCycle {
            waypoints,
            speed_mps,
        })
    } else {
        Source::Static(api_vec3(&source_json["position_m"]))
    };

    let listener_json = &root["listener"];
    let listener = ListenerState {
        pose: Pose {
            position: api_vec3(&listener_json["position_m"]),
            forward: api_vec3(&listener_json["forward_enu"]),
            up: api_vec3(&listener_json["up_enu"]),
        },
        linear_velocity_mps: ApiVector3::default(),
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
    simulation.max_occlusion_samples =
        integer(&simulation_json["direct"]["occlusion_samples"]) as i32;
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
    let path_bake = &simulation_json["path_bake"];
    ListeningFixture {
        id: string(&root["fixture_id"]).to_owned(),
        mesh,
        source,
        listener,
        probes: ProbeVolume {
            min_enu_m: vec3(&probe["min_m"]),
            max_enu_m: vec3(&probe["max_m"]),
            spacing_m: number(&probe["spacing_m"]),
            height_above_floor_m: number(&simulation_json["probe_generation"]["height_m"]),
        },
        simulation,
        asset_id: string(&source_json["asset_id"]).to_owned(),
        reference_level_db_spl: number(&source_json["reference_level"]["db_spl"]),
        path_visibility_range_m: number(&path_bake["visibility_range_m"]),
        path_range_m: number(&path_bake["path_range_m"]),
    }
}

pub fn render_scene(
    fixture: &ListeningFixture,
    trajectory: &[TrajectorySample],
    duration_frames: usize,
) -> RenderedScene {
    assert!(!trajectory.is_empty());
    let block_count = duration_frames.div_ceil(BLOCK_FRAMES);
    assert!(
        trajectory.len() == 1 || trajectory.len() == block_count,
        "static scenes need one pose; moving scenes need one pose per render block"
    );
    let total_frames = block_count * BLOCK_FRAMES;
    let input_mono = load_pinned_asset(&fixture.asset_id, total_frames);
    let baked = bake_s3(&S3BakeRequest {
        mesh: fixture.mesh.clone(),
        probes: fixture.probes,
        pathing: PathBakeConfig {
            num_visibility_samples: 1,
            probe_visibility_radius_m: fixture.probes.spacing_m,
            visibility_threshold: 0.1,
            visibility_range_m: fixture.path_visibility_range_m,
            path_range_m: fixture.path_range_m,
            num_threads: 1,
        },
    })
    .unwrap_or_else(|error| panic!("bake {} with pathing enabled: {error}", fixture.id));
    let descriptor = MultiSourceDescriptor::at(fixture.initial_source_position())
        .with_reference_level(ReferenceLevel::SplAtOneMeter {
            db_spl: fixture.reference_level_db_spl,
        });
    let (mut simulation, mut render) = build_multi_source_session(
        &fixture.mesh,
        &baked,
        AudioConfig {
            sample_rate_hz: SAMPLE_RATE,
            frame_size: BLOCK_FRAMES as i32,
        },
        fixture.simulation,
        &[descriptor],
    )
    .unwrap_or_else(|error| panic!("build retained {} session: {error}", fixture.id));

    let moving = trajectory.len() > 1;
    let mut interleaved = Vec::with_capacity(total_frames * 2);
    let mut left = vec![0.0_f32; BLOCK_FRAMES];
    let mut right = vec![0.0_f32; BLOCK_FRAMES];
    for block in 0..block_count {
        let sample = if moving {
            &trajectory[block]
        } else {
            &trajectory[0]
        };
        let mut sources = [SourceMotion::default(); fightbox_runtime::backend::MAX_ACTIVE_SOURCES];
        sources[0] = SourceMotion {
            active: true,
            pose: Pose {
                position: sample.position,
                forward: normalized_or_north(sample.velocity_mps),
                up: ApiVector3::new(0.0, 0.0, 1.0),
            },
            linear_velocity_mps: sample.velocity_mps,
        };
        simulation.update_inputs(&SimulationUpdate {
            listener: fixture.listener,
            sources,
        });
        simulation.run_direct().expect("direct simulation");
        if !moving || block.is_multiple_of(MOVING_SIMULATION_PERIOD_BLOCKS) {
            simulation
                .run_pathing()
                .expect("pathing simulation must remain enabled");
            simulation.run_reflections().expect("reflection simulation");
        }

        left.fill(0.0);
        right.fill(0.0);
        let block_start = block * BLOCK_FRAMES;
        let input = &input_mono[block_start..block_start + BLOCK_FRAMES];
        let sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: input,
        }];
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: fixture.listener.pose.forward,
                    up: fixture.listener.pose.up,
                },
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .expect("render retained full-pipeline block");
        interleaved.extend(
            left.iter()
                .copied()
                .zip(right.iter().copied())
                .flat_map(|(left, right)| [left, right]),
        );
    }
    RenderedScene {
        interleaved,
        input_mono,
    }
}

pub fn write_summed(scene_name: &str, filename: &str, samples: &[f32]) -> PathBuf {
    let output_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target")
        .join(scene_name);
    fs::create_dir_all(&output_dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", output_dir.display()));
    let path = output_dir.join(filename);
    let wav = write_wav(stereo_spec(), samples).expect("encode finite summed WAV");
    fs::write(&path, wav).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    path
}

pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().copied().map(f32::abs).fold(0.0, f32::max)
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt() as f32
}

pub fn rms_db(samples: &[f32]) -> f32 {
    20.0 * rms(samples).max(f32::MIN_POSITIVE).log10()
}

pub fn stereo_window(interleaved: &[f32], start_s: f32, end_s: f32) -> &[f32] {
    let start = (start_s * SAMPLE_RATE as f32).round() as usize * 2;
    let end = (end_s * SAMPLE_RATE as f32).round() as usize * 2;
    &interleaved[start.min(interleaved.len())..end.min(interleaved.len())]
}

pub fn mono_window(mono: &[f32], start_s: f32, end_s: f32) -> &[f32] {
    let start = (start_s * SAMPLE_RATE as f32).round() as usize;
    let end = (end_s * SAMPLE_RATE as f32).round() as usize;
    &mono[start.min(mono.len())..end.min(mono.len())]
}

pub fn channel_energy_balance(interleaved: &[f32]) -> f32 {
    let (left, right) =
        interleaved
            .chunks_exact(2)
            .fold((0.0_f64, 0.0_f64), |(left, right), frame| {
                (
                    left + f64::from(frame[0]) * f64::from(frame[0]),
                    right + f64::from(frame[1]) * f64::from(frame[1]),
                )
            });
    ((left - right) / (left + right).max(f64::MIN_POSITIVE)) as f32
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
        "pinned {asset_id} WAV bytes"
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
    let current_rms = rms(samples);
    assert!(current_rms > 0.0);
    let gain = 10.0_f32.powf((target_rms_dbfs - 20.0 * current_rms.log10()) / 20.0);
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

fn api_vec3(value: &Value) -> ApiVector3 {
    let values = value.as_array().expect("vec3 array");
    ApiVector3::new(number(&values[0]), number(&values[1]), number(&values[2]))
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

fn subtract(left: ApiVector3, right: ApiVector3) -> ApiVector3 {
    ApiVector3::new(
        left.east_m - right.east_m,
        left.north_m - right.north_m,
        left.up_m - right.up_m,
    )
}

fn scale(vector: ApiVector3, scalar: f32) -> ApiVector3 {
    ApiVector3::new(
        vector.east_m * scalar,
        vector.north_m * scalar,
        vector.up_m * scalar,
    )
}

fn normalized(vector: ApiVector3) -> ApiVector3 {
    let length = (vector.east_m * vector.east_m
        + vector.north_m * vector.north_m
        + vector.up_m * vector.up_m)
        .sqrt();
    assert!(length > 0.0);
    scale(vector, length.recip())
}

fn normalized_or_north(vector: ApiVector3) -> ApiVector3 {
    if vector == ApiVector3::default() {
        ApiVector3::new(0.0, 1.0, 0.0)
    } else {
        normalized(vector)
    }
}

fn lerp(from: ApiVector3, to: ApiVector3, fraction: f32) -> ApiVector3 {
    let fraction = fraction.clamp(0.0, 1.0);
    ApiVector3::new(
        from.east_m + (to.east_m - from.east_m) * fraction,
        from.north_m + (to.north_m - from.north_m) * fraction,
        from.up_m + (to.up_m - from.up_m) * fraction,
    )
}

fn distance(left: ApiVector3, right: ApiVector3) -> f32 {
    let delta = subtract(left, right);
    (delta.east_m * delta.east_m + delta.north_m * delta.north_m + delta.up_m * delta.up_m).sqrt()
}
