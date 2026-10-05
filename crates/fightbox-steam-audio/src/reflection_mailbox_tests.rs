use super::*;
use fightbox_runtime::backend::SourceMotion;

struct MailboxSource(ffi::IPLSource);

impl Drop for MailboxSource {
    fn drop(&mut self) {
        ffi::source_release(&mut self.0);
    }
}

struct MailboxEffect(ffi::IPLReflectionEffect);

impl Drop for MailboxEffect {
    fn drop(&mut self) {
        ffi::reflection_effect_release(&mut self.0);
    }
}

struct MailboxMixer(ffi::IPLReflectionMixer);

impl Drop for MailboxMixer {
    fn drop(&mut self) {
        ffi::reflection_mixer_release(&mut self.0);
    }
}

struct MailboxPair {
    effect: MailboxEffect,
    mixer: MailboxMixer,
    input: OwnedAudioBuffer,
    output: OwnedAudioBuffer,
    pcm: Vec<f32>,
}

impl MailboxPair {
    fn new(
        world: &WorldGeneration,
        audio: AudioConfig,
        params: ffi::IPLReflectionEffectParams,
    ) -> Self {
        let mut audio_settings = raw_audio_settings(audio);
        let mut settings = ffi::IPLReflectionEffectSettings {
            type_: params.type_,
            irSize: params.irSize,
            numChannels: params.numChannels,
        };
        let mut effect = MailboxEffect(core::ptr::null_mut());
        assert_eq!(
            ffi::reflection_effect_create(
                world.context(),
                &mut audio_settings,
                &mut settings,
                &mut effect.0
            ),
            ffi::IPL_STATUS_SUCCESS
        );
        let mut mixer = MailboxMixer(core::ptr::null_mut());
        assert_eq!(
            ffi::reflection_mixer_create(
                world.context(),
                &mut audio_settings,
                &mut settings,
                &mut mixer.0
            ),
            ffi::IPL_STATUS_SUCCESS
        );
        Self {
            effect,
            mixer,
            input: OwnedAudioBuffer::allocate(world.context(), 1, audio.frame_size).unwrap(),
            output: OwnedAudioBuffer::allocate(
                world.context(),
                params.numChannels,
                audio.frame_size,
            )
            .unwrap(),
            pcm: vec![0.0; (params.numChannels * audio.frame_size) as usize],
        }
    }

    fn apply(&mut self, mut params: ffi::IPLReflectionEffectParams, samples: &mut [f32]) -> &[f32] {
        self.input.write_mono(samples);
        let mut input = self.input.raw();
        let mut output = self.output.raw();
        ffi::reflection_effect_apply_to_mixer(
            self.effect.0,
            &mut params,
            &mut input,
            &mut output,
            self.mixer.0,
        );
        ffi::reflection_mixer_apply(self.mixer.0, &mut params, &mut output);
        self.output.read_interleaved(&mut self.pcm);
        &self.pcm
    }
}

#[test]
fn linked_inert_reflection_mailbox_preserves_input_history_and_real_ir_adoption() {
    let audio = AudioConfig {
        sample_rate_hz: 48_000,
        frame_size: 128,
    };
    let config = S3SimulationConfig {
        reflection_rays: 2_048,
        diffuse_samples: 8,
        reflection_bounces: 2,
        reflection_duration_s: 0.15,
        reflection_order: 1,
        simulation_threads: 1,
        ..S3SimulationConfig::default()
    };
    let mesh = SceneMesh {
        vertices_enu_m: vec![
            EnuVector3::new(-80.0, -80.0, 0.0),
            EnuVector3::new(80.0, -80.0, 0.0),
            EnuVector3::new(80.0, 80.0, 0.0),
            EnuVector3::new(-80.0, 80.0, 0.0),
        ],
        triangles: vec![[0, 1, 2], [0, 2, 3], [2, 1, 0], [3, 2, 0]],
        material_indices: vec![0; 4],
        materials: vec![crate::AcousticMaterial::MASONRY],
    };
    let descriptors = [MultiSourceDescriptor::at(ApiEnuVector3::new(2.0, 0.0, 2.0)); 2];
    let (mut simulation, _, _) = build_simulation_generation(
        &mesh,
        None,
        audio,
        config,
        &descriptors,
        1,
        QualityTier::Desktop,
        None,
    )
    .unwrap();
    let world = Arc::clone(&simulation.world);
    let mut inert = MailboxSource(core::ptr::null_mut());
    let mut inert_settings = ffi::IPLSourceSettings {
        flags: ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
    };
    assert_eq!(
        ffi::source_create(world.simulator(), &mut inert_settings, &mut inert.0),
        ffi::IPL_STATUS_SUCCESS
    );
    // This source is never added, so simulator passes cannot publish its IR.
    let mut inert_outputs = ffi::IPLSimulationOutputs::zeroed();
    ffi::source_get_outputs(
        inert.0,
        ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
        &mut inert_outputs,
    );
    assert!(!inert_outputs.reflections.ir.is_null());

    let run_reflections = |simulation: &mut MultiSourceSimulation, source_position| {
        let mut sources = [SourceMotion::default(); MAX_ACTIVE_SOURCES];
        for source in &mut sources[..2] {
            source.active = true;
            source.pose = default_api_pose(source_position);
        }
        simulation.update_inputs(&SimulationUpdate {
            listener: fightbox_api::ListenerState {
                pose: default_api_pose(ApiEnuVector3::new(0.0, 0.0, 2.0)),
                linear_velocity_mps: ApiEnuVector3::default(),
            },
            sources,
        });
        let quality = simulation.governor.render_quality();
        let mut shared = shared_inputs(simulation.frame.listener, quality).unwrap();
        shared.numRays = config.reflection_rays;
        shared.numBounces = config.reflection_bounces;
        shared.duration = config.reflection_duration_s;
        shared.order = config.reflection_order;
        ffi::simulator_set_shared_inputs(
            world.simulator(),
            ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            &mut shared,
        );
        for index in 0..2 {
            let mut inputs = source_inputs(
                simulation.frame.sources[index],
                Directivity::default(),
                DirectOcclusionMode::Raycast,
                world.probe_batch(),
                config,
                quality,
                ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
            )
            .unwrap();
            ffi::source_set_inputs(
                world.source(index),
                ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
                &mut inputs,
            );
        }
        ffi::simulator_run_reflections(world.simulator());
        std::array::from_fn::<_, 2, _>(|index| {
            let mut outputs = ffi::IPLSimulationOutputs::zeroed();
            ffi::source_get_outputs(
                world.source(index),
                ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
                &mut outputs,
            );
            outputs.reflections
        })
    };
    let initial = run_reflections(&mut simulation, ApiEnuVector3::new(2.0, 0.0, 2.0));
    assert_ne!(initial[0].ir, initial[1].ir);
    assert_ne!(initial[1].ir, inert_outputs.reflections.ir);
    assert!(
        initial
            .iter()
            .all(|params| params.numChannels == 4 && params.irSize > 128)
    );
    let mut reference = MailboxPair::new(&world, audio, initial[0]);
    let mut deferred = MailboxPair::new(&world, audio, initial[1]);
    let mut samples = [0.0; 128];
    reference.apply(initial[0], &mut samples);
    deferred.apply(initial[1], &mut samples);

    let history_blocks = (initial[0].irSize as usize).div_ceil(128);
    let hold_blocks = history_blocks * 2 + 17;
    let mut heard_reflections = false;
    let mut heard_tail = false;
    for phase in 0..2 {
        let params = if phase == 0 {
            initial
        } else {
            let updated = run_reflections(&mut simulation, ApiEnuVector3::new(9.0, 0.0, 2.0));
            assert_eq!(
                updated[0].ir, initial[0].ir,
                "real IR handle is a stable mailbox"
            );
            assert_eq!(updated[1].ir, initial[1].ir);
            updated
        };
        for block in 0..hold_blocks {
            let silent = block % 13 >= 10;
            for (frame, sample) in samples.iter_mut().enumerate() {
                *sample = if silent {
                    0.0
                } else {
                    (((block * 128 + frame) % 97) as f32 - 48.0) * 0.002
                };
            }
            let mut deferred_params = params[1];
            if phase == 0 || block > 0 {
                deferred_params.ir = inert_outputs.reflections.ir;
            }
            let expected = reference.apply(params[0], &mut samples);
            let actual = deferred.apply(deferred_params, &mut samples);
            for (frame, (a, b)) in expected.iter().zip(actual).enumerate() {
                assert!(a.is_finite() && b.is_finite());
                assert_eq!(
                    a.to_bits(),
                    b.to_bits(),
                    "phase {phase}, block {block}, sample {frame}"
                );
            }
            let audible = actual.iter().any(|sample| *sample != 0.0);
            heard_reflections |= audible;
            heard_tail |= silent && audible;
        }
    }
    assert!(
        heard_reflections && heard_tail,
        "comparison must exercise an audible IR and its tail"
    );
    let mut final_inert = ffi::IPLSimulationOutputs::zeroed();
    ffi::source_get_outputs(
        inert.0,
        ffi::IPL_SIMULATIONFLAGS_REFLECTIONS,
        &mut final_inert,
    );
    assert_eq!(final_inert.reflections.ir, inert_outputs.reflections.ir);
    eprintln!(
        "reflection mailbox: {} compared blocks, {}-block FFT history, exact PCM including real IR adoption",
        hold_blocks * 2,
        history_blocks
    );
}
