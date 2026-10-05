//! Optional pre-HRTF taps on the retained full-effect renderer.

use super::*;

struct ExportPath {
    effect: NeutralPathEffect,
    output: OwnedAudioBuffer,
}

pub(super) struct FullSpatialExportTap {
    presentation: Vec<f32>,
    environment: Vec<f32>,
    interleaved: Vec<f32>,
    paths: Vec<Option<ExportPath>>,
    metadata: fightbox_runtime::backend::SpatialOutputMetadata,
}

impl FullSpatialExportTap {
    fn new(graph: &MultiSourceRenderGraph) -> Result<Self, BackendError> {
        if !(0..=2).contains(&graph.config.pathing_order)
            || !(0..=2).contains(&graph.config.reflection_order)
        {
            return Err(BackendError::InvalidInput(
                "AmbiX export supports the full field only through order two",
            ));
        }
        let frames = graph.audio.frame_size as usize;
        let context = graph.world.context();
        let mut audio_settings = raw_audio_settings(graph.audio);
        let mut paths = Vec::with_capacity(graph.sources.len());
        for source in &graph.sources {
            let path = if source.pathing_send_enabled && graph.world.has_baked_pathing {
                let mut settings = ffi::IPLPathEffectSettings {
                    maxOrder: graph.config.pathing_order,
                    spatialize: ffi::IPL_FALSE,
                    speakerLayout: ffi::IPLSpeakerLayout {
                        type_: ffi::IPL_SPEAKERLAYOUTTYPE_STEREO,
                        numSpeakers: 0,
                        speakers: core::ptr::null_mut(),
                    },
                    hrtf: core::ptr::null_mut(),
                };
                let mut effect = core::ptr::null_mut();
                sdk_status(
                    "iplPathEffectCreate(AmbiX tap)",
                    ffi::path_effect_create(
                        context,
                        &mut audio_settings,
                        &mut settings,
                        &mut effect,
                    ),
                )?;
                let effect = NeutralPathEffect(effect as usize);
                Some(ExportPath {
                    effect,
                    output: OwnedAudioBuffer::allocate(
                        context,
                        ambisonics_channel_count(graph.config.pathing_order)
                            .expect("validated export path order"),
                        graph.audio.frame_size,
                    )?,
                })
            } else {
                None
            };
            paths.push(path);
        }
        Ok(Self {
            presentation: vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * frames],
            environment: vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * frames],
            interleaved: vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * frames],
            paths,
            metadata: fightbox_runtime::backend::SpatialOutputMetadata::default(),
        })
    }

    pub(super) fn reset_source(&mut self, source_index: usize) {
        if let Some(path) = &self.paths[source_index] {
            ffi::path_effect_reset(handle(path.effect.0));
        }
    }

    pub(super) fn reset(&mut self) {
        for index in 0..self.paths.len() {
            self.reset_source(index);
        }
    }

    pub(super) fn feed(
        &mut self,
        source_index: usize,
        component: SpatialPresentationComponent,
        position: SteamVector3,
        listener_position: SteamVector3,
        propagation: SteamSourcePropagation,
        samples: &[f32],
        gain: f32,
        ramp: GainRamp,
        latency_frames: u32,
    ) {
        let plane = NeutralMultiSourceRenderGraph::mark_feed(
            &mut self.metadata,
            source_index,
            component,
            position,
            listener_position,
            ApiEnuVector3::default(),
            false,
            propagation,
            latency_frames,
        )
        .expect("fixed export presentation component");
        for (frame, (output, sample)) in
            spatial_plane_mut(&mut self.presentation, plane, samples.len())
                .iter_mut()
                .zip(samples)
                .enumerate()
        {
            *output = *sample * gain * ramp.at(frame);
        }
    }

    pub(super) fn direct_buffer(
        &mut self,
        source_index: usize,
        position: SteamVector3,
        listener_position: SteamVector3,
        propagation: SteamSourcePropagation,
        buffer: &mut OwnedAudioBuffer,
        gain: f32,
        ramp: GainRamp,
    ) {
        let frames = buffer.samples as usize;
        buffer.read_interleaved(&mut self.interleaved[..frames]);
        let plane = NeutralMultiSourceRenderGraph::mark_feed(
            &mut self.metadata,
            source_index,
            SpatialPresentationComponent::DirectCenter,
            position,
            listener_position,
            ApiEnuVector3::default(),
            false,
            propagation,
            0,
        )
        .expect("fixed export point component");
        for (frame, (output, sample)) in spatial_plane_mut(&mut self.presentation, plane, frames)
            .iter_mut()
            .zip(&self.interleaved[..frames])
            .enumerate()
        {
            *output = *sample * gain * ramp.at(frame);
        }
    }

    pub(super) fn path(
        &mut self,
        source_index: usize,
        params: &mut ffi::IPLPathEffectParams,
        input: &mut ffi::IPLAudioBuffer,
        gain: f32,
        ramp: GainRamp,
    ) {
        let Some(path) = &mut self.paths[source_index] else {
            return;
        };
        let mut output = path.output.raw();
        let mut neutral_params = *params;
        neutral_params.binaural = ffi::IPL_FALSE;
        neutral_params.hrtf = core::ptr::null_mut();
        ffi::path_effect_apply(
            handle(path.effect.0),
            &mut neutral_params,
            input,
            &mut output,
        );
        let channels = path.output.channels as usize;
        let frames = path.output.samples as usize;
        path.output
            .read_interleaved(&mut self.interleaved[..frames * channels]);
        add_environment(
            &mut self.environment,
            &self.interleaved,
            channels,
            frames,
            gain,
            ramp,
        );
    }

    pub(super) fn reflection(
        &mut self,
        buffer: &mut OwnedAudioBuffer,
        order: i32,
        gain: f32,
        ramp: GainRamp,
    ) {
        let channels = buffer.channels as usize;
        let frames = buffer.samples as usize;
        buffer.read_interleaved(&mut self.interleaved[..frames * channels]);
        let active_channels =
            active_channel_count(order).expect("validated export reflection order");
        for channel in 0..active_channels.min(channels) {
            for (frame, output) in spatial_plane_mut(&mut self.environment, channel, frames)
                .iter_mut()
                .enumerate()
            {
                *output += self.interleaved[frame * channels + channel] * gain * ramp.at(frame);
            }
        }
    }

    pub(super) fn echo(
        &mut self,
        buffer: &mut OwnedAudioBuffer,
        position: SteamVector3,
        listener_position: SteamVector3,
        gain: f32,
    ) {
        let frames = buffer.samples as usize;
        buffer.read_interleaved(&mut self.interleaved[..frames]);
        let direction = normalized_api(steam_vector_to_api(SteamVector3::new(
            position.x - listener_position.x,
            position.y - listener_position.y,
            position.z - listener_position.z,
        )))
        .unwrap_or(ApiEnuVector3::new(0.0, 1.0, 0.0));
        let sh = native_steam_sh(direction);
        for (channel, coefficient) in sh.into_iter().enumerate() {
            for (output, sample) in spatial_plane_mut(&mut self.environment, channel, frames)
                .iter_mut()
                .zip(&self.interleaved[..frames])
            {
                *output += *sample * gain * coefficient;
            }
        }
    }
}

fn add_environment(
    bank: &mut [f32],
    samples: &[f32],
    channels: usize,
    frames: usize,
    gain: f32,
    ramp: GainRamp,
) {
    for channel in 0..channels {
        for (frame, output) in spatial_plane_mut(bank, channel, frames)
            .iter_mut()
            .enumerate()
        {
            *output += samples[frame * channels + channel] * gain * ramp.at(frame);
        }
    }
}

// Steam's Google SH basis includes Condon–Shortley phase for odd |m|.
// Its coordinate conversion is front=-z, left=-x, up=y.
fn native_steam_sh(direction_enu: ApiEnuVector3) -> [f32; 9] {
    let x = direction_enu.north_m;
    let y = -direction_enu.east_m;
    let z = direction_enu.up_m;
    let w = 1.0 / (4.0 * std::f32::consts::PI).sqrt();
    [
        w,
        -w * 3.0_f32.sqrt() * y,
        w * 3.0_f32.sqrt() * z,
        -w * 3.0_f32.sqrt() * x,
        w * 15.0_f32.sqrt() * x * y,
        -w * 15.0_f32.sqrt() * y * z,
        w * 5.0_f32.sqrt() * 0.5 * (3.0 * z * z - 1.0),
        -w * 15.0_f32.sqrt() * x * z,
        w * 15.0_f32.sqrt() * 0.5 * (x * x - y * y),
    ]
}

pub(crate) struct FullSpatialExportGraph {
    graph: MultiSourceRenderGraph,
    left: Vec<f32>,
    right: Vec<f32>,
}

impl FullSpatialExportGraph {
    pub(crate) fn new(mut graph: MultiSourceRenderGraph) -> Result<Self, BackendError> {
        graph.spatial_export = Some(FullSpatialExportTap::new(&graph)?);
        let frames = graph.audio.frame_size as usize;
        Ok(Self {
            graph,
            left: vec![0.0; frames],
            right: vec![0.0; frames],
        })
    }
}

impl SpatialBackendRenderGraph for FullSpatialExportGraph {
    fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
        let frames = self.graph.audio.frame_size as usize;
        let zeros = vec![0.0; frames];
        let snapshot = self.graph.publication.read();
        let quality = self.graph.governor_quality.read();
        let gains = self.graph.stage_output_gains.read();
        let echo_gain = self.graph.echo_output_gain.read();
        let listener = listener_pose(ListenerOrientation {
            forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
            up: ApiEnuVector3::new(0.0, 0.0, 1.0),
        })
        .expect("fixed preparation orientation");
        self.graph.applied_governor_quality = quality;
        // Seed every configured effect on the control thread, including gated
        // sources and both mono and stereo presentations. The history flag
        // prevents the distant pristine-silence shortcut from skipping work.
        for state in &mut self.graph.sources {
            state.program_history = true;
        }
        for block in 0..16 {
            self.left.fill(0.0);
            self.right.fill(0.0);
            self.graph.reflection_block_order = quality.ambisonic_order;
            for index in 0..self.graph.sources.len() {
                let stereo = block >= 8 && self.graph.sources[index].stereo_image.is_some();
                self.graph.render_source(
                    &SpatialBackendSourceBlock {
                        source_index: index,
                        program_plane_count: if stereo { 2 } else { 1 },
                        program_planes: [&zeros, if stereo { &zeros } else { &[] }],
                    },
                    snapshot.sources[index],
                    listener,
                    snapshot.listener_position,
                    snapshot.listener_linear_velocity_mps,
                    snapshot.direct_sequence,
                    &mut self.left,
                    &mut self.right,
                    gains,
                    quality,
                    0,
                    echo_gain,
                );
            }
            self.graph.render_reflection_mix(
                listener,
                &mut self.left,
                &mut self.right,
                gains.reflections,
                quality,
                &mut StageEnergyAccumulator::default(),
            );
        }
        // Echo plans may not exist until Play. Initialize those fixed effects
        // without admitting a tap or changing the source's trigger clock.
        for state in &mut self.graph.sources {
            let Some(echo) = &mut state.echo else {
                continue;
            };
            echo.input.write_mono(&mut self.graph.mono_work);
            let mut input = echo.input.raw();
            for index in 0..MAX_ECHO_TAPS_PER_SOURCE {
                let mut direct = ffi::IPLDirectEffectParams {
                    flags: ffi::IPL_DIRECTEFFECTFLAGS_APPLYDISTANCEATTENUATION
                        | ffi::IPL_DIRECTEFFECTFLAGS_APPLYAIRABSORPTION,
                    transmissionType: ffi::IPL_TRANSMISSIONTYPE_FREQDEPENDENT,
                    distanceAttenuation: 1.0,
                    airAbsorption: [1.0; 3],
                    directivity: 1.0,
                    occlusion: 1.0,
                    transmission: [1.0; 3],
                };
                let mut binaural = ffi::IPLBinauralEffectParams {
                    direction: ffi::IPLVector3 {
                        x: 0.0,
                        y: 0.0,
                        z: -1.0,
                    },
                    interpolation: ffi::IPL_HRTFINTERPOLATION_BILINEAR,
                    spatialBlend: 1.0,
                    hrtf: handle(self.graph.hrtf),
                    peakDelays: core::ptr::null_mut(),
                };
                echo.tap_silent_pairs[index].render(
                    false,
                    &zeros,
                    echo.tap_direct_effects[index],
                    echo.tap_binaural_effects[index],
                    &mut direct,
                    &mut binaural,
                    &mut input,
                    &mut echo.filtered,
                    &mut echo.stereo,
                    &mut self.graph.stereo_work,
                );
            }
        }
        self.graph.reset_scene_history();
        Ok(())
    }

    fn render_spatial_block(
        &mut self,
        block: SpatialPropagationRenderBlock<'_>,
    ) -> Result<(), SpatialBackendRenderError> {
        let frames = self.graph.audio.frame_size as usize;
        if block.presentation_bank.len() != MAX_SPATIAL_PRESENTATION_FEEDS * frames
            || block.environmental_bank.len() != MAX_SPATIAL_ENVIRONMENT_PLANES * frames
        {
            return Err(SpatialBackendRenderError::InvalidBlockLength);
        }
        // This adapter retains the legacy session's coherence: its one copied
        // backend snapshot selects both activity and acoustics. The caller
        // supplies every configured program, including gated silence.
        let tap = self
            .graph
            .spatial_export
            .as_mut()
            .expect("export tap enabled");
        tap.presentation.fill(0.0);
        tap.environment.fill(0.0);
        tap.metadata = fightbox_runtime::backend::SpatialOutputMetadata {
            sample_rate_hz: self.graph.audio.sample_rate_hz as u32,
            block_size_frames: frames as u32,
            block_start_frame: block.block_start_frame,
            generation: self.graph.world.generation,
            discontinuity_sequence: block.metadata.discontinuity_sequence,
            active_environmental_order: SpatialAmbisonicOrder::Two,
            active_environmental_plane_count: 9,
            environmental_basis: SpatialEnvironmentalBasis::RightHandedXRightYUpZBack,
            world_space_unrotated: true,
            source_drive_applied: true,
            source_safety_gain_applied: true,
            ..fightbox_runtime::backend::SpatialOutputMetadata::default()
        };
        self.left.fill(0.0);
        self.right.fill(0.0);
        self.graph
            .render_program_block(fightbox_runtime::ProgramRenderBlock {
                listener_orientation: ListenerOrientation {
                    forward: ApiEnuVector3::new(0.0, 1.0, 0.0),
                    up: ApiEnuVector3::new(0.0, 0.0, 1.0),
                },
                sources: block.sources,
                output_left: &mut self.left,
                output_right: &mut self.right,
            })
            .map_err(|error| match error {
                BackendRenderError::InvalidBlockLength => {
                    SpatialBackendRenderError::InvalidBlockLength
                }
                BackendRenderError::InvalidSourceIndex => {
                    SpatialBackendRenderError::InvalidSourceIndex
                }
                BackendRenderError::InactiveGraph => SpatialBackendRenderError::InactiveGraph,
            })?;
        let tap = self
            .graph
            .spatial_export
            .as_mut()
            .expect("export tap enabled");
        if !tap
            .presentation
            .iter()
            .chain(&tap.environment)
            .all(|sample| sample.is_finite())
        {
            return Err(SpatialBackendRenderError::InvalidOutputMetadata);
        }
        tap.metadata.active_presentation_feed_count = tap
            .metadata
            .presentation_feeds
            .iter()
            .filter(|feed| feed.valid)
            .count();
        tap.metadata.validity = SpatialOutputValidity::Valid;
        block.presentation_bank.copy_from_slice(&tap.presentation);
        block.environmental_bank.copy_from_slice(&tap.environment);
        *block.metadata = tap.metadata;
        Ok(())
    }
}
