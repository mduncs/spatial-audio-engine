use super::*;
use crate::{
    AcousticMaterial, BakedProbeBatch, PROBE_BATCH_METADATA_SCHEMA, ProbeBatchMetadata,
    STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION,
};
use fightbox_runtime::backend::{BackendSourceBlock, SourceMotion};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

const SAMPLE_RATE: i32 = 48_000;
const BLOCK_FRAMES: i32 = 128;
const BELLS: ApiEnuVector3 = ApiEnuVector3::new(482.5, 292.5, 1.5);
const OCCLUDED_CORNER: ApiEnuVector3 = ApiEnuVector3::new(292.5, 387.5, 1.5);
const VISIBLE_STREET: ApiEnuVector3 = ApiEnuVector3::new(292.5, 292.5, 1.5);

#[derive(Clone, Copy, Debug)]
struct Observation {
    direct: SteamDirectParams,
    path_eq: [f32; 3],
    path_sh_energy: f32,
    source_has_probe: bool,
    listener_has_probe: bool,
    unoccluded_level_db_spl: f32,
    smoothed_direct_gain: f32,
    smoothed_path_sh_energy: f32,
    smoothed_path_send_energy: f32,
}

#[derive(Clone, Copy, Debug, Default)]
struct ReflectionStageStats {
    mixer_energy: f64,
    decoded_energy: f64,
    stem_energy: f64,
    stem_peak: f32,
    first_audible_sample: Option<usize>,
}

#[test]
fn retained_reflections_respond_to_nearby_masonry_wall() {
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = S3SimulationConfig {
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 2,
        reflection_duration_s: 0.15,
        reflection_order: 1,
        ..S3SimulationConfig::default()
    };
    let source = ApiEnuVector3::new(0.0, 0.0, 1.5);
    let descriptors = [crate::MultiSourceDescriptor::at(source)];
    let (mut simulation, mut render) = build_multi_source_generation(
        &reflection_wall_mesh(),
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
    )
    .expect("build retained wall reflection session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take retained wall stage control");
    stage_gains.publish(StageOutputGains {
        direct: 0.0,
        pathing: 0.0,
        reflections: 1.0,
    });
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
    }
    let mut energies = Vec::new();
    for (label, listener) in [
        ("near-wall", ApiEnuVector3::new(0.0, 1.0, 1.5)),
        ("open-side", ApiEnuVector3::new(0.0, -12.0, 1.5)),
    ] {
        simulation.update_inputs(&one_source_update(source, listener));
        simulation
            .run_reflections()
            .expect("wall reflection simulation");
        let zeros = vec![0.0; BLOCK_FRAMES as usize];
        for _ in 0..smoothing_settle_blocks(audio) {
            render_source_block(&mut render, &zeros);
        }
        let retained_stats = capture_retained_reflection(&mut render, audio, 1.0);
        simulation
            .run_reflections()
            .expect("refresh wall reflection IR for standalone effect");
        let reflection = simulation.snapshot.sources[0].reflections;
        let stats = measure_reflection_ir_through_effect(
            simulation.world.context(),
            audio,
            config,
            reflection,
        );
        println!(
            "WALL state={label} listener={listener:?} reflection_ir_effect_energy={:.9e} \
             reflection_ir_effect_peak={:.9e} mixer_energy={:.9e} decoded_energy={:.9e} \
             retained_reflection_energy={:.9e} retained_reflection_peak={:.9e} \
             retained_first_audible_sample={:?} stage_gain=1.000000000e0 \
             source_send_gain={:.9e} governor_output_gain={:.9e}",
            stats.0,
            stats.1,
            retained_stats.mixer_energy,
            retained_stats.decoded_energy,
            retained_stats.stem_energy,
            retained_stats.stem_peak,
            retained_stats.first_audible_sample,
            render.sources[0].quality_gains[2],
            render.applied_governor_quality.reflection_output_gain,
        );
        energies.push((stats.0, retained_stats.stem_energy));
    }
    let [(near_ir, near_stem), (open_ir, open_stem)] = energies.as_slice() else {
        unreachable!("two wall observations")
    };
    assert!(
        *near_ir > 0.1 && *near_stem > 0.1 && *open_ir > 0.005 && *open_stem > 0.005,
        "masonry reflections fell below the physically motivated unit-impulse floors: {energies:?}"
    );
    assert!(
        *near_stem / *open_stem > 5.0,
        "reflection energy did not respond materially to wall distance: {energies:?}"
    );
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock bake"]
fn megablock_reflection_to_direct_scale_diagnostics() {
    const DINER: ApiEnuVector3 = ApiEnuVector3::new(292.5, 292.5, 1.5);
    let package = env_path(
        "FIGHTBOX_DIAG_PACKAGE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox",
    );
    let bake = env_path(
        "FIGHTBOX_DIAG_BAKE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.baked",
    );
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let baked = load_baked(&bake);
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = S3SimulationConfig {
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 3,
        reflection_duration_s: 1.5,
        reflection_order: 1,
        pathing_order: 2,
        ..S3SimulationConfig::default()
    };
    let descriptors = [crate::MultiSourceDescriptor::at(DINER)
        .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl: 105.0 })];
    let (mut simulation, mut render) =
        build_multi_source_session(&mesh, &baked, audio, config, &descriptors)
            .expect("build retained megablock scale session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take retained megablock stage control");
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
    }

    for (label, listener) in [
        ("near-wall", ApiEnuVector3::new(285.0, 285.0, 1.5)),
        ("open-intersection", ApiEnuVector3::new(197.5, 292.5, 1.5)),
    ] {
        simulation.update_inputs(&one_source_update(DINER, listener));
        simulation.run_direct().expect("scale direct simulation");
        simulation
            .run_reflections()
            .expect("scale reflection simulation");
        let zeros = vec![0.0; BLOCK_FRAMES as usize];

        stage_gains.publish(StageOutputGains {
            direct: 1.0,
            pathing: 0.0,
            reflections: 0.0,
        });
        for _ in 0..smoothing_settle_blocks(audio) {
            render_source_block(&mut render, &zeros);
        }
        let direct = capture_retained_reflection(&mut render, audio, 4.0);

        simulation
            .run_reflections()
            .expect("refresh scale reflection IR");
        stage_gains.publish(StageOutputGains {
            direct: 0.0,
            pathing: 0.0,
            reflections: 1.0,
        });
        for _ in 0..smoothing_settle_blocks(audio) {
            render_source_block(&mut render, &zeros);
        }
        let reflections = capture_retained_reflection(&mut render, audio, 4.0);
        let ratio = reflections.stem_energy / direct.stem_energy;
        let ratio_db = 10.0 * ratio.log10();
        println!(
            "MEGABLOCK_SCALE state={label} listener={listener:?} \
             direct_unit_impulse_energy={:.9e} reflection_unit_impulse_energy={:.9e} \
             reflection_to_direct_energy_ratio={ratio:.9e} \
             reflection_to_direct_db={ratio_db:.3}",
            direct.stem_energy, reflections.stem_energy,
        );
        assert!(direct.stem_energy > 0.0 && reflections.stem_energy > 0.0 && ratio.is_finite());
    }
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock bake"]
fn megablock_source_teleport_pathing_is_order_independent() {
    let package = env_path(
        "FIGHTBOX_DIAG_PACKAGE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox",
    );
    let bake = env_path(
        "FIGHTBOX_DIAG_BAKE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.baked",
    );
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let baked = load_baked(&bake);
    let tallest_roof_m = mesh
        .vertices_enu_m
        .iter()
        .map(|vertex| vertex.z)
        .reduce(f32::max)
        .expect("megablock has vertices");
    let heights = [1.5, tallest_roof_m * 0.5, tallest_roof_m + 3.0];
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = S3SimulationConfig {
        reflection_rays: 64,
        diffuse_samples: 8,
        reflection_bounces: 1,
        reflection_duration_s: 0.05,
        reflection_order: 1,
        pathing_order: 2,
        ..S3SimulationConfig::default()
    };
    let descriptor = [crate::MultiSourceDescriptor::at(BELLS)
        .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl: 115.0 })];
    let (mut simulation, mut render) =
        build_multi_source_session(&mesh, &baked, audio, config, &descriptor)
            .expect("build retained megablock session");

    println!(
        "TELEPORT setup tallest_roof_m={tallest_roof_m:.3} heights_m={heights:?} \
         listener_occluded={OCCLUDED_CORNER:?} listener_visible={VISIBLE_STREET:?}"
    );

    for (label, height) in ["cold-street", "cold-medium", "cold-above"]
        .into_iter()
        .zip(heights)
    {
        let observation = drive_state(
            &mut simulation,
            &mut render,
            with_height(BELLS, height),
            OCCLUDED_CORNER,
            32,
        );
        print_observation(label, height, OCCLUDED_CORNER, observation);
    }

    walk_listener(
        &mut simulation,
        &mut render,
        with_height(BELLS, heights[2]),
        OCCLUDED_CORNER,
        VISIBLE_STREET,
    );
    for (label, height) in ["visible-street", "visible-medium", "visible-above"]
        .into_iter()
        .zip(heights)
    {
        let observation = drive_state(
            &mut simulation,
            &mut render,
            with_height(BELLS, height),
            VISIBLE_STREET,
            32,
        );
        print_observation(label, height, VISIBLE_STREET, observation);
    }

    walk_listener(
        &mut simulation,
        &mut render,
        with_height(BELLS, heights[2]),
        VISIBLE_STREET,
        OCCLUDED_CORNER,
    );
    let warm_final = drive_state(
        &mut simulation,
        &mut render,
        with_height(BELLS, heights[2]),
        OCCLUDED_CORNER,
        smoothing_settle_blocks(audio),
    );
    print_observation("warm-final-above", heights[2], OCCLUDED_CORNER, warm_final);

    let reset_street = drive_state(
        &mut simulation,
        &mut render,
        with_height(BELLS, heights[0]),
        OCCLUDED_CORNER,
        32,
    );
    print_observation("reset-street", heights[0], OCCLUDED_CORNER, reset_street);
    let reset_final = drive_state(
        &mut simulation,
        &mut render,
        with_height(BELLS, heights[2]),
        OCCLUDED_CORNER,
        smoothing_settle_blocks(audio),
    );
    print_observation(
        "reset-final-above",
        heights[2],
        OCCLUDED_CORNER,
        reset_final,
    );

    let raw_difference = (warm_final.path_sh_energy - reset_final.path_sh_energy).abs();
    let smoothed_difference =
        (warm_final.smoothed_path_send_energy - reset_final.smoothed_path_send_energy).abs();
    println!(
        "TELEPORT comparison raw_path_sh_energy_delta={raw_difference:.9e} \
         smoothed_path_send_energy_delta={smoothed_difference:.9e}"
    );
    assert_eq!(
        warm_final.direct, reset_final.direct,
        "same final geometry produced different direct targets"
    );
    assert_eq!(
        warm_final.path_eq, reset_final.path_eq,
        "same final geometry produced different path EQ targets"
    );
    assert!(
        raw_difference <= 1.0e-10 && smoothed_difference <= 1.0e-10,
        "same final listener/source geometry retained history: warm={warm_final:?} reset={reset_final:?}"
    );
    assert!(
        warm_final.smoothed_direct_gain <= 1.0e-6 && reset_final.smoothed_direct_gain <= 1.0e-6,
        "direct gain did not converge to its silent target within eight smoothing time constants"
    );
}

#[test]
#[ignore = "requires the locally acquired Steam Audio 4.8.1 SDK and megablock bake"]
fn megablock_artillery_direct_and_reflection_diagnostics() {
    const ARTILLERY: ApiEnuVector3 = ApiEnuVector3::new(7.5, 7.5, 1.5);
    let package = env_path(
        "FIGHTBOX_DIAG_PACKAGE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.fightbox",
    );
    let bake = env_path(
        "FIGHTBOX_DIAG_BAKE",
        "/Users/md/fightbox-runs/megablock-seed1/megablock.baked",
    );
    let mesh = load_megablock_mesh(&package.join("mesh.bin"));
    let baked = load_baked(&bake);
    let (mesh_min, mesh_max) = mesh_bounds(&mesh);
    println!(
        "ARTILLERY mesh_min={mesh_min:?} mesh_max={mesh_max:?} source={ARTILLERY:?} \
         source_inside_horizontal_mesh_bounds={}",
        ARTILLERY.east_m >= mesh_min.x
            && ARTILLERY.east_m <= mesh_max.x
            && ARTILLERY.north_m >= mesh_min.y
            && ARTILLERY.north_m <= mesh_max.y,
    );
    let audio = AudioConfig {
        sample_rate_hz: SAMPLE_RATE,
        frame_size: BLOCK_FRAMES,
    };
    let config = S3SimulationConfig {
        reflection_rays: 4_096,
        diffuse_samples: 32,
        reflection_bounces: 3,
        reflection_duration_s: 1.50,
        reflection_order: 1,
        pathing_order: 2,
        ..S3SimulationConfig::default()
    };
    let descriptor = [crate::MultiSourceDescriptor::at(ARTILLERY)
        .with_reference_level(fightbox_api::ReferenceLevel::SplAtOneMeter { db_spl: 155.0 })];
    let (mut simulation, mut render) =
        build_multi_source_session(&mesh, &baked, audio, config, &descriptor)
            .expect("build retained megablock artillery session");
    let mut stage_gains = render
        .take_stage_output_gain_writer()
        .expect("take retained stage control");
    stage_gains.publish(StageOutputGains {
        direct: 0.0,
        pathing: 0.0,
        reflections: 1.0,
    });
    let initial_quality = simulation.quality_governor_telemetry();
    println!(
        "ARTILLERY initial_source_quality={:?} initial_reflection_level={:?} \
         initial_reverb={:?} initial_render_quality_gains={:?} \
         initial_reflection_send_energy=0.000000000e0",
        initial_quality.sources[0].quality,
        initial_quality.reflections.level,
        initial_quality.reverb,
        render.sources[0].quality_gains,
    );
    for _ in 0..20_000 {
        simulation.observe_render_timing(100_000);
        let quality = simulation.quality_governor_telemetry();
        if quality.sources[0].quality == SourceQualityLevel::Full
            && quality.reflections.level == crate::ReflectionQualityLevel::Full
            && quality.reverb == ReverbStrategy::SdkMixerConvolution
        {
            break;
        }
    }
    let recovered_quality = simulation.quality_governor_telemetry();
    assert_eq!(
        recovered_quality.sources[0].quality,
        SourceQualityLevel::Full
    );
    assert_eq!(
        recovered_quality.reflections.level,
        crate::ReflectionQualityLevel::Full
    );
    assert_eq!(
        recovered_quality.reverb,
        ReverbStrategy::SdkMixerConvolution
    );
    println!(
        "ARTILLERY recovered_source_quality={:?} recovered_reflection_level={:?} \
         recovered_reverb={:?}",
        recovered_quality.sources[0].quality,
        recovered_quality.reflections.level,
        recovered_quality.reverb,
    );

    let positions = [
        ("near-source", ARTILLERY, ApiEnuVector3::new(7.5, 15.0, 1.5)),
        (
            "diner-one-block",
            ApiEnuVector3::new(292.5, 292.5, 1.5),
            ApiEnuVector3::new(197.5, 292.5, 1.5),
        ),
        (
            "street-corridor-los",
            ARTILLERY,
            ApiEnuVector3::new(292.5, 7.5, 1.5),
        ),
        (
            "one-block-diagonal-blocked",
            ARTILLERY,
            ApiEnuVector3::new(102.5, 102.5, 1.5),
        ),
        (
            "map-center-blocked",
            ARTILLERY,
            ApiEnuVector3::new(292.5, 292.5, 1.5),
        ),
    ];
    for (label, source, listener) in positions {
        simulation.update_inputs(&one_source_update(source, listener));
        simulation
            .run_direct()
            .expect("artillery direct simulation");
        simulation
            .run_reflections()
            .expect("artillery reflection simulation");
        let reflection = simulation.snapshot.sources[0].reflections;
        let ir_effect_stats = measure_reflection_ir_through_effect(
            simulation.world.context(),
            audio,
            config,
            reflection,
        );
        // The SDK IR handle is a one-consumer partition payload. Re-run the
        // simulation after the standalone convolution probe so the retained
        // graph receives a fresh payload through its real mixer path.
        simulation
            .run_reflections()
            .expect("refresh artillery reflection IR for retained graph");
        let quality = simulation.quality_governor_telemetry().sources[0];
        let zeros = vec![0.0; BLOCK_FRAMES as usize];
        for _ in 0..smoothing_settle_blocks(audio) {
            render_source_block(&mut render, &zeros);
        }

        let mut reflection_energy = 0.0_f64;
        let mut reflection_peak = 0.0_f32;
        let mut mixer_energy = 0.0_f64;
        let mut decoded_energy = 0.0_f64;
        let mut mixer_samples = vec![0.0; (render.reflection_mix.channels * BLOCK_FRAMES) as usize];
        let mut decoded_samples = vec![0.0; (2 * BLOCK_FRAMES) as usize];
        let capture_blocks = (4.0 * SAMPLE_RATE as f32 / BLOCK_FRAMES as f32).ceil() as usize;
        for block in 0..capture_blocks {
            let mut input = vec![0.0; BLOCK_FRAMES as usize];
            if block == 0 {
                input[0] = 1.0;
            }
            let (left, right) = render_source_block(&mut render, &input);
            render.reflection_mix.read_interleaved(&mut mixer_samples);
            render
                .reflection_stereo
                .read_interleaved(&mut decoded_samples);
            mixer_energy += buffer_energy(&mixer_samples);
            decoded_energy += buffer_energy(&decoded_samples);
            for sample in left.into_iter().chain(right) {
                reflection_energy += f64::from(sample * sample);
                reflection_peak = reflection_peak.max(sample.abs());
            }
        }
        let direct = simulation.snapshot.sources[0].direct;
        let unoccluded_gain = direct.distance_attenuation
            * (direct.air_absorption.into_iter().sum::<f32>() / 3.0)
            * direct.directivity;
        let unoccluded_level_db_spl = 155.0 + 20.0 * unoccluded_gain.max(f32::MIN_POSITIVE).log10();
        println!(
            "ARTILLERY state={label} listener=[{:.3},{:.3},{:.3}] \
             direct_occlusion={:.9e} direct_distance={:.9e} direct_air={:?} \
             unoccluded_level_db_spl={unoccluded_level_db_spl:.3} \
             reflection_ir_nonnull={} reflection_ir_size={} reflection_channels={} \
             reflection_ir_effect_energy={:.9e} reflection_ir_effect_peak={:.9e} \
             reflection_reverb_times={:?} source_quality={:?} quality_mode=headroom-recovered \
             stage_gain=1.000000000e0 source_send_gain={:.9e} \
             governor_output_gain={:.9e} mixer_energy={mixer_energy:.9e} \
             decoded_energy={decoded_energy:.9e} \
             reflection_unit_impulse_energy={reflection_energy:.9e} \
             reflection_unit_impulse_peak={reflection_peak:.9e}",
            listener.east_m,
            listener.north_m,
            listener.up_m,
            direct.occlusion,
            direct.distance_attenuation,
            direct.air_absorption,
            reflection.ir != 0,
            reflection.ir_size,
            reflection.num_channels,
            ir_effect_stats.0,
            ir_effect_stats.1,
            reflection.reverb_times,
            quality.quality,
            render.sources[0].quality_gains[2],
            render.applied_governor_quality.reflection_output_gain,
        );
    }
}

fn measure_reflection_ir_through_effect(
    context: ffi::IPLContext,
    audio: AudioConfig,
    config: S3SimulationConfig,
    reflection: SteamReflectionParams,
) -> (f64, f32) {
    let mut audio_settings = raw_audio_settings(audio);
    let mut settings = ffi::IPLReflectionEffectSettings {
        type_: reflection_effect_ffi_type(config.reflection_effect.effect_type)
            .expect("validated reflection type"),
        irSize: reflection_ir_size(config.reflection_duration_s, audio.sample_rate_hz)
            .expect("validated reflection duration"),
        numChannels: ambisonics_channel_count(config.reflection_order)
            .expect("validated reflection order"),
    };
    let mut effect = core::ptr::null_mut();
    assert_eq!(
        ffi::reflection_effect_create(context, &mut audio_settings, &mut settings, &mut effect,),
        ffi::IPL_STATUS_SUCCESS
    );
    let mut input = OwnedAudioBuffer::allocate(context, 1, audio.frame_size)
        .expect("allocate diagnostic reflection input");
    let mut output = OwnedAudioBuffer::allocate(context, settings.numChannels, audio.frame_size)
        .expect("allocate diagnostic reflection output");
    let mut interleaved = vec![0.0; (settings.numChannels * audio.frame_size) as usize];
    let mut energy = 0.0_f64;
    let mut peak = 0.0_f32;
    let capture_blocks =
        (4.0 * audio.sample_rate_hz as f32 / audio.frame_size as f32).ceil() as usize;
    for block in 0..capture_blocks {
        let mut input_samples = vec![0.0; audio.frame_size as usize];
        if block == 0 {
            input_samples[0] = 1.0;
        }
        input.write_mono(&mut input_samples);
        let mut input_raw = input.raw();
        let mut output_raw = output.raw();
        let mut params = reflection_effect_params(reflection, config);
        ffi::reflection_effect_apply(effect, &mut params, &mut input_raw, &mut output_raw);
        output.read_interleaved(&mut interleaved);
        for sample in interleaved.iter().copied() {
            energy += f64::from(sample) * f64::from(sample);
            peak = peak.max(sample.abs());
        }
    }
    ffi::reflection_effect_release(&mut effect);
    (energy, peak)
}

fn capture_retained_reflection(
    render: &mut MultiSourceRenderGraph,
    audio: AudioConfig,
    seconds: f32,
) -> ReflectionStageStats {
    let mut stats = ReflectionStageStats::default();
    let mut mixer_samples = vec![0.0; (render.reflection_mix.channels * audio.frame_size) as usize];
    let mut decoded_samples = vec![0.0; (2 * audio.frame_size) as usize];
    let blocks = (seconds * audio.sample_rate_hz as f32 / audio.frame_size as f32).ceil() as usize;
    for block in 0..blocks {
        let mut input = vec![0.0; audio.frame_size as usize];
        if block == 0 {
            input[0] = 1.0;
        }
        let (left, right) = render_source_block(render, &input);
        render.reflection_mix.read_interleaved(&mut mixer_samples);
        render
            .reflection_stereo
            .read_interleaved(&mut decoded_samples);
        stats.mixer_energy += buffer_energy(&mixer_samples);
        stats.decoded_energy += buffer_energy(&decoded_samples);
        for (frame, (left, right)) in left.into_iter().zip(right).enumerate() {
            stats.stem_energy +=
                f64::from(left) * f64::from(left) + f64::from(right) * f64::from(right);
            stats.stem_peak = stats.stem_peak.max(left.abs()).max(right.abs());
            if stats.first_audible_sample.is_none() && left.abs().max(right.abs()) >= 1.0e-7 {
                stats.first_audible_sample = Some(block * audio.frame_size as usize + frame);
            }
        }
    }
    stats
}

fn buffer_energy(samples: &[f32]) -> f64 {
    samples
        .iter()
        .copied()
        .map(|sample| f64::from(sample) * f64::from(sample))
        .sum()
}

fn reflection_wall_mesh() -> SceneMesh {
    SceneMesh {
        vertices_enu_m: vec![
            EnuVector3::new(-20.0, 2.0, 0.0),
            EnuVector3::new(20.0, 2.0, 0.0),
            EnuVector3::new(20.0, 2.0, 10.0),
            EnuVector3::new(-20.0, 2.0, 10.0),
            EnuVector3::new(-20.0, -20.0, 0.0),
            EnuVector3::new(20.0, -20.0, 0.0),
            EnuVector3::new(20.0, 20.0, 0.0),
            EnuVector3::new(-20.0, 20.0, 0.0),
        ],
        triangles: vec![
            [0, 1, 2],
            [0, 2, 3],
            [2, 1, 0],
            [3, 2, 0],
            [4, 5, 6],
            [4, 6, 7],
            [6, 5, 4],
            [7, 6, 4],
        ],
        material_indices: vec![0; 8],
        materials: vec![AcousticMaterial::MASONRY],
    }
}

fn mesh_bounds(mesh: &SceneMesh) -> (EnuVector3, EnuVector3) {
    let mut minimum = EnuVector3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut maximum = EnuVector3::new(f32::NEG_INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for vertex in &mesh.vertices_enu_m {
        minimum.x = minimum.x.min(vertex.x);
        minimum.y = minimum.y.min(vertex.y);
        minimum.z = minimum.z.min(vertex.z);
        maximum.x = maximum.x.max(vertex.x);
        maximum.y = maximum.y.max(vertex.y);
        maximum.z = maximum.z.max(vertex.z);
    }
    (minimum, maximum)
}

fn walk_listener(
    simulation: &mut MultiSourceSimulation,
    render: &mut MultiSourceRenderGraph,
    source: ApiEnuVector3,
    from: ApiEnuVector3,
    to: ApiEnuVector3,
) {
    for step in 1..=16 {
        let amount = step as f32 / 16.0;
        let listener = ApiEnuVector3::new(
            from.east_m + (to.east_m - from.east_m) * amount,
            from.north_m + (to.north_m - from.north_m) * amount,
            from.up_m + (to.up_m - from.up_m) * amount,
        );
        drive_state(simulation, render, source, listener, 4);
    }
}

fn drive_state(
    simulation: &mut MultiSourceSimulation,
    render: &mut MultiSourceRenderGraph,
    source: ApiEnuVector3,
    listener: ApiEnuVector3,
    render_blocks: usize,
) -> Observation {
    simulation.update_inputs(&one_source_update(source, listener));
    simulation.run_direct().expect("direct simulation");
    simulation.run_pathing().expect("path simulation");
    let input = vec![0.0; BLOCK_FRAMES as usize];
    for _ in 0..render_blocks {
        render_one_source_block(render, &input);
    }
    observe(simulation, render)
}

fn observe(simulation: &MultiSourceSimulation, render: &MultiSourceRenderGraph) -> Observation {
    let raw = simulation.snapshot.sources[0];
    let smoothed = render.sources[0].propagation_smoother.applied();
    let smoothed_direct_gain = predicted_direct_gain(smoothed.direct);
    let smoothed_path_sh_energy = energy(smoothed.path_sh);
    let mean_eq_squared = smoothed
        .path_eq
        .into_iter()
        .map(|coefficient| coefficient * coefficient)
        .sum::<f32>()
        / 3.0;
    Observation {
        direct: raw.direct,
        path_eq: raw.path_eq,
        path_sh_energy: energy(raw.path_sh),
        source_has_probe: simulation
            .world
            .has_influencing_probe(simulation.frame.sources[0].position),
        listener_has_probe: simulation
            .world
            .has_influencing_probe(simulation.frame.listener.position),
        unoccluded_level_db_spl: {
            let air = raw.direct.air_absorption.into_iter().sum::<f32>() / 3.0;
            let gain = raw.direct.distance_attenuation * air * raw.direct.directivity;
            115.0 + 20.0 * gain.max(f32::MIN_POSITIVE).log10()
        },
        smoothed_direct_gain,
        smoothed_path_sh_energy,
        smoothed_path_send_energy: smoothed_path_sh_energy * mean_eq_squared,
    }
}

fn print_observation(
    label: &str,
    source_height_m: f32,
    listener: ApiEnuVector3,
    observation: Observation,
) {
    println!(
        "TELEPORT state={label} source_z_m={source_height_m:.3} \
         listener=[{:.3},{:.3},{:.3}] direct_occlusion={:.9e} \
         direct_distance={:.9e} path_sh_energy={:.9e} path_eq={:?} \
         source_has_probe={} listener_has_probe={} unoccluded_level_db_spl={:.3} \
         smoothed_direct_gain={:.9e} smoothed_path_sh_energy={:.9e} \
         smoothed_path_send_energy={:.9e}",
        listener.east_m,
        listener.north_m,
        listener.up_m,
        observation.direct.occlusion,
        observation.direct.distance_attenuation,
        observation.path_sh_energy,
        observation.path_eq,
        observation.source_has_probe,
        observation.listener_has_probe,
        observation.unoccluded_level_db_spl,
        observation.smoothed_direct_gain,
        observation.smoothed_path_sh_energy,
        observation.smoothed_path_send_energy,
    );
}

fn smoothing_settle_blocks(audio: AudioConfig) -> usize {
    let block_seconds = audio.frame_size as f32 / audio.sample_rate_hz as f32;
    (PROPAGATION_SLEW_TIME_SECONDS * 8.0 / block_seconds).ceil() as usize
}

fn energy<const N: usize>(values: [f32; N]) -> f32 {
    values
        .into_iter()
        .map(|coefficient| coefficient * coefficient)
        .sum()
}

fn with_height(position: ApiEnuVector3, height_m: f32) -> ApiEnuVector3 {
    ApiEnuVector3::new(position.east_m, position.north_m, height_m)
}

fn one_source_update(
    source_position: ApiEnuVector3,
    listener_position: ApiEnuVector3,
) -> SimulationUpdate {
    let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
    sources[0] = SourceMotion {
        active: true,
        pose: default_api_pose(source_position),
        linear_velocity_mps: ApiEnuVector3::default(),
    };
    SimulationUpdate {
        listener: fightbox_api::ListenerState {
            pose: default_api_pose(listener_position),
            linear_velocity_mps: ApiEnuVector3::default(),
        },
        sources,
    }
}

fn render_one_source_block(render: &mut MultiSourceRenderGraph, input: &[f32]) {
    let _ = render_source_block(render, input);
}

fn render_source_block(render: &mut MultiSourceRenderGraph, input: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let sources = [BackendSourceBlock {
        source_index: 0,
        input_mono: input,
    }];
    let mut left = vec![0.0; input.len()];
    let mut right = vec![0.0; input.len()];
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
        .expect("render retained block");
    (left, right)
}

fn env_path(variable: &str, default: &str) -> PathBuf {
    env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
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
    let digits = json_field_tail(json, field)
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    digits.parse().unwrap_or_else(|_| panic!("numeric {field}"))
}

fn json_string(json: &str, field: &str) -> String {
    json_field_tail(json, field)
        .trim_start()
        .strip_prefix('"')
        .and_then(|tail| tail.split('"').next())
        .unwrap_or_else(|| panic!("string {field}"))
        .to_owned()
}

fn json_field_tail<'a>(json: &'a str, field: &str) -> &'a str {
    let needle = format!("\"{field}\":");
    json.split_once(&needle)
        .unwrap_or_else(|| panic!("metadata field {field}"))
        .1
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(read_u32(bytes, offset))
}
