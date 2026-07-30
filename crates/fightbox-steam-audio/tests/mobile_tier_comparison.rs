//! Contended-machine observational comparison for the Phase E mobile tier.

use fightbox_api::{EnuVector3 as ApiEnuVector3, ListenerState, Pose, ReferenceLevel};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationRunner, SimulationUpdate, SourceMotion,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, MultiSourceDescriptor, PathBakeConfig, ProbeVolume,
    QualityTier, S3BakeRequest, SceneMesh, bake_s3, build_multi_source_session_for_tier,
};
use serde_json::Value;
use std::f32::consts::TAU;
use std::time::Instant;

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: usize = 128;
const MEASURED_BLOCKS: usize = SAMPLE_RATE as usize * 3 / BLOCK_FRAMES;

#[derive(Clone, Copy, Debug)]
struct Observation {
    p50_ns: u64,
    p95_ns: u64,
    p99_ns: u64,
    tracked_memory_bytes: u64,
}

#[test]
#[ignore = "observational benchmark; requires the locally acquired Steam Audio 4.8.1 SDK"]
fn s6a_desktop_vs_mobile_render_and_tracked_memory() {
    let root: Value = serde_json::from_str(include_str!(
        "../../../fixtures/s6a-four-sources/fixture.json"
    ))
    .expect("S6a fixture JSON parses");
    assert_eq!(
        root["fixture_id"].as_str(),
        Some("s6a-four-sources-one-moving")
    );
    let mesh = mesh_from_fixture(&root);
    let probes = probe_volume(&root);
    let baked = bake_s3(&S3BakeRequest {
        mesh: mesh.clone(),
        probes,
        pathing: PathBakeConfig {
            probe_visibility_radius_m: probes.spacing_m,
            visibility_threshold: 0.1,
            visibility_range_m: 100.0,
            path_range_m: 100.0,
            ..PathBakeConfig::default()
        },
    })
    .expect("bake S6a comparison world");
    let positions = source_positions(&root);
    let listener = vector(&root["listener"]["position_m"]);

    let desktop = observe_tier(QualityTier::Desktop, &mesh, &baked, positions, listener);
    let mobile = observe_tier(QualityTier::Mobile, &mesh, &baked, positions, listener);

    assert!(mobile.tracked_memory_bytes < desktop.tracked_memory_bytes);
    println!(
        "MOBILE_TIER_OBSERVATIONAL contended_machine=true fixture=s6a duration_s=3 blocks={} \
         desktop_p50_us={:.3} desktop_p95_us={:.3} desktop_p99_us={:.3} \
         mobile_p50_us={:.3} mobile_p95_us={:.3} mobile_p99_us={:.3} \
         desktop_tracked_bytes={} mobile_tracked_bytes={}",
        MEASURED_BLOCKS,
        desktop.p50_ns as f64 / 1_000.0,
        desktop.p95_ns as f64 / 1_000.0,
        desktop.p99_ns as f64 / 1_000.0,
        mobile.p50_ns as f64 / 1_000.0,
        mobile.p95_ns as f64 / 1_000.0,
        mobile.p99_ns as f64 / 1_000.0,
        desktop.tracked_memory_bytes,
        mobile.tracked_memory_bytes,
    );
}

fn observe_tier(
    tier: QualityTier,
    mesh: &SceneMesh,
    baked: &fightbox_steam_audio::BakedProbeBatch,
    positions: [ApiEnuVector3; 4],
    listener_position: ApiEnuVector3,
) -> Observation {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES as i32,
    };
    let descriptors = positions.map(|position| {
        MultiSourceDescriptor::at(position)
            .with_reference_level(ReferenceLevel::CreativeDb { db: 0.0 })
    });
    let (mut simulation, mut render) = build_multi_source_session_for_tier(
        mesh,
        baked,
        audio,
        tier.simulation_defaults(),
        &descriptors,
        tier,
    )
    .expect("build tiered S6a session");
    let mut update = SimulationUpdate {
        listener: ListenerState {
            pose: pose(listener_position),
            linear_velocity_mps: ApiEnuVector3::default(),
        },
        sources: [SourceMotion::default(); MAX_ACTIVE_SOURCES],
    };
    for (index, position) in positions.into_iter().enumerate() {
        update.sources[index] = SourceMotion {
            active: true,
            pose: pose(position),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
    }
    simulation.update_inputs(&update);
    for _ in 0..20_000 {
        simulation.observe_render_timing(50_000);
    }
    simulation.run_direct().unwrap();
    simulation.run_pathing().unwrap();
    simulation.run_reflections().unwrap();

    let inputs = (0..4)
        .map(|source| {
            (0..BLOCK_FRAMES)
                .map(|frame| {
                    let hz = 220.0 + source as f32 * 110.0;
                    (TAU * hz * frame as f32 / SAMPLE_RATE as f32).sin() * 0.02
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let source_blocks = inputs
        .iter()
        .enumerate()
        .map(|(source_index, input_mono)| BackendSourceBlock {
            source_index,
            input_mono,
        })
        .collect::<Vec<_>>();
    let mut left = vec![0.0; BLOCK_FRAMES];
    let mut right = vec![0.0; BLOCK_FRAMES];
    let orientation = ListenerOrientation {
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    };
    for _ in 0..64 {
        left.fill(0.0);
        right.fill(0.0);
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: orientation,
                sources: &source_blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
    }
    let mut timings = Vec::with_capacity(MEASURED_BLOCKS);
    for _ in 0..MEASURED_BLOCKS {
        left.fill(0.0);
        right.fill(0.0);
        let started = Instant::now();
        render
            .render_block(PropagationRenderBlock {
                listener_orientation: orientation,
                sources: &source_blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        timings.push(started.elapsed().as_nanos() as u64);
    }
    assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
    timings.sort_unstable();
    let telemetry = simulation
        .quality_governor_telemetry()
        .expect("baked S6a session reports telemetry");
    Observation {
        p50_ns: percentile(&timings, 0.50),
        p95_ns: percentile(&timings, 0.95),
        p99_ns: percentile(&timings, 0.99),
        tracked_memory_bytes: telemetry.memory.tracked_current_bytes,
    }
}

fn pose(position: ApiEnuVector3) -> Pose {
    Pose {
        position,
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn percentile(sorted: &[u64], fraction: f64) -> u64 {
    let rank = ((sorted.len() as f64 * fraction).ceil() as usize)
        .max(1)
        .min(sorted.len())
        - 1;
    sorted[rank]
}

fn mesh_from_fixture(root: &Value) -> SceneMesh {
    let geometry = &root["geometry"];
    let vertices_enu_m = geometry["vertices_m"]
        .as_array()
        .unwrap()
        .iter()
        .map(vector)
        .map(|value| EnuVector3::new(value.east_m, value.north_m, value.up_m))
        .collect::<Vec<_>>();
    let triangles_json = geometry["triangles"].as_array().unwrap();
    let triangles = triangles_json
        .iter()
        .map(|triangle| {
            let values = triangle["indices"].as_array().unwrap();
            [
                values[0].as_i64().unwrap() as i32,
                values[1].as_i64().unwrap() as i32,
                values[2].as_i64().unwrap() as i32,
            ]
        })
        .collect::<Vec<_>>();
    SceneMesh {
        vertices_enu_m,
        material_indices: vec![0; triangles.len()],
        triangles,
        materials: vec![AcousticMaterial::MASONRY],
    }
}

fn probe_volume(root: &Value) -> ProbeVolume {
    let volume = &root["simulation"]["probe_volume"];
    let min = vector(&volume["min_m"]);
    let max = vector(&volume["max_m"]);
    ProbeVolume {
        min_enu_m: EnuVector3::new(min.east_m, min.north_m, min.up_m),
        max_enu_m: EnuVector3::new(max.east_m, max.north_m, max.up_m),
        spacing_m: volume["spacing_m"].as_f64().unwrap() as f32,
        height_above_floor_m: 1.5,
    }
}

fn source_positions(root: &Value) -> [ApiEnuVector3; 4] {
    let sources = root["sources"].as_array().unwrap();
    std::array::from_fn(|index| {
        sources[index]
            .get("position_m")
            .map(vector)
            .unwrap_or_else(|| vector(&sources[index]["trajectory"]["waypoints_m"][0]))
    })
}

fn vector(value: &Value) -> ApiEnuVector3 {
    let values = value.as_array().unwrap();
    ApiEnuVector3::new(
        values[0].as_f64().unwrap() as f32,
        values[1].as_f64().unwrap() as f32,
        values[2].as_f64().unwrap() as f32,
    )
}
