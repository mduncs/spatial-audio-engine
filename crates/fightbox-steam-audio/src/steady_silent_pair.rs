use super::{OwnedAudioBuffer, ffi, handle};

const FRAMES: usize = 128;
const STEADY_BLOCKS: u8 = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
struct ParameterBits {
    values: [u32; 16],
    hrtf: usize,
    peak_delays: usize,
    zero_signs: [u64; 2],
}

impl ParameterBits {
    fn new(
        direct: &ffi::IPLDirectEffectParams,
        binaural: &ffi::IPLBinauralEffectParams,
        input: &[f32],
    ) -> Option<Self> {
        if input.len() != FRAMES || !binaural.peakDelays.is_null() {
            return None;
        }
        let mut zero_signs = [0; 2];
        for (index, sample) in input.iter().enumerate() {
            if *sample != 0.0 {
                return None;
            }
            zero_signs[index / 64] |= u64::from(sample.is_sign_negative()) << (index % 64);
        }
        Some(Self {
            values: [
                direct.flags as u32,
                direct.transmissionType as u32,
                direct.distanceAttenuation.to_bits(),
                direct.airAbsorption[0].to_bits(),
                direct.airAbsorption[1].to_bits(),
                direct.airAbsorption[2].to_bits(),
                direct.directivity.to_bits(),
                direct.occlusion.to_bits(),
                direct.transmission[0].to_bits(),
                direct.transmission[1].to_bits(),
                direct.transmission[2].to_bits(),
                binaural.direction.x.to_bits(),
                binaural.direction.y.to_bits(),
                binaural.direction.z.to_bits(),
                binaural.interpolation as u32,
                binaural.spatialBlend.to_bits(),
            ],
            hrtf: binaural.hrtf as usize,
            peak_delays: binaural.peakDelays as usize,
            zero_signs,
        })
    }
}

pub(super) struct SteadySilentPair {
    parameters: Option<ParameterBits>,
    steady_blocks: u8,
    filtered: [f32; FRAMES],
    stereo: [f32; FRAMES * 2],
    #[cfg(test)]
    pub(super) reused_blocks: u64,
}

impl SteadySilentPair {
    pub(super) fn new() -> Self {
        Self {
            parameters: None,
            steady_blocks: 0,
            filtered: [0.0; FRAMES],
            stereo: [0.0; FRAMES * 2],
            #[cfg(test)]
            reused_blocks: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.parameters = None;
        self.steady_blocks = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &mut self,
        enabled: bool,
        input_samples: &[f32],
        direct_effect: usize,
        binaural_effect: usize,
        direct_params: &mut ffi::IPLDirectEffectParams,
        binaural_params: &mut ffi::IPLBinauralEffectParams,
        input: &mut ffi::IPLAudioBuffer,
        filtered: &mut OwnedAudioBuffer,
        stereo: &mut OwnedAudioBuffer,
        output: &mut [f32],
    ) {
        let parameters = enabled
            .then(|| ParameterBits::new(direct_params, binaural_params, input_samples))
            .flatten();
        if parameters.is_some()
            && parameters == self.parameters
            && self.steady_blocks >= STEADY_BLOCKS
        {
            // Echo taps share their owned output buffers. Restore both stages
            // so replay is independent of whichever tap rendered before this.
            filtered.write_mono(&mut self.filtered);
            stereo.write_interleaved(&mut self.stereo);
            output.copy_from_slice(&self.stereo);
            #[cfg(test)]
            {
                self.reused_blocks += 1;
            }
            return;
        }

        let mut raw_filtered = filtered.raw();
        let mut raw_stereo = stereo.raw();
        ffi::direct_effect_apply(
            handle(direct_effect),
            direct_params,
            input,
            &mut raw_filtered,
        );
        ffi::binaural_effect_apply(
            handle(binaural_effect),
            binaural_params,
            &mut raw_filtered,
            &mut raw_stereo,
        );
        stereo.read_interleaved(output);
        let Some(parameters) = parameters else {
            self.reset();
            return;
        };
        let mut current_filtered = [0.0; FRAMES];
        filtered.read_interleaved(&mut current_filtered);
        if !current_filtered
            .iter()
            .chain(output.iter())
            .all(|sample| sample.is_finite())
        {
            self.reset();
            return;
        }
        let identical = self.parameters == Some(parameters)
            && current_filtered
                .iter()
                .zip(&self.filtered)
                .all(|(a, b)| a.to_bits() == b.to_bits())
            && output
                .iter()
                .zip(&self.stereo)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        self.steady_blocks = if identical {
            self.steady_blocks.saturating_add(1)
        } else {
            1
        };
        self.parameters = Some(parameters);
        self.filtered.copy_from_slice(&current_filtered);
        self.stereo.copy_from_slice(output);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BinauralBits {
    values: [u32; 5],
    hrtf: usize,
    peak_delays: usize,
}

impl BinauralBits {
    fn new(params: &ffi::IPLBinauralEffectParams) -> Self {
        Self {
            values: [
                params.direction.x.to_bits(),
                params.direction.y.to_bits(),
                params.direction.z.to_bits(),
                params.interpolation as u32,
                params.spatialBlend.to_bits(),
            ],
            hrtf: params.hrtf as usize,
            peak_delays: params.peakDelays as usize,
        }
    }
}

pub(super) struct SteadyBinaural {
    parameters: Option<BinauralBits>,
    steady_blocks: u8,
    input: [f32; FRAMES],
    stereo: [f32; FRAMES * 2],
    #[cfg(test)]
    pub(super) reused_blocks: u64,
}

impl SteadyBinaural {
    pub(super) fn new() -> Self {
        Self {
            parameters: None,
            steady_blocks: 0,
            input: [0.0; FRAMES],
            stereo: [0.0; FRAMES * 2],
            #[cfg(test)]
            reused_blocks: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.parameters = None;
        self.steady_blocks = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &mut self,
        enabled: bool,
        filtered_input: &[f32],
        effect: usize,
        params: &mut ffi::IPLBinauralEffectParams,
        input: &mut ffi::IPLAudioBuffer,
        stereo: &mut OwnedAudioBuffer,
        output: &mut [f32],
    ) {
        let parameters = (enabled && filtered_input.len() == FRAMES && params.peakDelays.is_null())
            .then(|| BinauralBits::new(params));
        let same_input = filtered_input
            .iter()
            .zip(&self.input)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        if parameters.is_some()
            && parameters == self.parameters
            && same_input
            && self.steady_blocks >= STEADY_BLOCKS
        {
            stereo.write_interleaved(&mut self.stereo);
            output.copy_from_slice(&self.stereo);
            #[cfg(test)]
            {
                self.reused_blocks += 1;
            }
            return;
        }
        let mut raw_stereo = stereo.raw();
        ffi::binaural_effect_apply(handle(effect), params, input, &mut raw_stereo);
        stereo.read_interleaved(output);
        let Some(parameters) = parameters else {
            self.reset();
            return;
        };
        if !filtered_input
            .iter()
            .chain(output.iter())
            .all(|sample| sample.is_finite())
        {
            self.reset();
            return;
        }
        let identical = self.parameters == Some(parameters)
            && same_input
            && output
                .iter()
                .zip(&self.stereo)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        self.steady_blocks = if identical {
            self.steady_blocks.saturating_add(1)
        } else {
            1
        };
        self.parameters = Some(parameters);
        self.input.copy_from_slice(filtered_input);
        self.stereo.copy_from_slice(output);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PathBits {
    eq: [u32; 3],
    sh: [u32; 16],
    listener: [u32; 12],
    options: [u32; 3],
    hrtf: usize,
    sh_pointer: usize,
    zero_signs: [u64; 2],
}

impl PathBits {
    fn new(params: &ffi::IPLPathEffectParams, sh: &[f32; 16], input: &[f32]) -> Option<Self> {
        if input.len() != FRAMES
            || !(0..=3).contains(&params.order)
            || !params
                .eqCoeffs
                .iter()
                .all(|gain| gain.is_finite() && *gain > 0.0)
        {
            return None;
        }
        let channels = (params.order as usize + 1).pow(2);
        // A zero spatial field can hide changing recursive EQ history. Keep
        // that route advancing natively even when its stereo output is zero.
        if !sh[..channels].iter().any(|coefficient| *coefficient != 0.0) {
            return None;
        }
        let mut zero_signs = [0; 2];
        for (index, sample) in input.iter().enumerate() {
            if *sample != 0.0 {
                return None;
            }
            zero_signs[index / 64] |= u64::from(sample.is_sign_negative()) << (index % 64);
        }
        let listener = params.listener;
        Some(Self {
            eq: params.eqCoeffs.map(f32::to_bits),
            sh: sh.map(f32::to_bits),
            listener: [
                listener.right.x.to_bits(),
                listener.right.y.to_bits(),
                listener.right.z.to_bits(),
                listener.up.x.to_bits(),
                listener.up.y.to_bits(),
                listener.up.z.to_bits(),
                listener.ahead.x.to_bits(),
                listener.ahead.y.to_bits(),
                listener.ahead.z.to_bits(),
                listener.origin.x.to_bits(),
                listener.origin.y.to_bits(),
                listener.origin.z.to_bits(),
            ],
            options: [
                params.order as u32,
                params.binaural as u32,
                params.normalizeEQ as u32,
            ],
            hrtf: params.hrtf as usize,
            sh_pointer: params.shCoeffs as usize,
            zero_signs,
        })
    }
}

pub(super) struct SteadyPath {
    parameters: Option<PathBits>,
    steady_blocks: u8,
    stereo: [f32; FRAMES * 2],
    #[cfg(test)]
    pub(super) reused_blocks: u64,
}

impl SteadyPath {
    pub(super) fn new() -> Self {
        Self {
            parameters: None,
            steady_blocks: 0,
            stereo: [0.0; FRAMES * 2],
            #[cfg(test)]
            reused_blocks: 0,
        }
    }

    pub(super) fn reset(&mut self) {
        self.parameters = None;
        self.steady_blocks = 0;
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn render(
        &mut self,
        enabled: bool,
        input_samples: &[f32],
        sh: &[f32; 16],
        effect: usize,
        params: &mut ffi::IPLPathEffectParams,
        input: &mut ffi::IPLAudioBuffer,
        stereo: &mut OwnedAudioBuffer,
        output: &mut [f32],
    ) {
        let parameters = enabled
            .then(|| PathBits::new(params, sh, input_samples))
            .flatten();
        if parameters.is_some()
            && parameters == self.parameters
            && self.steady_blocks >= STEADY_BLOCKS
        {
            stereo.write_interleaved(&mut self.stereo);
            output.copy_from_slice(&self.stereo);
            #[cfg(test)]
            {
                self.reused_blocks += 1;
            }
            return;
        }
        let mut raw_stereo = stereo.raw();
        ffi::path_effect_apply(handle(effect), params, input, &mut raw_stereo);
        stereo.read_interleaved(output);
        let Some(parameters) = parameters else {
            self.reset();
            return;
        };
        if !output.iter().all(|sample| sample.is_finite()) {
            self.reset();
            return;
        }
        let identical = self.parameters == Some(parameters)
            && output
                .iter()
                .zip(&self.stereo)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        self.steady_blocks = if identical {
            self.steady_blocks.saturating_add(1)
        } else {
            1
        };
        self.parameters = Some(parameters);
        self.stereo.copy_from_slice(output);
    }
}
