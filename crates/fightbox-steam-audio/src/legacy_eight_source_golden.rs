//! Immutable Wave 0 reference for the complete legacy eight-source mix.
//!
//! Golden recipe, schema `legacy-eight-point-final-mix/v1`:
//!
//! - Steam Audio 4.8.1; 48 kHz; 128-frame blocks; Desktop quality; unbaked
//!   floor-only scene; direct raycast simulation once; direct stage gain 1;
//!   path, reflection, echo, and source reflection sends all 0/off.
//! - Listener `(0, 0, 1.5)` m, facing north with +Z up. The eight fixed Point,
//!   omnidirectional sources use `SOURCE_POSITIONS_ENU_M` and declared levels
//!   `SOURCE_SPL_AT_ONE_METER_DB` in source-index order.
//! - Runtime propagation keeps all eight logical sources active with zero
//!   backend-neutral delay and unity L/R gain. Steam owns distance delay,
//!   distance loss, direct filtering, and HRTF. `RuntimeGraph` owns the one
//!   calibrated source drive, the default output-safety publication (+30 dB
//!   monitor gain), and the final -1 dBTP lookahead limiter.
//! - Each source starts from `INPUT_SEED_BASE + INPUT_SEED_STRIDE * (index+1)`
//!   modulo 2^64. Each sample applies xorshift64* shifts 12/25/27, multiplies
//!   by `0x2545F4914F6CDD1D`, maps the high 24 bits to `[-1, 1)`, and scales by
//!   `1e-4`. A source mask replaces its samples with exact zero after advancing
//!   the generator, so isolation sessions retain the same source clocks.
//! - Discard 32 blocks (4,096 frames, 85.333 ms), then capture 64 blocks
//!   (8,192 frames, 170.667 ms). Hash interleaved L/R final-output `f32` bit
//!   patterns as little-endian bytes.

use super::{MultiSourceRenderGraph, build_multi_source_generation};
use crate::{
    AcousticMaterial, AudioConfig, DirectOcclusionMode, MultiSourceDescriptor, QualityTier,
    ReflectionEffectConfig, S3SimulationConfig, STEAM_AUDIO_VERSION, SceneMesh, StageOutputGains,
};
use fightbox_api::{
    AssetAnalysis, AssetMeasurementProvenance, Directivity, EngineConfig,
    EnuVector3 as ApiEnuVector3, ExtentDescriptor, ListenerState, OutputSafetyConfig, Pose,
    ReferenceLevel, SceneCalibration, SourceId, SourceProfile,
};
use fightbox_runtime::backend::{
    BackendRenderError, BackendRenderGraph, MAX_ACTIVE_SOURCES, PropagationRenderBlock,
    SimulationUpdate, SourceMotion,
};
use fightbox_runtime::{
    BlockProcessor, OutputSafetyPublication, ProcessBlock, PropagationSnapshot, RuntimeGraph,
    SnapshotPublication, SourceBlock, SourcePropagation,
};

const SOURCE_COUNT: usize = 8;
const SAMPLE_RATE_HZ: u32 = 48_000;
const BLOCK_FRAMES: usize = 128;
const WARMUP_BLOCKS: usize = 32;
const CAPTURE_BLOCKS: usize = 64;
const ALL_SOURCES_MASK: u8 = u8::MAX;
const INPUT_PEAK: f32 = 1.0e-4;
const INPUT_SEED_BASE: u64 = 0xD1B5_4A32_D192_ED03;
const INPUT_SEED_STRIDE: u64 = 0x9E37_79B9_7F4A_7C15;
const XORSHIFT64_STAR_MULTIPLIER: u64 = 0x2545_F491_4F6C_DD1D;
const ASSET_PROGRAM_RMS_DBFS: f32 = -84.77;
const ASSET_TRUE_PEAK_DBTP: f32 = -80.0;
const FIXTURE_GENERATION: u64 = 17;

const LISTENER_POSITION_ENU_M: ApiEnuVector3 = ApiEnuVector3::new(0.0, 0.0, 1.5);
const SOURCE_POSITIONS_ENU_M: [ApiEnuVector3; SOURCE_COUNT] = [
    ApiEnuVector3::new(3.0, 4.0, 1.5),
    ApiEnuVector3::new(-4.0, 3.0, 1.5),
    ApiEnuVector3::new(-5.0, -2.0, 1.5),
    ApiEnuVector3::new(2.0, -6.0, 1.5),
    ApiEnuVector3::new(8.0, 1.0, 1.5),
    ApiEnuVector3::new(-1.0, 9.0, 1.5),
    ApiEnuVector3::new(6.0, -7.0, 1.5),
    ApiEnuVector3::new(-8.0, -6.0, 1.5),
];
const SOURCE_SPL_AT_ONE_METER_DB: [f32; SOURCE_COUNT] =
    [86.0, 88.0, 90.0, 92.0, 94.0, 96.0, 98.0, 100.0];

// Frozen only after two entirely fresh sessions compared bit-for-bit equal.
// Re-pinned when direct air moved to the shared ISO 9613-1 exponential model.
const LEGACY_EIGHT_SOURCE_FINAL_MIX_SHA256: &str =
    "02b81c7b65fcb485f1d4083f57981f0da310b195e995fe4cf9736270d47074bb";
// The separate predecessor assertion
// `weight_zero_unbaked_direct_render_is_bit_identical_to_the_prechange_fingerprint`
// continues to freeze the one-source direct/HRTF hash as
// `e43a455e6eda686dbea905e16c434474406f55bf7db6356f2d145d24137a27ef`.

struct LegacyDirectBackend(MultiSourceRenderGraph);

impl BackendRenderGraph for LegacyDirectBackend {
    fn render_block(
        &mut self,
        block: PropagationRenderBlock<'_>,
    ) -> Result<(), BackendRenderError> {
        self.0.render_block(block)
    }
}

struct FixtureCapture {
    interleaved: Vec<f32>,
    limiter_engagements: u64,
    proximity_ceiling_engagements: u64,
    backend_render_errors: u64,
}

fn pose_at(position: ApiEnuVector3) -> Pose {
    Pose {
        position,
        forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
        up: ApiEnuVector3::new(0.0, 0.0, 1.0),
    }
}

fn fixture_scene() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            crate::EnuVector3::new(-1_024.0, -1_024.0, 0.0),
            crate::EnuVector3::new(1_024.0, -1_024.0, 0.0),
            crate::EnuVector3::new(1_024.0, 1_024.0, 0.0),
            crate::EnuVector3::new(-1_024.0, 1_024.0, 0.0),
        ],
        triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
        material_indices: vec![0; 4],
        materials: vec![AcousticMaterial::GROUND],
    }
}

const fn fixture_simulation_config() -> S3SimulationConfig {
    S3SimulationConfig {
        air_pressure_exponents_per_m: fightbox_runtime::FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M,
        max_occlusion_samples: 64,
        direct_occlusion: DirectOcclusionMode::Raycast,
        reflection_rays: 64,
        diffuse_samples: 8,
        reflection_bounces: 1,
        reflection_duration_s: 0.05,
        reflection_order: 1,
        reflection_effect: ReflectionEffectConfig::CONVOLUTION,
        simulation_threads: 1,
        ray_batch_size: 64,
        pathing_order: 1,
        pathing_visibility_samples: 1,
        pathing_visibility_radius_m: 0.0,
        pathing_visibility_threshold: 0.5,
        pathing_visibility_range_m: 6.0,
        validate_paths: false,
        find_alternate_paths: false,
        trace_path_validation: false,
    }
}

fn source_profile(source_index: usize) -> SourceProfile {
    SourceProfile {
        id: SourceId::new(format!("legacy-golden-point-{source_index}")),
        pose: pose_at(SOURCE_POSITIONS_ENU_M[source_index]),
        reference_level: ReferenceLevel::SplAtOneMeter {
            db_spl: SOURCE_SPL_AT_ONE_METER_DB[source_index],
        },
        asset_analysis: AssetAnalysis::new(
            ASSET_PROGRAM_RMS_DBFS,
            ASSET_TRUE_PEAK_DBTP,
            AssetMeasurementProvenance::new("legacy-eight-point-final-mix/v1").unwrap(),
        )
        .unwrap(),
        extent: ExtentDescriptor::Point,
        directivity: Directivity::OMNIDIRECTIONAL,
        max_speed_mps: 0.0,
    }
}

fn next_input_sample(state: &mut u64) -> f32 {
    let mut value = *state;
    value ^= value >> 12;
    value ^= value << 25;
    value ^= value >> 27;
    *state = value;
    let random = value.wrapping_mul(XORSHIFT64_STAR_MULTIPLIER);
    let high_24_bits = (random >> 40) as u32;
    let unit_signed = high_24_bits as f32 * (1.0 / 8_388_608.0) - 1.0;
    unit_signed * INPUT_PEAK
}

fn render_fixture(active_program_mask: u8) -> FixtureCapture {
    assert_eq!(STEAM_AUDIO_VERSION, "4.8.1");
    assert_eq!(MAX_ACTIVE_SOURCES, 16);
    assert_ne!(active_program_mask, 0);
    assert_eq!(active_program_mask & !ALL_SOURCES_MASK, 0);

    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE_HZ as i32,
        frame_size: BLOCK_FRAMES as i32,
    };
    let descriptors = std::array::from_fn::<_, SOURCE_COUNT, _>(|source_index| {
        MultiSourceDescriptor::at(SOURCE_POSITIONS_ENU_M[source_index])
            .with_initial_pose(pose_at(SOURCE_POSITIONS_ENU_M[source_index]))
            .with_reference_level(ReferenceLevel::SplAtOneMeter {
                db_spl: SOURCE_SPL_AT_ONE_METER_DB[source_index],
            })
            .with_directivity(Directivity::OMNIDIRECTIONAL)
            .with_extent(ExtentDescriptor::Point)
            .with_reflection_send(false)
    });
    let (mut simulation, mut render) = build_multi_source_generation(
        &fixture_scene(),
        None,
        audio,
        fixture_simulation_config(),
        &descriptors,
        FIXTURE_GENERATION,
        QualityTier::Desktop,
    )
    .unwrap();

    let listener = ListenerState {
        pose: pose_at(LISTENER_POSITION_ENU_M),
        linear_velocity_mps: ApiEnuVector3::default(),
    };
    let mut motions = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    for source_index in 0..SOURCE_COUNT {
        motions[source_index] = SourceMotion {
            active: true,
            pose: pose_at(SOURCE_POSITIONS_ENU_M[source_index]),
            linear_velocity_mps: ApiEnuVector3::default(),
        };
    }
    simulation.update_inputs(&SimulationUpdate {
        listener,
        sources: motions,
    });
    simulation.run_direct().unwrap();
    render
        .take_stage_output_gain_writer()
        .unwrap()
        .publish(StageOutputGains {
            direct: 1.0,
            pathing: 0.0,
            reflections: 0.0,
        });

    let profiles = std::array::from_fn::<_, SOURCE_COUNT, _>(source_profile);
    let (mut safety_controller, safety_reader) =
        OutputSafetyPublication::new(OutputSafetyConfig::default()).unwrap();
    safety_controller
        .set_listener_position(LISTENER_POSITION_ENU_M)
        .unwrap();
    for (source_index, profile) in profiles.iter().enumerate() {
        safety_controller
            .set_source(source_index, profile, None)
            .unwrap();
    }

    let (mut propagation_writer, propagation_reader) =
        SnapshotPublication::new(PropagationSnapshot::default());
    propagation_writer.publish(PropagationSnapshot {
        sequence: 1,
        simulated_at_ns: 0,
        sources: std::array::from_fn(|source_index| SourcePropagation {
            active: source_index < SOURCE_COUNT,
            target_delay_samples: 0.0,
            left_gain: 1.0,
            right_gain: 1.0,
        }),
    });
    let mut graph = RuntimeGraph::new_with_backend_and_output_safety(
        EngineConfig {
            sample_rate_hz: SAMPLE_RATE_HZ,
            block_size_frames: BLOCK_FRAMES as u32,
            speed_of_sound_mps: 343.0,
            max_active_sources: SOURCE_COUNT as u8,
        },
        propagation_reader,
        safety_reader,
        Box::new(LegacyDirectBackend(render)),
    )
    .unwrap();
    graph.set_listener_state(listener);
    for (source_index, profile) in profiles.iter().enumerate() {
        let drive = graph
            .set_source(source_index, profile, SceneCalibration::default())
            .unwrap();
        assert!(drive.linear_gain() > 1.0);
    }

    let mut generator_states = std::array::from_fn::<_, SOURCE_COUNT, _>(|source_index| {
        INPUT_SEED_BASE.wrapping_add(INPUT_SEED_STRIDE.wrapping_mul(source_index as u64 + 1))
    });
    let mut inputs = std::array::from_fn::<_, SOURCE_COUNT, _>(|_| [0.0_f32; BLOCK_FRAMES]);
    let mut output_left = [0.0_f32; BLOCK_FRAMES];
    let mut output_right = [0.0_f32; BLOCK_FRAMES];
    let mut interleaved = Vec::with_capacity(CAPTURE_BLOCKS * BLOCK_FRAMES * 2);
    for block_index in 0..WARMUP_BLOCKS + CAPTURE_BLOCKS {
        for source_index in 0..SOURCE_COUNT {
            for sample in &mut inputs[source_index] {
                let generated = next_input_sample(&mut generator_states[source_index]);
                *sample = if active_program_mask & (1_u8 << source_index) != 0 {
                    generated
                } else {
                    0.0
                };
            }
        }
        let source_blocks = std::array::from_fn::<_, SOURCE_COUNT, _>(|source_index| SourceBlock {
            source_index,
            decoded_mono: &inputs[source_index],
        });
        graph
            .process_block(ProcessBlock {
                now_ns: 0,
                sources: &source_blocks,
                output_left: &mut output_left,
                output_right: &mut output_right,
            })
            .unwrap();
        if block_index >= WARMUP_BLOCKS {
            interleaved.extend(
                output_left
                    .iter()
                    .copied()
                    .zip(output_right.iter().copied())
                    .flat_map(|(left, right)| [left, right]),
            );
        }
    }

    FixtureCapture {
        interleaved,
        limiter_engagements: graph.safety_telemetry().limiter_engagements,
        proximity_ceiling_engagements: graph.safety_telemetry().proximity_ceiling_engagements,
        backend_render_errors: graph.fault_counters().backend_render_error,
    }
}

fn sample_bytes(samples: &[f32]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_bits().to_le_bytes())
        .collect()
}

fn energy(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| {
            let sample = f64::from(*sample);
            sample * sample
        })
        .sum()
}

fn assert_bit_identical(left: &[f32], right: &[f32]) {
    assert_eq!(left.len(), right.len());
    if let Some((index, (left, right))) = left
        .iter()
        .zip(right)
        .enumerate()
        .find(|(_, (left, right))| left.to_bits() != right.to_bits())
    {
        panic!(
            "fresh-session mismatch at interleaved sample {index}: {:08x} != {:08x}",
            left.to_bits(),
            right.to_bits()
        );
    }
}

fn assert_capture_health(capture: &FixtureCapture) {
    assert_eq!(capture.interleaved.len(), CAPTURE_BLOCKS * BLOCK_FRAMES * 2);
    assert!(capture.interleaved.iter().all(|sample| sample.is_finite()));
    assert!(energy(&capture.interleaved) > 1.0e-12);
    assert_eq!(capture.limiter_engagements, 0);
    assert_eq!(capture.proximity_ceiling_engagements, 0);
    assert_eq!(capture.backend_render_errors, 0);
}

#[test]
fn legacy_eight_point_source_final_mix_is_immutable() {
    let first = render_fixture(ALL_SOURCES_MASK);
    let second = render_fixture(ALL_SOURCES_MASK);
    assert_capture_health(&first);
    assert_capture_health(&second);
    assert_bit_identical(&first.interleaved, &second.interleaved);

    let hash = crate::sha256_hex(&sample_bytes(&first.interleaved));
    eprintln!(
        "legacy_eight_source_final_mix_sha256={hash} energy={:.12e} samples={}",
        energy(&first.interleaved),
        first.interleaved.len()
    );
    assert_eq!(hash, LEGACY_EIGHT_SOURCE_FINAL_MIX_SHA256);
}

#[test]
fn legacy_eight_point_source_fixture_isolates_every_program() {
    let mut hashes = Vec::with_capacity(SOURCE_COUNT);
    for source_index in 0..SOURCE_COUNT {
        let capture = render_fixture(1_u8 << source_index);
        assert_capture_health(&capture);
        let hash = crate::sha256_hex(&sample_bytes(&capture.interleaved));
        assert!(
            hashes.iter().all(|prior| prior != &hash),
            "source {source_index} duplicated a preceding isolated capture"
        );
        hashes.push(hash);
    }
}
