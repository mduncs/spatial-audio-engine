//! One-shot, ignored diagnosis for the reported megablock reflection inversion.
//!
//! This file is intentionally not part of a default gate. It reads the caller's
//! package and bake, then runs the same retained render graph as the workbench.

use fightbox_api::{EnuVector3, ListenerState, Pose, ReferenceLevel};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, MAX_ACTIVE_SOURCES,
    PropagationRenderBlock, SimulationRunner, SimulationUpdate, SourceMotion,
};
use fightbox_steam_audio::EnuVector3 as SteamEnuVector3;
use fightbox_steam_audio::{
    AcousticMaterial, AnomalyClass, AnomalyQuerySession, AudioConfig, BakedProbeBatch,
    DirectOcclusionMode, MultiSourceDescriptor, PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata,
    ReflectionEffectConfig, S3SimulationConfig, STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION,
    SceneMesh, StageOutputGains, build_multi_source_session, classify_sample_at_distance,
};
use serde_json::Value;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const LISTENER: EnuVector3 = EnuVector3::new(108.06, 303.91, 1.50);
const SOURCE_EAST_M: f32 = 292.5;
const SOURCE_NORTH_M: f32 = 292.5;
const EFFECTIVE_SPL_AT_ONE_METER_DB: f32 = 129.0;
const TARGET_SOURCE_RMS_DBFS: f32 = -15.0;
const SETTLE_BLOCKS: usize = 800;
const MEASURE_BLOCKS: usize = 750;
const ZERO_PATH_EXEMPLARS: [EnuVector3; 7] = [
    EnuVector3::new(275.0, 280.0, 1.5),
    EnuVector3::new(310.0, 281.0, 1.5),
    EnuVector3::new(279.0, 310.0, 1.5),
    EnuVector3::new(305.0, 310.0, 1.5),
    EnuVector3::new(280.0, 280.0, 1.5),
    EnuVector3::new(303.0, 281.0, 1.5),
    EnuVector3::new(292.5, 292.5, 1.5),
];
const ZERO_PATH_GEOMETRY_CASES: [(&str, EnuVector3, bool); 8] = [
    ("zero-sw", EnuVector3::new(275.0, 280.0, 1.5), true),
    ("zero-se", EnuVector3::new(310.0, 281.0, 1.5), true),
    ("zero-nw", EnuVector3::new(279.0, 310.0, 1.5), true),
    ("zero-ne", EnuVector3::new(305.0, 310.0, 1.5), true),
    ("street-sw", EnuVector3::new(280.0, 280.0, 1.5), false),
    ("street-se", EnuVector3::new(303.0, 281.0, 1.5), false),
    ("street-ne", EnuVector3::new(304.0, 305.0, 1.5), false),
    ("source", EnuVector3::new(292.5, 292.5, 1.5), false),
];
const INVALID_COEFFICIENT_EXEMPLARS: [EnuVector3; 10] = [
    EnuVector3::new(394.0, 6.0, 1.5),
    EnuVector3::new(190.0, 10.0, 1.5),
    EnuVector3::new(198.0, 10.0, 1.5),
    EnuVector3::new(402.0, 10.0, 1.5),
    EnuVector3::new(390.0, 14.0, 1.5),
    EnuVector3::new(190.0, 18.0, 1.5),
    EnuVector3::new(198.0, 18.0, 1.5),
    EnuVector3::new(398.0, 18.0, 1.5),
    EnuVector3::new(394.0, 26.0, 1.5),
    EnuVector3::new(394.0, 102.0, 1.5),
];

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

#[derive(Clone, Copy)]
struct Noise {
    state: u64,
}

impl Noise {
    fn fill(&mut self, output: &mut [f32]) {
        let amplitude = 10.0_f32.powf(TARGET_SOURCE_RMS_DBFS / 20.0) * 3.0_f32.sqrt();
        for sample in output {
            let mut value = self.state;
            value ^= value << 13;
            value ^= value >> 7;
            value ^= value << 17;
            self.state = value;
            let unit = ((value >> 40) as u32) as f32 / 16_777_215.0;
            *sample = (unit * 2.0 - 1.0) * amplitude;
        }
    }
}

#[test]
#[ignore = "requires FIGHTBOX_DIAG_PACKAGE/FIGHTBOX_DIAG_BAKE and linked Steam Audio"]
fn megablock_above_rooves_reflection_inversion() {
    let package = required_path("FIGHTBOX_DIAG_PACKAGE");
    let bake = required_path("FIGHTBOX_DIAG_BAKE");
    let mesh = load_mesh(&package);
    let baked = load_baked(&bake);
    let coverage = baked
        .probe_coverage()
        .expect("parse probe influence spheres");

    println!(
        "REFL_INV_INPUT package={} bake={} probes={} listener=[{:.2},{:.2},{:.2}] yaw_deg=176.3 source_xy=[{SOURCE_EAST_M:.1},{SOURCE_NORTH_M:.1}] effective_spl1m_db={EFFECTIVE_SPL_AT_ONE_METER_DB:.1} target_source_rms_dbfs={TARGET_SOURCE_RMS_DBFS:.1}",
        package.display(),
        bake.display(),
        baked.metadata.probe_count,
        LISTENER.east_m,
        LISTENER.north_m,
        LISTENER.up_m,
    );

    for height_m in [63.0_f32, 60.0, 35.0, 1.5] {
        let source = EnuVector3::new(SOURCE_EAST_M, SOURCE_NORTH_M, height_m);
        let hits = segment_hits(&mesh, LISTENER, source);
        println!(
            "REFL_INV_GEOMETRY z={height_m:.1} slant_m={:.3} elevation_deg={:.3} segment_triangle_hits={} first_hit_distance_m={} first_hit_z_m={} source_probe={} listener_probe={}",
            distance(LISTENER, source),
            ((height_m - LISTENER.up_m) / horizontal_distance(LISTENER, source))
                .atan()
                .to_degrees(),
            hits.len(),
            hits.first()
                .map(|hit| format!("{:.3}", hit.0))
                .unwrap_or_else(|| "none".into()),
            hits.first()
                .map(|hit| format!("{:.3}", hit.1))
                .unwrap_or_else(|| "none".into()),
            coverage.contains(to_steam_enu(source)),
            coverage.contains(to_steam_enu(LISTENER)),
        );
        measure(
            &mesh,
            &baked,
            source,
            DirectOcclusionMode::Volumetric {
                radius_m: 1.0,
                sample_count: 64,
            },
            "volumetric-r1-n64",
        );
    }

    measure(
        &mesh,
        &baked,
        EnuVector3::new(SOURCE_EAST_M, SOURCE_NORTH_M, 63.0),
        DirectOcclusionMode::Raycast,
        "raycast-control",
    );
}

#[test]
#[ignore = "requires FIGHTBOX_DIAG_PACKAGE/FIGHTBOX_DIAG_BAKE and linked Steam Audio"]
fn megablock_zero_path_cluster_real_render() {
    let package = required_path("FIGHTBOX_DIAG_PACKAGE");
    let bake = required_path("FIGHTBOX_DIAG_BAKE");
    let mesh = load_mesh(&package);
    let baked = load_baked(&bake);
    let coverage = baked
        .probe_coverage()
        .expect("parse probe influence spheres");
    let config = S3SimulationConfig {
        pathing_visibility_range_m: 10.0,
        simulation_threads: 1,
        ..S3SimulationConfig::default()
    };
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };

    for source_height_m in [1.5_f32, 63.0] {
        let source = EnuVector3::new(SOURCE_EAST_M, SOURCE_NORTH_M, source_height_m);
        let descriptor = MultiSourceDescriptor::at(source)
            .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 105.0 });
        let mut query = AnomalyQuerySession::new(&mesh, &baked, config, descriptor)
            .expect("build anomaly query session");
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, config, &[descriptor])
                .expect("build retained render session");
        let mut stage_control = render
            .take_stage_output_gain_control()
            .expect("take stage output control");
        stage_control
            .publish(PATH_ONLY)
            .expect("select path-only output");
        let source_probes = influencing_probes(&coverage, to_steam_enu(source));

        for listener in ZERO_PATH_EXEMPLARS {
            let query_sample = query
                .sample(SteamEnuVector3::new(
                    listener.east_m,
                    listener.north_m,
                    listener.up_m,
                ))
                .expect("query exemplar");
            simulation.update_inputs(&one_source_update_at(source, listener));
            simulation.run_direct().expect("direct simulation");
            simulation.run_pathing().expect("path simulation");
            let render_diagnostics = simulation
                .source_diagnostics(0)
                .expect("source zero diagnostics");
            let mut noise = Noise {
                state: 0x9e37_79b9_7f4a_7c15,
            };
            let path_dbfs = measure_stage(&mut render, &mut stage_control, &mut noise, PATH_ONLY);
            let listener_probes = influencing_probes(&coverage, to_steam_enu(listener));
            println!(
                "ZERO_PATH_COMPARE source_z={source_height_m:.1} listener=[{:.1},{:.1},{:.1}] query_direct={:.9e} query_sh_energy={:.9e} render_direct={:.9e} render_sh_energy={:.9e} render_path_dbfs={path_dbfs:.6} source_influences={} listener_influences={} shared_influences={} nearest_source_probe={} nearest_listener_probe={}",
                listener.east_m,
                listener.north_m,
                listener.up_m,
                query_sample.direct_audibility,
                query_sample.path_sh_energy,
                render_diagnostics.occlusion,
                render_diagnostics.path_sh_energy,
                source_probes.len(),
                listener_probes.len(),
                shared_probe_count(&source_probes, &listener_probes),
                nearest_probe(&source_probes),
                nearest_probe(&listener_probes),
            );
        }
    }
}

#[test]
#[ignore = "requires FIGHTBOX_DIAG_PACKAGE"]
fn megablock_zero_path_exemplars_are_inside_building_solids() {
    let package = required_path("FIGHTBOX_DIAG_PACKAGE");
    let mesh = load_mesh(&package);
    for (label, listener, expected_inside) in ZERO_PATH_GEOMETRY_CASES {
        let above = EnuVector3::new(listener.east_m, listener.north_m, 1_000.0);
        let mut roof_heights = segment_hits(&mesh, listener, above)
            .into_iter()
            .map(|(_, height)| height)
            .collect::<Vec<_>>();
        roof_heights.sort_by(f32::total_cmp);
        roof_heights.dedup_by(|left, right| (*left - *right).abs() < 1.0e-3);
        let inside = !roof_heights.is_empty();
        println!(
            "ZERO_PATH_GEOMETRY label={label} listener=[{:.1},{:.1},{:.1}] inside_building={inside} overhead_surface_heights_m={roof_heights:?}",
            listener.east_m, listener.north_m, listener.up_m,
        );
        assert_eq!(
            inside, expected_inside,
            "geometry classification for {label}"
        );
    }
}

#[test]
#[ignore = "requires FIGHTBOX_DIAG_PACKAGE/FIGHTBOX_DIAG_BAKE and linked Steam Audio"]
fn megablock_invalid_path_coefficient_query_matches_real_render_chain() {
    let package = required_path("FIGHTBOX_DIAG_PACKAGE");
    let bake = required_path("FIGHTBOX_DIAG_BAKE");
    let mesh = load_mesh(&package);
    let baked = load_baked(&bake);
    let config = S3SimulationConfig {
        pathing_visibility_range_m: 10.0,
        simulation_threads: 1,
        ..S3SimulationConfig::default()
    };
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };

    for source_height_m in [1.5_f32, 63.0] {
        let source = EnuVector3::new(SOURCE_EAST_M, SOURCE_NORTH_M, source_height_m);
        let descriptor = MultiSourceDescriptor::at(source)
            .with_reference_level(ReferenceLevel::SplAtOneMeter { db_spl: 105.0 });
        let mut query = AnomalyQuerySession::new(&mesh, &baked, config, descriptor)
            .expect("build anomaly query session");
        let (mut simulation, mut render) =
            build_multi_source_session(&mesh, &baked, audio, config, &[descriptor])
                .expect("build retained render session");
        let mut stage_control = render
            .take_stage_output_gain_control()
            .expect("take stage output control");
        stage_control
            .publish(PATH_ONLY)
            .expect("select path-only output");

        for (index, listener) in INVALID_COEFFICIENT_EXEMPLARS.into_iter().enumerate() {
            let query_sample = query
                .sample(SteamEnuVector3::new(
                    listener.east_m,
                    listener.north_m,
                    listener.up_m,
                ))
                .expect("query exemplar");
            let query_cell =
                classify_sample_at_distance(query_sample, 105.0, distance(source, listener));
            assert!(
                !query_cell.flags.contains(AnomalyClass::InvalidCoefficient),
                "unnormalized path EQ at {listener:?} is not a normalized coefficient failure"
            );
            simulation.update_inputs(&one_source_update_at(source, listener));
            simulation.run_direct().expect("direct simulation");
            simulation.run_pathing().expect("path simulation");
            let render_diagnostics = simulation
                .source_diagnostics(0)
                .expect("source zero diagnostics");
            let mut noise = Noise {
                state: 0x9e37_79b9_7f4a_7c15,
            };
            let path_dbfs = (index == 0)
                .then(|| measure_stage(&mut render, &mut stage_control, &mut noise, PATH_ONLY));
            let above = EnuVector3::new(listener.east_m, listener.north_m, 1_000.0);
            let overhead_hits = segment_hits(&mesh, listener, above).len();
            println!(
                "INVALID_COEFF_COMPARE source_z={source_height_m:.1} listener=[{:.1},{:.1},{:.1}] distance_m={:.6} query_direct={:.9e} query_eq={:?} query_sh_energy={:.9e} source_probe={} listener_probe={} source_inside={} listener_inside={} render_distance={:.9e} render_direct={:.9e} render_transmission={:?} render_air={:?} render_eq={:?} render_sh_energy={:.9e} render_path_dbfs={path_dbfs:?} overhead_hits={overhead_hits}",
                listener.east_m,
                listener.north_m,
                listener.up_m,
                distance(source, listener),
                query_sample.direct_audibility,
                query_sample.path_eq,
                query_sample.path_sh_energy,
                query_sample.source_probe_covered,
                query_sample.listener_probe_covered,
                query_sample.source_endpoint_inside_static_geometry,
                query_sample.listener_endpoint_inside_static_geometry,
                render_diagnostics.distance_attenuation,
                render_diagnostics.occlusion,
                render_diagnostics.transmission,
                render_diagnostics.air_absorption,
                render_diagnostics.path_eq,
                render_diagnostics.path_sh_energy,
            );
        }
    }
}

fn measure(
    mesh: &SceneMesh,
    baked: &BakedProbeBatch,
    source: EnuVector3,
    direct_occlusion: DirectOcclusionMode,
    occlusion_label: &str,
) {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = S3SimulationConfig {
        max_occlusion_samples: 64,
        direct_occlusion,
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 3,
        reflection_duration_s: 1.5,
        reflection_order: 1,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 2,
        pathing_visibility_range_m: 10.0,
        validate_paths: true,
        find_alternate_paths: true,
        ..S3SimulationConfig::default()
    };
    let descriptors =
        [
            MultiSourceDescriptor::at(source).with_reference_level(ReferenceLevel::SplAtOneMeter {
                db_spl: EFFECTIVE_SPL_AT_ONE_METER_DB,
            }),
        ];
    let (mut simulation, mut render) =
        build_multi_source_session(mesh, baked, audio, config, &descriptors)
            .expect("build retained diagnostic session");
    let mut stage_control = render
        .take_stage_output_gain_control()
        .expect("take stage output control");
    simulation.update_inputs(&one_source_update_at(source, LISTENER));
    simulation.run_direct().expect("direct simulation");
    simulation.run_pathing().expect("path simulation");
    simulation.run_reflections().expect("reflection simulation");

    // Give the one-source diagnostic enough synthetic headroom observations to
    // reach the requested Full/4096/3/1.5 s graph before comparing buses.
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
    }
    let quality = simulation
        .quality_governor_telemetry()
        .expect("baked session exposes governor telemetry");
    let diagnostics = simulation
        .source_diagnostics(0)
        .expect("source zero diagnostics");
    let mut noise = Noise {
        state: 0x9e37_79b9_7f4a_7c15,
    };
    let direct_dbfs = measure_stage(&mut render, &mut stage_control, &mut noise, DIRECT_ONLY);
    let path_dbfs = measure_stage(&mut render, &mut stage_control, &mut noise, PATH_ONLY);
    let reflections_dbfs = measure_stage(
        &mut render,
        &mut stage_control,
        &mut noise,
        REFLECTIONS_ONLY,
    );
    println!(
        "REFL_INV_LEVEL z={:.1} occlusion_mode={occlusion_label} direct_dbfs={direct_dbfs:.3} path_dbfs={path_dbfs:.3} reflections_dbfs={reflections_dbfs:.3} refl_minus_direct_db={:.3} refl_minus_path_db={:.3} distance_gain={:.9e} occlusion={:.9e} transmission={:?} air={:?} path_eq={:?} path_sh_energy={:.9e} reflection_ir_size={} quality_source={:?} quality_reflections={:?} quality_reverb={:?} reflection_output_gain={:.9e}",
        source.up_m,
        reflections_dbfs - direct_dbfs,
        reflections_dbfs - path_dbfs,
        diagnostics.distance_attenuation,
        diagnostics.occlusion,
        diagnostics.transmission,
        diagnostics.air_absorption,
        diagnostics.path_eq,
        diagnostics.path_sh_energy,
        diagnostics.reflection_ir_size,
        quality.sources[0].quality,
        quality.reflections.level,
        quality.reverb,
        quality.reflection_output_gain,
    );
}

fn measure_stage(
    render: &mut fightbox_steam_audio::SteamAudioRenderGraph,
    control: &mut fightbox_steam_audio::StageOutputGainControl,
    noise: &mut Noise,
    gains: StageOutputGains,
) -> f64 {
    control
        .publish(gains)
        .expect("valid diagnostic stage gains");
    for _ in 0..SETTLE_BLOCKS {
        render_noise_block(render, noise, None);
    }
    let mut squared_sum = 0.0_f64;
    for _ in 0..MEASURE_BLOCKS {
        render_noise_block(render, noise, Some(&mut squared_sum));
    }
    let rms = (squared_sum / (MEASURE_BLOCKS * BLOCK_FRAMES as usize * 2) as f64).sqrt();
    if rms > 0.0 {
        20.0 * rms.log10()
    } else {
        f64::NEG_INFINITY
    }
}

fn render_noise_block(
    render: &mut fightbox_steam_audio::SteamAudioRenderGraph,
    noise: &mut Noise,
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
    let yaw = 176.3_f32.to_radians();
    render
        .render_block(PropagationRenderBlock {
            listener_orientation: ListenerOrientation {
                forward: EnuVector3::new(yaw.sin(), yaw.cos(), 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            sources: &sources,
            output_left: &mut left,
            output_right: &mut right,
        })
        .expect("render retained diagnostic block");
    if let Some(sum) = squared_sum {
        for sample in left.into_iter().chain(right) {
            *sum += f64::from(sample) * f64::from(sample);
        }
    }
}

fn one_source_update_at(source: EnuVector3, listener_position: EnuVector3) -> SimulationUpdate {
    let yaw = 176.3_f32.to_radians();
    let listener = ListenerState {
        pose: Pose {
            position: listener_position,
            forward: EnuVector3::new(yaw.sin(), yaw.cos(), 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        linear_velocity_mps: EnuVector3::default(),
    };
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: Pose {
            position: source,
            forward: EnuVector3::new(0.0, 1.0, 0.0),
            up: EnuVector3::new(0.0, 0.0, 1.0),
        },
        linear_velocity_mps: EnuVector3::default(),
    };
    SimulationUpdate { listener, sources }
}

fn influencing_probes(
    coverage: &fightbox_steam_audio::ProbeCoverage<'_>,
    position: SteamEnuVector3,
) -> Vec<(usize, f32)> {
    coverage
        .spheres()
        .enumerate()
        .filter_map(|(index, (center, radius))| {
            let distance = ((center.x - position.x).powi(2)
                + (center.y - position.y).powi(2)
                + (center.z - position.z).powi(2))
            .sqrt();
            (distance <= radius).then_some((index, distance))
        })
        .collect()
}

fn shared_probe_count(left: &[(usize, f32)], right: &[(usize, f32)]) -> usize {
    left.iter()
        .filter(|(left_index, _)| {
            right
                .iter()
                .any(|(right_index, _)| left_index == right_index)
        })
        .count()
}

fn nearest_probe(probes: &[(usize, f32)]) -> String {
    probes
        .iter()
        .min_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(index, distance)| format!("{index}@{distance:.3}m"))
        .unwrap_or_else(|| "none".into())
}

fn segment_hits(mesh: &SceneMesh, from: EnuVector3, to: EnuVector3) -> Vec<(f32, f32)> {
    let direction = subtract(to, from);
    let length = distance(from, to);
    let mut hits = Vec::new();
    for triangle in &mesh.triangles {
        let a = to_api_enu(mesh.vertices_enu_m[triangle[0] as usize]);
        let b = to_api_enu(mesh.vertices_enu_m[triangle[1] as usize]);
        let c = to_api_enu(mesh.vertices_enu_m[triangle[2] as usize]);
        if let Some(t) = segment_triangle_t(from, direction, a, b, c) {
            let point = add(from, scale(direction, t));
            hits.push((length * t, point.up_m));
        }
    }
    hits.sort_by(|left, right| left.0.total_cmp(&right.0));
    hits
}

fn segment_triangle_t(
    origin: EnuVector3,
    direction: EnuVector3,
    a: EnuVector3,
    b: EnuVector3,
    c: EnuVector3,
) -> Option<f32> {
    let edge1 = subtract(b, a);
    let edge2 = subtract(c, a);
    let p = cross(direction, edge2);
    let determinant = dot(edge1, p);
    if determinant.abs() < 1.0e-7 {
        return None;
    }
    let inverse = determinant.recip();
    let tvec = subtract(origin, a);
    let u = dot(tvec, p) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(tvec, edge1);
    let v = dot(direction, q) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = dot(edge2, q) * inverse;
    (t > 1.0e-5 && t < 1.0 - 1.0e-5).then_some(t)
}

fn load_mesh(package: &Path) -> SceneMesh {
    let bytes = fs::read(package.join("mesh.bin")).expect("read mesh.bin");
    assert!(bytes.len() >= 20 && &bytes[..8] == b"FBXMESH\0");
    let vertex_count = read_u32(&bytes, 12) as usize;
    let triangle_count = read_u32(&bytes, 16) as usize;
    let mut cursor = 20;
    let mut vertices = Vec::with_capacity(vertex_count);
    for _ in 0..vertex_count {
        vertices.push(SteamEnuVector3::new(
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
    let json: Value = serde_json::from_slice(
        &fs::read(package.join("materials.json")).expect("read materials.json"),
    )
    .expect("parse materials.json");
    let materials = ["asphalt", "brick", "concrete", "glass", "grass"]
        .map(|name| material(&json[name]))
        .to_vec();
    SceneMesh {
        vertices_enu_m: vertices,
        triangles,
        material_indices,
        materials,
    }
}

fn material(value: &Value) -> AcousticMaterial {
    let band = |field: &str| {
        let values = value[field].as_array().expect("material band array");
        [0, 1, 2].map(|index| values[index].as_f64().expect("material band number") as f32)
    };
    AcousticMaterial {
        absorption: band("absorption"),
        scattering: value["scattering"].as_f64().expect("material scattering") as f32,
        transmission: band("transmission"),
    }
}

fn load_baked(path: &Path) -> BakedProbeBatch {
    let metadata: Value = serde_json::from_slice(
        &fs::read(path.join("probe-batch-metadata.json")).expect("read probe metadata"),
    )
    .expect("parse probe metadata");
    let number = |field: &str| {
        metadata[field]
            .as_u64()
            .unwrap_or_else(|| panic!("{field}"))
    };
    let baked = BakedProbeBatch {
        metadata: ProbeBatchMetadata {
            schema_version: PROBE_BATCH_METADATA_SCHEMA,
            steam_audio_version: STEAM_AUDIO_VERSION,
            upstream_commit: STEAM_AUDIO_UPSTREAM_COMMIT,
            probe_count: number("probe_count") as u32,
            path_data_size_bytes: number("path_data_size_bytes"),
            serialized_size_bytes: number("serialized_size_bytes"),
            content_sha256: metadata["content_sha256"]
                .as_str()
                .expect("content hash")
                .into(),
            bake_progress_callback_count: number("bake_progress_callback_count") as u32,
            final_bake_progress_millionths: number("final_bake_progress_millionths") as u32,
        },
        bytes: fs::read(path.join("probe-batch.bin")).expect("read probe batch"),
    };
    baked.validate().expect("validate probe batch");
    baked
}

fn required_path(variable: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("{variable} must name a diagnostic input"))
}

fn horizontal_distance(a: EnuVector3, b: EnuVector3) -> f32 {
    (a.east_m - b.east_m).hypot(a.north_m - b.north_m)
}

fn distance(a: EnuVector3, b: EnuVector3) -> f32 {
    let delta = subtract(a, b);
    dot(delta, delta).sqrt()
}

fn add(a: EnuVector3, b: EnuVector3) -> EnuVector3 {
    EnuVector3::new(a.east_m + b.east_m, a.north_m + b.north_m, a.up_m + b.up_m)
}

fn subtract(a: EnuVector3, b: EnuVector3) -> EnuVector3 {
    EnuVector3::new(a.east_m - b.east_m, a.north_m - b.north_m, a.up_m - b.up_m)
}

fn scale(a: EnuVector3, value: f32) -> EnuVector3 {
    EnuVector3::new(a.east_m * value, a.north_m * value, a.up_m * value)
}

fn dot(a: EnuVector3, b: EnuVector3) -> f32 {
    a.east_m * b.east_m + a.north_m * b.north_m + a.up_m * b.up_m
}

fn cross(a: EnuVector3, b: EnuVector3) -> EnuVector3 {
    EnuVector3::new(
        a.north_m * b.up_m - a.up_m * b.north_m,
        a.up_m * b.east_m - a.east_m * b.up_m,
        a.east_m * b.north_m - a.north_m * b.east_m,
    )
}

fn to_steam_enu(value: EnuVector3) -> SteamEnuVector3 {
    SteamEnuVector3::new(value.east_m, value.north_m, value.up_m)
}

fn to_api_enu(value: SteamEnuVector3) -> EnuVector3 {
    EnuVector3::new(value.x, value.y, value.z)
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}
