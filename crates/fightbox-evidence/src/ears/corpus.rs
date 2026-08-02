//! Deterministic, in-memory Gate 0 corruption corpus.

use super::Pcm;

/// Corpus sample rate.
pub const SAMPLE_RATE_HZ: u32 = 48_000;
const DURATION_S: usize = 3;
const SAMPLE_COUNT: usize = SAMPLE_RATE_HZ as usize * DURATION_S;

/// FFT window used by [`moving_notch_orbit`] and its moving-notch proof.
pub const MOVING_NOTCH_WINDOW_FRAMES: usize = 16_384;
const MOVING_NOTCH_WINDOW_COUNT: usize = 8;
const HRTF_DELAYS: [usize; MOVING_NOTCH_WINDOW_COUNT] = [96, 128, 160, 192, 112, 176, 144, 208];
const PATH_DELAYS: [usize; MOVING_NOTCH_WINDOW_COUNT] = [224, 160, 240, 176, 208, 144, 192, 128];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corruption {
    SlapbackEcho,
    CoherentComb,
    Zipper,
    FlippedHrtf,
    OcclusionAsVolume,
    SpliceClicks,
    SteppedPitch,
    MonoCollapsed,
    SteppedEnclosure,
    PumpingLimiter,
    AbruptCull,
}

/// The locked v2.4 corruption classes, in stable report order.
pub const CORRUPTIONS: [Corruption; 11] = [
    Corruption::SlapbackEcho,
    Corruption::CoherentComb,
    Corruption::Zipper,
    Corruption::FlippedHrtf,
    Corruption::OcclusionAsVolume,
    Corruption::SpliceClicks,
    Corruption::SteppedPitch,
    Corruption::MonoCollapsed,
    Corruption::SteppedEnclosure,
    Corruption::PumpingLimiter,
    Corruption::AbruptCull,
];

#[derive(Debug, Clone)]
/// Owned deinterleaved stereo PCM.
pub struct Stereo {
    /// Left-channel samples.
    pub left: Vec<f32>,
    /// Right-channel samples.
    pub right: Vec<f32>,
}

impl Stereo {
    /// Borrow this generated buffer as extractor input.
    pub fn pcm(&self) -> Pcm<'_> {
        Pcm {
            left: &self.left,
            right: &self.right,
            sample_rate_hz: SAMPLE_RATE_HZ,
        }
    }
}

/// Matched synthetic captures for reference-differenced moving-notch tests.
///
/// The “HRTF” here is deliberately adversarial rather than physiological: a
/// deep regular notch family changes spacing in every analysis window. That
/// makes the old source-referenced detector false-positive deterministically
/// and proves that a matched point capture removes direction-dependent transfer
/// structure. `honest_width` is an exact magnitude-preserving Hilbert phase
/// rotation of Gate 0's center signal behind that common transfer.
/// `coherent_path_sweep` adds a second, independently moving coherent delay
/// family before the same transfer, modeling the prohibited satellite
/// path-length sweep.
#[derive(Debug, Clone)]
pub struct MovingNotchOrbit {
    /// Exact mono program before the synthetic moving HRTF.
    pub source_mono: Vec<f32>,
    /// Point-shaped dual-mono input through the moving HRTF.
    pub point_reference: Stereo,
    /// Gate 0's honest wide capture through the same moving HRTF.
    pub honest_width: Stereo,
    /// Point input plus a walking coherent copy through the same moving HRTF.
    pub coherent_path_sweep: Stereo,
}

/// Generate the deterministic Wave 11 HRTF-versus-comb discrimination corpus.
///
/// Filters are circular within each [`MOVING_NOTCH_WINDOW_FRAMES`] block so
/// the known transfer functions are stationary inside an FFT window. The final
/// partial block is copied unchanged and is intentionally outside the eight
/// complete proof windows.
pub fn moving_notch_orbit() -> MovingNotchOrbit {
    let wide = clean();
    let source_mono = wide
        .left
        .iter()
        .zip(&wide.right)
        .map(|(&left, &right)| 0.5 * (left + right))
        .collect::<Vec<_>>();
    let point_input = Stereo {
        left: source_mono.clone(),
        right: source_mono.clone(),
    };
    let quadrature = windowed_quadrature(&source_mono);
    let angle = 35.0_f64.to_radians();
    let honest_input = Stereo {
        left: source_mono
            .iter()
            .zip(&quadrature)
            .map(|(&center, &quadrature)| {
                (angle.cos() * f64::from(center) + angle.sin() * f64::from(quadrature)) as f32
            })
            .collect(),
        right: source_mono
            .iter()
            .zip(&quadrature)
            .map(|(&center, &quadrature)| {
                (angle.cos() * f64::from(center) - angle.sin() * f64::from(quadrature)) as f32
            })
            .collect(),
    };
    let point_reference = map_windowed_delays(&point_input, &HRTF_DELAYS, 0.995);
    let honest_width = map_windowed_delays(&honest_input, &HRTF_DELAYS, 0.995);
    let walking_path = map_windowed_delays(&point_input, &PATH_DELAYS, 0.98);
    let coherent_path_sweep = map_windowed_delays(&walking_path, &HRTF_DELAYS, 0.995);
    MovingNotchOrbit {
        source_mono,
        point_reference,
        honest_width,
        coherent_path_sweep,
    }
}

/// Generate the deterministic clean control.
pub fn clean() -> Stereo {
    let mut rng = Rng::new(0x6a09_e667_f3bc_c909);
    let mut carrier = vec![0.0_f32; SAMPLE_COUNT];
    let mut colored = 0.0_f64;
    for (index, sample) in carrier.iter_mut().enumerate() {
        let white = rng.bipolar();
        colored = 0.72 * colored + 0.28 * white;
        let time = index as f64 / SAMPLE_RATE_HZ as f64;
        let tone = 0.025 * (std::f64::consts::TAU * 440.0 * time).sin();
        let upper = 0.025 * (std::f64::consts::TAU * 3_700.0 * time).sin();
        *sample = (0.15 * colored + tone + upper) as f32;
    }
    let fade = SAMPLE_RATE_HZ as usize / 50;
    for index in 0..fade {
        let gain = (index as f64 / fade as f64 * std::f64::consts::FRAC_PI_2)
            .sin()
            .powi(2) as f32;
        carrier[index] *= gain;
        carrier[SAMPLE_COUNT - 1 - index] *= gain;
    }
    spatialize(&carrier, 18, 0.72)
}

/// Generate one deterministic corruption class.
pub fn corrupted(kind: Corruption) -> Stereo {
    let clean = clean();
    match kind {
        Corruption::SlapbackEcho => delay_add(clean, 1_440, 0.72),
        Corruption::CoherentComb => delay_add(clean, 96, 0.82),
        Corruption::Zipper => map_gain(clean, |index| {
            if (index / 5_760).is_multiple_of(2) {
                1.0
            } else {
                0.48
            }
        }),
        Corruption::FlippedHrtf => flipped_hrtf(),
        Corruption::OcclusionAsVolume => map_gain(clean, |_| 0.32),
        Corruption::SpliceClicks => {
            let mut output = clean;
            for index in [SAMPLE_COUNT / 3, SAMPLE_COUNT * 2 / 3] {
                output.left[index] += 1.5;
                output.right[index] += 1.5;
            }
            output
        }
        Corruption::SteppedPitch => stepped_pitch(),
        Corruption::MonoCollapsed => {
            let mut output = clean;
            for index in 0..SAMPLE_COUNT {
                let center = 0.5 * (output.left[index] + output.right[index]);
                output.left[index] = center;
                output.right[index] = center;
            }
            output
        }
        Corruption::SteppedEnclosure => {
            let mut output = clean.clone();
            let switch = SAMPLE_COUNT / 2;
            for index in switch..SAMPLE_COUNT {
                let left_early = index.checked_sub(336).map_or(0.0, |at| clean.left[at]);
                let left_late = index.checked_sub(816).map_or(0.0, |at| clean.left[at]);
                let right_early = index.checked_sub(432).map_or(0.0, |at| clean.right[at]);
                let right_late = index.checked_sub(912).map_or(0.0, |at| clean.right[at]);
                output.left[index] += 0.62 * left_early + 0.34 * left_late;
                output.right[index] += 0.62 * right_early + 0.34 * right_late;
            }
            output
        }
        Corruption::PumpingLimiter => map_gain(clean, |index| {
            let time = index as f64 / SAMPLE_RATE_HZ as f64;
            (0.72 + 0.26 * (std::f64::consts::TAU * 3.0 * time).sin()) as f32
        }),
        Corruption::AbruptCull => {
            let mut output = clean;
            let cull = SAMPLE_COUNT * 2 / 3;
            output.left[cull..].fill(0.0);
            output.right[cull..].fill(0.0);
            output
        }
    }
}

fn spatialize(carrier: &[f32], delay: usize, left_gain: f32) -> Stereo {
    let mut left = vec![0.0; carrier.len()];
    let mut right = vec![0.0; carrier.len()];
    let mut rng_l = Rng::new(0xbb67_ae85_84ca_a73b);
    let mut rng_r = Rng::new(0x3c6e_f372_fe94_f82b);
    for index in 0..carrier.len() {
        left[index] = if index >= delay {
            left_gain * carrier[index - delay]
        } else {
            0.0
        } + 0.035 * rng_l.bipolar() as f32;
        right[index] = carrier[index] + 0.035 * rng_r.bipolar() as f32;
    }
    Stereo { left, right }
}

fn delay_add(mut signal: Stereo, delay: usize, gain: f32) -> Stereo {
    let dry = signal.clone();
    for index in delay..SAMPLE_COUNT {
        signal.left[index] += gain * dry.left[index - delay];
        signal.right[index] += gain * dry.right[index - delay];
    }
    signal
}

fn map_windowed_delays(signal: &Stereo, delays: &[usize], gain: f32) -> Stereo {
    Stereo {
        left: windowed_delay_add(&signal.left, delays, gain),
        right: windowed_delay_add(&signal.right, delays, gain),
    }
}

fn windowed_delay_add(signal: &[f32], delays: &[usize], gain: f32) -> Vec<f32> {
    let mut output = signal.to_vec();
    for (window, &delay) in delays.iter().enumerate() {
        let start = window * MOVING_NOTCH_WINDOW_FRAMES;
        let end = start + MOVING_NOTCH_WINDOW_FRAMES;
        if end > signal.len() || delay == 0 || delay >= MOVING_NOTCH_WINDOW_FRAMES {
            break;
        }
        for offset in 0..MOVING_NOTCH_WINDOW_FRAMES {
            let delayed_offset =
                (offset + MOVING_NOTCH_WINDOW_FRAMES - delay) % MOVING_NOTCH_WINDOW_FRAMES;
            output[start + offset] = signal[start + offset] + gain * signal[start + delayed_offset];
        }
    }
    output
}

fn windowed_quadrature(signal: &[f32]) -> Vec<f32> {
    let mut output = vec![0.0; signal.len()];
    for window in 0..MOVING_NOTCH_WINDOW_COUNT {
        let start = window * MOVING_NOTCH_WINDOW_FRAMES;
        let end = start + MOVING_NOTCH_WINDOW_FRAMES;
        if end > signal.len() {
            break;
        }
        let mut real = signal[start..end]
            .iter()
            .map(|sample| f64::from(*sample))
            .collect::<Vec<_>>();
        let mut imaginary = vec![0.0; MOVING_NOTCH_WINDOW_FRAMES];
        fft_in_place(&mut real, &mut imaginary);
        real[0] = 0.0;
        imaginary[0] = 0.0;
        real[MOVING_NOTCH_WINDOW_FRAMES / 2] = 0.0;
        imaginary[MOVING_NOTCH_WINDOW_FRAMES / 2] = 0.0;
        for bin in 1..MOVING_NOTCH_WINDOW_FRAMES / 2 {
            let old_real = real[bin];
            real[bin] = imaginary[bin];
            imaginary[bin] = -old_real;
        }
        for bin in MOVING_NOTCH_WINDOW_FRAMES / 2 + 1..MOVING_NOTCH_WINDOW_FRAMES {
            let old_real = real[bin];
            real[bin] = -imaginary[bin];
            imaginary[bin] = old_real;
        }
        inverse_fft_in_place(&mut real, &mut imaginary);
        for (target, value) in output[start..end].iter_mut().zip(real) {
            *target = value as f32;
        }
    }
    output
}

fn inverse_fft_in_place(real: &mut [f64], imaginary: &mut [f64]) {
    for value in imaginary.iter_mut() {
        *value = -*value;
    }
    fft_in_place(real, imaginary);
    let scale = 1.0 / real.len() as f64;
    for (real, imaginary) in real.iter_mut().zip(imaginary) {
        *real *= scale;
        *imaginary *= -scale;
    }
}

fn fft_in_place(real: &mut [f64], imaginary: &mut [f64]) {
    let length = real.len();
    debug_assert!(length.is_power_of_two() && imaginary.len() == length);
    let mut target = 0;
    for source in 1..length {
        let mut bit = length >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target ^= bit;
        if source < target {
            real.swap(source, target);
            imaginary.swap(source, target);
        }
    }
    let mut butterfly_length = 2;
    while butterfly_length <= length {
        let angle = -std::f64::consts::TAU / butterfly_length as f64;
        let step_real = angle.cos();
        let step_imaginary = angle.sin();
        for start in (0..length).step_by(butterfly_length) {
            let mut twiddle_real = 1.0;
            let mut twiddle_imaginary = 0.0;
            for offset in 0..butterfly_length / 2 {
                let even = start + offset;
                let odd = even + butterfly_length / 2;
                let odd_real = real[odd] * twiddle_real - imaginary[odd] * twiddle_imaginary;
                let odd_imaginary = real[odd] * twiddle_imaginary + imaginary[odd] * twiddle_real;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;
                let next_real = twiddle_real * step_real - twiddle_imaginary * step_imaginary;
                twiddle_imaginary = twiddle_real * step_imaginary + twiddle_imaginary * step_real;
                twiddle_real = next_real;
            }
        }
        butterfly_length *= 2;
    }
}

fn map_gain(mut signal: Stereo, gain: impl Fn(usize) -> f32) -> Stereo {
    for index in 0..SAMPLE_COUNT {
        let gain = gain(index);
        signal.left[index] *= gain;
        signal.right[index] *= gain;
    }
    signal
}

fn flipped_hrtf() -> Stereo {
    let clean = clean();
    let mono: Vec<f32> = clean
        .left
        .iter()
        .zip(&clean.right)
        .map(|(&left, &right)| 0.5 * (left + right))
        .collect();
    // Preserve right-ear dominance while reversing only the arrival-time cue:
    // right is delayed but remains louder, producing conflicting ITD/ILD signs.
    let mut left = vec![0.0; SAMPLE_COUNT];
    let mut right = vec![0.0; SAMPLE_COUNT];
    for index in 0..SAMPLE_COUNT {
        left[index] = 0.72 * mono[index];
        right[index] = if index >= 18 { mono[index - 18] } else { 0.0 };
    }
    Stereo { left, right }
}

fn stepped_pitch() -> Stereo {
    let mut rng = Rng::new(0xa54f_f53a_5f1d_36f1);
    let mut carrier = vec![0.0; SAMPLE_COUNT];
    let mut phase = 0.0_f64;
    for (index, sample) in carrier.iter_mut().enumerate() {
        let frequency = if index < SAMPLE_COUNT / 2 {
            440.0
        } else {
            554.365
        };
        phase += std::f64::consts::TAU * frequency / SAMPLE_RATE_HZ as f64;
        *sample = (0.16 * phase.sin() + 0.035 * rng.bipolar()) as f32;
    }
    spatialize(&carrier, 18, 0.72)
}

#[derive(Debug, Clone, Copy)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn bipolar(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let unit = (self.0 >> 11) as f64 / (1_u64 << 53) as f64;
        unit * 2.0 - 1.0
    }
}
