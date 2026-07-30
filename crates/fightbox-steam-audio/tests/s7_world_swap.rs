//! S7 prepared-generation adoption proof.

use fightbox_evidence::{WavSpec, summed_output_continuity};
use fightbox_runtime::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, SimulationRunner,
    SimulationUpdate, SourceMotion,
};
use fightbox_steam_audio::{
    AcousticMaterial, AudioConfig, EnuVector3, MultiSourceDescriptor, PathBakeConfig, ProbeVolume,
    ReflectionEffectConfig, S3BakeRequest, S3SimulationConfig, SceneMesh, StageOutputGains,
    WorldReflectionState, bake_s3, build_multi_source_session,
};
use serde_json::Value;
use std::f32::consts::TAU;
use std::time::Instant;

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: usize = 128;
const SWAP_BLOCK: usize = 24;
const TOTAL_BLOCKS: usize = 64;

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK"]
fn prepared_world_swap_is_continuous_generation_safe_and_truthful() {
    let room = mesh_from_fixture(include_str!("../../../fixtures/s4-room/fixture.json"));
    let canyon = mesh_from_fixture(include_str!("../../../fixtures/s4-canyon/fixture.json"));
    let probes = ProbeVolume {
        min_enu_m: EnuVector3::new(-1.0, 0.0, 0.25),
        max_enu_m: EnuVector3::new(1.0, 5.0, 2.75),
        spacing_m: 1.5,
        height_above_floor_m: 1.5,
    };
    let pathing = PathBakeConfig {
        num_visibility_samples: 1,
        probe_visibility_radius_m: 0.5,
        visibility_threshold: 0.1,
        visibility_range_m: 12.0,
        path_range_m: 20.0,
        num_threads: 1,
    };
    let room_bake = bake_s3(&S3BakeRequest {
        mesh: room.clone(),
        probes,
        pathing,
    })
    .expect("bake room generation");
    let canyon_bake = bake_s3(&S3BakeRequest {
        mesh: canyon.clone(),
        probes,
        pathing,
    })
    .expect("bake canyon generation");

    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES as i32,
    };
    let config = S3SimulationConfig {
        reflection_rays: 1_024,
        diffuse_samples: 32,
        reflection_bounces: 2,
        reflection_duration_s: 0.1,
        reflection_order: 1,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        pathing_order: 1,
        ..S3SimulationConfig::default()
    };
    let source_position = fightbox_api::EnuVector3::new(1.0, 1.0, 1.4);
    let listener_position = fightbox_api::EnuVector3::new(0.81, 4.0, 1.5);
    let descriptors = [MultiSourceDescriptor::at(source_position)];
    let update = update(source_position, listener_position);
    let (mut simulation, mut render) =
        build_multi_source_session(&room, &room_bake, audio, config, &descriptors)
            .expect("build active room generation");
    simulation.update_inputs(&update);
    simulation.run_direct().unwrap();
    simulation.run_pathing().unwrap();
    simulation.run_reflections().unwrap();
    let room_diagnostics = simulation.world_diagnostics();

    let mut prepared = simulation
        .prepare_world(&canyon, Some(&canyon_bake), config, &descriptors)
        .expect("prepare canyon while room remains renderable");
    prepared.update_inputs(&update);
    prepared.run_direct().unwrap();
    prepared.run_pathing().unwrap();
    prepared.run_reflections().unwrap();
    for _ in 0..5_000 {
        prepared.observe_render_timing(100_000);
    }
    prepared.run_direct().unwrap();
    prepared.run_pathing().unwrap();
    prepared.run_reflections().unwrap();
    assert_eq!(
        prepared
            .quality_governor_telemetry()
            .expect("prepared quality telemetry")
            .sources[0]
            .quality,
        fightbox_steam_audio::SourceQualityLevel::Full
    );
    let prepared_diagnostics = prepared.diagnostics();
    assert!(prepared.capabilities().baked_pathing);
    assert_eq!(
        prepared_diagnostics.generation,
        prepared.capabilities().generation
    );
    assert_eq!(
        prepared_diagnostics.reflection_ir_size,
        (config.reflection_duration_s * SAMPLE_RATE as f32).round() as i32
    );

    let mut summed = Vec::with_capacity(TOTAL_BLOCKS * BLOCK_FRAMES * 2);
    let mut prepared = Some(prepared);
    let mut ordinary_ns = Vec::new();
    let mut transition_ns = Vec::new();
    let mut global_frame = 0_usize;
    let mut receipt_generation = 0;
    let mut new_gain_control = None;
    for block_index in 0..TOTAL_BLOCKS {
        if block_index == SWAP_BLOCK {
            let receipt = simulation
                .swap_prepared_world(prepared.take().expect("single prepared swap"))
                .expect("publish prepared canyon without waiting for callback");
            receipt_generation = receipt.generation;
            new_gain_control = Some(receipt.stage_output_gain_control);
        }
        let input = (0..BLOCK_FRAMES)
            .map(|_| {
                let sample = (TAU * 440.0 * global_frame as f32 / SAMPLE_RATE as f32).sin() * 0.05;
                global_frame += 1;
                sample
            })
            .collect::<Vec<_>>();
        let sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: &input,
        }];
        let mut left = vec![0.0; BLOCK_FRAMES];
        let mut right = vec![0.0; BLOCK_FRAMES];
        let started = Instant::now();
        render
            .render_block(fightbox_runtime::backend::PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: fightbox_api::EnuVector3::new(0.0, 1.0, 0.0),
                    up: fightbox_api::EnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        let elapsed = started.elapsed().as_nanos() as u64;
        if (SWAP_BLOCK..SWAP_BLOCK + 8).contains(&block_index) {
            transition_ns.push(elapsed);
        } else if block_index >= 8 {
            ordinary_ns.push(elapsed);
        }
        for (left, right) in left.into_iter().zip(right) {
            assert!(left.is_finite() && right.is_finite());
            if block_index >= 8 {
                summed.extend_from_slice(&[left, right]);
            }
        }
    }

    let continuity = summed_output_continuity(
        WavSpec {
            sample_rate_hz: SAMPLE_RATE as u32,
            channels: 2,
        },
        &summed,
        BLOCK_FRAMES,
        32,
        0.5,
    )
    .expect("S7 summed-output continuity");
    assert_eq!(
        continuity.detected_click_count, 0,
        "prepared swap introduced a summed-output block-boundary click"
    );

    let delivered = simulation.delivered_world_state();
    assert_eq!(delivered.capabilities.generation, receipt_generation);
    assert!(delivered.capabilities.baked_pathing);
    assert_eq!(
        delivered.capabilities.reflections,
        WorldReflectionState::RealtimeConvolution
    );
    assert_eq!(delivered.transition_blocks_remaining, 0);
    let canyon_diagnostics = simulation.world_diagnostics();
    assert_eq!(canyon_diagnostics.generation, receipt_generation);
    assert_eq!(canyon_diagnostics, prepared_diagnostics);
    let propagation_changed = room_diagnostics.baked_data_fingerprint
        != canyon_diagnostics.baked_data_fingerprint
        && (room_diagnostics.path_eq != canyon_diagnostics.path_eq
            || room_diagnostics.path_sh_energy.to_bits()
                != canyon_diagnostics.path_sh_energy.to_bits());
    assert!(
        propagation_changed,
        "distinct room/canyon worlds published indistinguishable baked path proof values"
    );

    let mut gain_control = new_gain_control.expect("new generation gain producer");
    gain_control
        .publish(StageOutputGains {
            direct: 0.0,
            pathing: 1.0,
            reflections: 0.0,
        })
        .unwrap();
    let path_energy = render_stage_energy(&mut render, &mut global_frame, 8, false);
    gain_control
        .publish(StageOutputGains {
            direct: 0.0,
            pathing: 0.0,
            reflections: 1.0,
        })
        .unwrap();
    let reflection_energy = render_stage_energy(&mut render, &mut global_frame, 128, true);
    println!("S7 stage energy path={path_energy:.12e} reflection={reflection_energy:.12e}");
    assert!(
        path_energy > 1.0e-10,
        "new world's baked path stage was silent"
    );
    assert!(
        reflection_energy > 1.0e-16,
        "new world's convolution reflection stage was silent"
    );

    let swap_block_ns = transition_ns[0];
    ordinary_ns.sort_unstable();
    transition_ns.sort_unstable();
    let ordinary_p95 = percentile(&ordinary_ns, 0.95);
    let transition_max = *transition_ns.last().unwrap();
    assert!(
        transition_max <= ordinary_p95.saturating_mul(3),
        "swap render cost {transition_max}ns exceeded modest 3x envelope over ordinary p95 {ordinary_p95}ns"
    );
    println!(
        "S7 generation {} -> {}; fade_blocks=8 clicks={} max_boundary_ratio={:.4}; ordinary_p95={:.3}ms swap_block={:.3}ms transition_max={:.3}ms max_ratio={:.2}x; room_path_energy={:.6e} canyon_path_energy={:.6e} new_ir_size={} post_path_output={:.6e} post_reflection_output={:.6e}",
        room_diagnostics.generation,
        canyon_diagnostics.generation,
        continuity.detected_click_count,
        continuity.max_boundary_step_to_peak_ratio,
        ordinary_p95 as f64 / 1_000_000.0,
        swap_block_ns as f64 / 1_000_000.0,
        transition_max as f64 / 1_000_000.0,
        transition_max as f64 / ordinary_p95.max(1) as f64,
        room_diagnostics.path_sh_energy,
        canyon_diagnostics.path_sh_energy,
        canyon_diagnostics.reflection_ir_size,
        path_energy,
        reflection_energy,
    );

    let mut unbaked = simulation
        .prepare_world(&room, None, config, &descriptors)
        .expect("prepare complete unbaked world");
    assert!(!unbaked.capabilities().baked_pathing);
    unbaked.update_inputs(&update);
    unbaked.run_direct().unwrap();
    assert_eq!(
        unbaked.run_pathing(),
        Err(fightbox_runtime::backend::SimulationError::KernelFailure)
    );
    unbaked.run_reflections().unwrap();
    let unbaked_generation = unbaked.capabilities().generation;
    simulation
        .swap_prepared_world(unbaked)
        .expect("publish unbaked generation");
    render_stage_energy(&mut render, &mut global_frame, 8, false);
    let unbaked_delivered = simulation.delivered_world_state();
    assert_eq!(
        unbaked_delivered.capabilities.generation,
        unbaked_generation
    );
    assert!(!unbaked_delivered.capabilities.baked_pathing);
    assert_eq!(unbaked_delivered.transition_blocks_remaining, 0);
    println!(
        "S7 unbaked generation={} delivered_pathing={} reflection={:?}",
        unbaked_generation,
        unbaked_delivered.capabilities.baked_pathing,
        unbaked_delivered.capabilities.reflections
    );
}

fn render_stage_energy(
    render: &mut fightbox_steam_audio::SteamAudioRenderGraph,
    global_frame: &mut usize,
    blocks: usize,
    impulse: bool,
) -> f64 {
    let mut energy = 0.0;
    for block in 0..blocks {
        let input = (0..BLOCK_FRAMES)
            .map(|frame| {
                let sample = if impulse {
                    if block == 0 && frame == 0 { 0.5 } else { 0.0 }
                } else {
                    (TAU * 440.0 * *global_frame as f32 / SAMPLE_RATE as f32).sin() * 0.05
                };
                *global_frame += 1;
                sample
            })
            .collect::<Vec<_>>();
        let sources = [BackendSourceBlock {
            source_index: 0,
            input_mono: &input,
        }];
        let mut left = vec![0.0; BLOCK_FRAMES];
        let mut right = vec![0.0; BLOCK_FRAMES];
        render
            .render_block(fightbox_runtime::backend::PropagationRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: fightbox_api::EnuVector3::new(0.0, 1.0, 0.0),
                    up: fightbox_api::EnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        energy += left
            .into_iter()
            .chain(right)
            .map(|sample| f64::from(sample * sample))
            .sum::<f64>();
    }
    energy
}

fn update(
    source_position: fightbox_api::EnuVector3,
    listener_position: fightbox_api::EnuVector3,
) -> SimulationUpdate {
    let pose = |position| fightbox_api::Pose {
        position,
        forward: fightbox_api::EnuVector3::new(0.0, 1.0, 0.0),
        up: fightbox_api::EnuVector3::new(0.0, 0.0, 1.0),
    };
    let mut sources = [SourceMotion::default(); fightbox_runtime::backend::MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: pose(source_position),
        linear_velocity_mps: fightbox_api::EnuVector3::default(),
    };
    SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: pose(listener_position),
            linear_velocity_mps: fightbox_api::EnuVector3::default(),
        },
        sources,
    }
}

fn mesh_from_fixture(json: &str) -> SceneMesh {
    let root: Value = serde_json::from_str(json).unwrap();
    let geometry = &root["geometry"];
    let materials = geometry["materials"].as_object().unwrap();
    let material_names = materials.keys().cloned().collect::<Vec<_>>();
    let material_values = material_names
        .iter()
        .map(|name| {
            let value = &materials[name];
            AcousticMaterial {
                absorption: triple(&value["absorption"]),
                scattering: number(&value["scattering"]),
                transmission: triple(&value["transmission"]),
            }
        })
        .collect();
    let vertices_enu_m = geometry["vertices_m"]
        .as_array()
        .unwrap()
        .iter()
        .map(vec3)
        .collect();
    let triangles_json = geometry["triangles"].as_array().unwrap();
    let triangles = triangles_json
        .iter()
        .map(|triangle| {
            let indices = triangle["indices"].as_array().unwrap();
            [
                indices[0].as_i64().unwrap() as i32,
                indices[1].as_i64().unwrap() as i32,
                indices[2].as_i64().unwrap() as i32,
            ]
        })
        .collect();
    let material_indices = triangles_json
        .iter()
        .map(|triangle| {
            material_names
                .iter()
                .position(|name| name == triangle["material"].as_str().unwrap())
                .unwrap() as i32
        })
        .collect();
    SceneMesh {
        vertices_enu_m,
        triangles,
        material_indices,
        materials: material_values,
    }
}

fn vec3(value: &Value) -> EnuVector3 {
    let values = value.as_array().unwrap();
    EnuVector3::new(number(&values[0]), number(&values[1]), number(&values[2]))
}

fn triple(value: &Value) -> [f32; 3] {
    let values = value.as_array().unwrap();
    [number(&values[0]), number(&values[1]), number(&values[2])]
}

fn number(value: &Value) -> f32 {
    value.as_f64().unwrap() as f32
}

fn percentile(sorted: &[u64], quantile: f32) -> u64 {
    sorted[((sorted.len() - 1) as f32 * quantile).round() as usize]
}
