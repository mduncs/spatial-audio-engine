//! Deterministic Phase A signal generation.
//!
//! These generators produce finite, calibrated PCM suitable for S0/S3 mechanical
//! captures without committing binary audio. They are deliberately deterministic:
//! the same parameters always yield the same samples, so a stem hash is stable.
//!
//! Generator-normalization contract (an evidence-local fact):
//!   - the raw signal is measured first; its RMS in dBFS is recorded as
//!     `raw_rms_dbfs` (the generator's pre-normalization level);
//!   - a normalization gain is then applied to reach the requested target
//!     program RMS, and that gain in dB is recorded as `normalization_gain_db`;
//!   - the delivered samples therefore measure the target program RMS, which is
//!     also recorded as `target_rms_dbfs`.
//!
//! This [`GeneratorNormalization`] is **not** the physical source drive. It only
//! describes how the deterministic generator produces a buffer at a chosen
//! program RMS. The scene-owned source drive that maps a declared SPL to pre-
//! propagation PCM is a separate, single gain chain owned by
//! [`fightbox_api::SceneCalibration::derive_source_drive`] (ADR 0002). Keeping
//! the two distinct is the point of this record: a generator normalization gain
//! and a physical source drive must never be confused for one another or folded
//! into a second caller-supplied loudness gain.
//!
//! The target must be a finite value strictly below 0 dBFS, and the gain must
//! not push any delivered sample's absolute peak past 1.0. A target that would
//! clip is rejected with [`SignalError::PeakExceedsFullScale`] rather than
//! clipped silently, so a normalization signal never hides a non-representable
//! full-scale value.
//!
//! Nothing here makes a delivered-ear-SPL claim. That requires a measured
//! output-device transfer, which Phase A does not have (authority note §ρ).

use crate::wav::{WavError, WavSpec, validate_spec};

/// Which deterministic generator produced a signal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalKind {
    Sine,
    Multitone,
    PinkLike,
    FireworkBurst,
}
impl SignalKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Multitone => "multitone",
            Self::PinkLike => "pink_like",
            Self::FireworkBurst => "firework_burst",
        }
    }
}

/// Fixed seed for [`firework_burst`]. The seed is part of the evidence
/// contract so the broadband crack and crackle train remain byte-stable.
pub const FIREWORK_BURST_SEED: u64 = 0xF1AE_2026_0729_0048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalError {
    BadSpec(WavError),
    EmptyMultitone,
    InvalidFrequency,
    /// `target_rms_dbfs` was not a finite value strictly below 0 dBFS.
    InvalidTarget,
    /// Applying the calibration gain would push a sample's absolute peak past
    /// 1.0. The result is rejected rather than clipped silently.
    PeakExceedsFullScale,
    SilentSource,
}

impl SignalError {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::BadSpec(_) => "wav spec was invalid",
            Self::EmptyMultitone => "multitone requires at least one frequency",
            Self::InvalidFrequency => "frequency must be finite, positive, and below Nyquist",
            Self::InvalidTarget => "target_rms_dbfs must be finite and strictly below 0 dBFS",
            Self::PeakExceedsFullScale => {
                "calibration gain would push the absolute peak past 1.0 (no silent clipping)"
            }
            Self::SilentSource => "raw signal is silent; no gain is defined",
        }
    }
}

/// The deterministic generator's normalization fact: an evidence-local record
/// of how a generated buffer was brought to its target program RMS.
///
/// Fields:
/// - `raw_rms_dbfs` — the generator's **pre-normalization** RMS in dBFS,
///   measured before the normalization gain is applied;
/// - `target_rms_dbfs` — the **program RMS** the delivered buffer measures after
///   normalization (the level the asset is built to);
/// - `normalization_gain_db` — the gain in dB applied to move from the raw level
///   to the target. By construction `target_rms_dbfs = raw_rms_dbfs +
///   normalization_gain_db`.
///
/// This is distinct from the scene-owned source drive. It is not a physical
/// loudness gain and carries no SPL meaning; see the module docs and ADR 0002.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GeneratorNormalization {
    pub raw_rms_dbfs: f32,
    pub target_rms_dbfs: f32,
    pub normalization_gain_db: f32,
}

/// A generated, normalized interleaved buffer plus its generator-normalization
/// record.
#[derive(Clone, Debug, PartialEq)]
pub struct GeneratedSignal {
    pub kind: SignalKind,
    pub spec: WavSpec,
    pub samples: Vec<f32>,
    pub normalization: GeneratorNormalization,
}

impl GeneratedSignal {
    /// Whole frames (samples divided by channel count).
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.spec.channels).max(1)
    }

    /// Analyze the delivered buffer after generator normalization.
    ///
    /// This measures the normalized asset PCM; it does not apply or fold in a
    /// scene source drive. [`GeneratorNormalization`] remains a separate fact
    /// in capture records.
    pub fn analyze(
        &self,
    ) -> Result<crate::analysis::AnalyzedAsset, crate::analysis::AssetAnalysisError> {
        crate::analysis::analyze_decoded_asset(self.spec, &self.samples)
    }
}

/// A single calibrated sine wave at full-scale peak before calibration.
pub fn sine(
    spec: WavSpec,
    frequency_hz: f32,
    frame_count: usize,
    target_rms_dbfs: f32,
) -> Result<GeneratedSignal, SignalError> {
    validate_spec(spec).map_err(SignalError::BadSpec)?;
    if !frequency_hz.is_finite() || frequency_hz <= 0.0 {
        return Err(SignalError::InvalidFrequency);
    }
    let nyquist = spec.sample_rate_hz as f32 * 0.5;
    if frequency_hz >= nyquist {
        return Err(SignalError::InvalidFrequency);
    }
    let mut samples = interleave_buffer(spec.channels, frame_count, |n| {
        (2.0 * std::f32::consts::PI * frequency_hz * n as f32 / spec.sample_rate_hz as f32).sin()
    });
    let normalization = calibrate(&mut samples, target_rms_dbfs)?;
    Ok(GeneratedSignal {
        kind: SignalKind::Sine,
        spec,
        samples,
        normalization,
    })
}

/// A sum of equal-amplitude sine partials, each at `1 / count` amplitude so the
/// raw peak stays at or below full scale before calibration.
pub fn multitone(
    spec: WavSpec,
    frequencies_hz: &[f32],
    frame_count: usize,
    target_rms_dbfs: f32,
) -> Result<GeneratedSignal, SignalError> {
    validate_spec(spec).map_err(SignalError::BadSpec)?;
    if frequencies_hz.is_empty() {
        return Err(SignalError::EmptyMultitone);
    }
    let nyquist = spec.sample_rate_hz as f32 * 0.5;
    for &f in frequencies_hz {
        if !f.is_finite() || f <= 0.0 || f >= nyquist {
            return Err(SignalError::InvalidFrequency);
        }
    }
    let inv_count = 1.0 / frequencies_hz.len() as f32;
    let mut samples = interleave_buffer(spec.channels, frame_count, |n| {
        let t = n as f32 / spec.sample_rate_hz as f32;
        frequencies_hz
            .iter()
            .map(|&f| (2.0 * std::f32::consts::PI * f * t).sin() * inv_count)
            .sum::<f32>()
    });
    let normalization = calibrate(&mut samples, target_rms_dbfs)?;
    Ok(GeneratedSignal {
        kind: SignalKind::Multitone,
        spec,
        samples,
        normalization,
    })
}

/// A deterministic pink-like broadband source.
///
/// White samples come from a splitmix64 sequence seeded by `seed`; a Paul Kellet
/// pink filter (a well-known economical approximation) shapes them. The result is
/// a reproducible pink-ish reference, not a measurement-grade pink generator, and
/// it makes no delivered-ear-SPL claim. Stereo output duplicates the same mono
/// content into every channel so channel-health and stereo-difference metrics
/// are well defined on a calibration signal.
pub fn pink_like(
    spec: WavSpec,
    seed: u64,
    frame_count: usize,
    target_rms_dbfs: f32,
) -> Result<GeneratedSignal, SignalError> {
    validate_spec(spec).map_err(SignalError::BadSpec)?;
    let mut state = seed;
    let mut b0 = 0.0_f32;
    let mut b1 = 0.0_f32;
    let mut b2 = 0.0_f32;
    let mut samples = interleave_buffer(spec.channels, frame_count, |_| {
        let white = next_uniform(&mut state);
        b0 = 0.99765_f32 * b0 + white * 0.0990460_f32;
        b1 = 0.96300_f32 * b1 + white * 0.2965164_f32;
        b2 = 0.57000_f32 * b2 + white * 1.0526913_f32;
        b0 + b1 + b2 + white * 0.1848_f32
    });
    let normalization = calibrate(&mut samples, target_rms_dbfs)?;
    Ok(GeneratedSignal {
        kind: SignalKind::PinkLike,
        spec,
        samples,
        normalization,
    })
}

/// A deterministic firework-like impulse with a sharp broadband crack, a
/// decaying low-frequency boom, and a short train of crackle transients.
///
/// The first sample is intentionally nonzero. Broadband components use the
/// fixed [`FIREWORK_BURST_SEED`]; callers cannot accidentally make a
/// non-reproducible qualification stimulus. The requested buffer should be at
/// least 250 ms long to retain the complete crackle train.
pub fn firework_burst(
    spec: WavSpec,
    frame_count: usize,
    target_rms_dbfs: f32,
) -> Result<GeneratedSignal, SignalError> {
    validate_spec(spec).map_err(SignalError::BadSpec)?;
    let sample_rate = spec.sample_rate_hz as f32;
    let mut state = FIREWORK_BURST_SEED;
    let crackles = [
        (0.043_f32, 0.42_f32),
        (0.079, 0.31),
        (0.131, 0.24),
        (0.207, 0.18),
    ];
    let mut previous_noise = 0.0_f32;
    let mut samples = interleave_buffer(spec.channels, frame_count, |frame| {
        let t = frame as f32 / sample_rate;
        let noise = next_uniform(&mut state);
        let high_pass_noise = noise - previous_noise * 0.82;
        previous_noise = noise;

        // Roughly 9 ms of fast broadband crack, with an explicit onset.
        let crack = if t < 0.009 {
            let onset = if frame == 0 { 1.0 } else { 0.0 };
            onset + 0.72 * high_pass_noise * (-420.0 * t).exp()
        } else {
            0.0
        };

        // Delayed low boom: two slightly inharmonic components avoid a pure
        // calibration-tone character while retaining a smooth physical decay.
        let boom_t = t - 0.012;
        let boom = if boom_t >= 0.0 {
            let envelope = (-3.8 * boom_t).exp();
            envelope
                * (0.34 * (std::f32::consts::TAU * 52.0 * boom_t).sin()
                    + 0.16 * (std::f32::consts::TAU * 83.0 * boom_t).sin())
        } else {
            0.0
        };

        let crackle = crackles
            .iter()
            .map(|(start_s, amplitude)| {
                let local_t = t - start_s;
                if (0.0..0.004).contains(&local_t) {
                    amplitude * high_pass_noise * (-900.0 * local_t).exp()
                } else {
                    0.0
                }
            })
            .sum::<f32>();

        crack + boom + crackle
    });
    let normalization = calibrate(&mut samples, target_rms_dbfs)?;
    Ok(GeneratedSignal {
        kind: SignalKind::FireworkBurst,
        spec,
        samples,
        normalization,
    })
}

/// Measure a flat RMS over the interleaved buffer (RMS is channel-agnostic).
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq = samples
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>();
    (sum_sq / samples.len() as f64).sqrt() as f32
}

/// Measure raw RMS, compute the normalization gain to reach `target_rms_dbfs`,
/// apply it, and return the generator-normalization record. The delivered
/// samples measure the target program RMS.
///
/// `target_rms_dbfs` must be finite and strictly below 0 dBFS. After applying the
/// gain, any delivered sample whose absolute peak exceeds 1.0 is rejected with
/// [`SignalError::PeakExceedsFullScale`] — the layer never clips silently.
fn calibrate(
    samples: &mut [f32],
    target_rms_dbfs: f32,
) -> Result<GeneratorNormalization, SignalError> {
    if !target_rms_dbfs.is_finite() || target_rms_dbfs >= 0.0 {
        return Err(SignalError::InvalidTarget);
    }
    let raw_rms = rms(samples);
    if raw_rms <= 0.0 {
        return Err(SignalError::SilentSource);
    }
    let raw_rms_dbfs = 20.0 * raw_rms.log10();
    let normalization_gain_db = target_rms_dbfs - raw_rms_dbfs;
    let gain = 10.0_f32.powf(normalization_gain_db / 20.0);
    let mut peak = 0.0_f32;
    for sample in samples.iter_mut() {
        *sample *= gain;
        let abs = sample.abs();
        if abs > peak {
            peak = abs;
        }
    }
    if peak > 1.0 {
        return Err(SignalError::PeakExceedsFullScale);
    }
    Ok(GeneratorNormalization {
        raw_rms_dbfs,
        target_rms_dbfs,
        normalization_gain_db,
    })
}

/// Fill an interleaved buffer by evaluating `sample_at_frame(frame_index)` once
/// per frame and duplicating the value across channels.
fn interleave_buffer(
    channels: u16,
    frame_count: usize,
    mut sample_at_frame: impl FnMut(usize) -> f32,
) -> Vec<f32> {
    let ch = usize::from(channels);
    let mut out = Vec::with_capacity(frame_count * ch);
    for frame in 0..frame_count {
        let value = sample_at_frame(frame);
        for _ in 0..ch {
            out.push(value);
        }
    }
    out
}

/// splitmix64 step returning a deterministic `u64`, advancing `state`.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Map the high 24 bits of a splitmix64 word to a uniform float in `[-1, 1)`.
fn next_uniform(state: &mut u64) -> f32 {
    let bits = (splitmix64(state) >> 40) as u32; // 24 high bits
    let q = (bits as f32) / ((1u32 << 24) as f32); // [0, 1)
    q * 2.0 - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::channel_metrics;

    fn spec(channels: u16) -> WavSpec {
        WavSpec {
            sample_rate_hz: 48_000,
            channels,
        }
    }

    #[test]
    fn sine_reaches_target_rms_and_is_deterministic() {
        let spec = spec(1);
        let a = sine(spec, 1_000.0, 48_000, -20.0).unwrap();
        let b = sine(spec, 1_000.0, 48_000, -20.0).unwrap();
        assert_eq!(a.samples, b.samples);

        let metrics = channel_metrics(spec, &a.samples).unwrap();
        let delivered_dbfs = metrics.rms_dbfs_per_channel[0].unwrap();
        assert!((delivered_dbfs - (-20.0)).abs() < 0.05);
        // A full-scale sine references about -3.01 dBFS before normalization.
        assert!((a.normalization.raw_rms_dbfs - (-3.01)).abs() < 0.05);
        // The generator records the target program RMS and the normalization gain.
        assert!((a.normalization.target_rms_dbfs - (-20.0)).abs() < 0.05);
        assert!(
            (a.normalization.normalization_gain_db
                - (a.normalization.target_rms_dbfs - a.normalization.raw_rms_dbfs))
                .abs()
                < 1e-3
        );
    }

    #[test]
    fn multitone_rejects_empty_and_reaches_target() {
        let spec = spec(1);
        assert_eq!(
            multitone(spec, &[], 100, -20.0).unwrap_err(),
            SignalError::EmptyMultitone
        );
        let mt = multitone(spec, &[440.0, 880.0, 1760.0], 48_000, -18.0).unwrap();
        let metrics = channel_metrics(spec, &mt.samples).unwrap();
        assert!((metrics.rms_dbfs_per_channel[0].unwrap() - (-18.0)).abs() < 0.05);
    }

    #[test]
    fn pink_like_is_deterministic_and_distinct_per_seed() {
        let spec = spec(2);
        let a = pink_like(spec, 0xC0FFEE, 4_800, -20.0).unwrap();
        let b = pink_like(spec, 0xC0FFEE, 4_800, -20.0).unwrap();
        let other = pink_like(spec, 0x1234, 4_800, -20.0).unwrap();
        assert_eq!(a.samples, b.samples);
        assert_ne!(a.samples, other.samples);
        // Stereo duplicates mono content, so the side channel is silent.
        let metrics = channel_metrics(spec, &a.samples).unwrap();
        assert!(metrics.stereo_difference_rms.unwrap_or(1.0).abs() < 1e-6);
    }

    #[test]
    fn firework_burst_is_deterministic_impulsive_and_finite() {
        let spec = spec(1);
        let a = firework_burst(spec, 48_000 * 2, -30.0).unwrap();
        let b = firework_burst(spec, 48_000 * 2, -30.0).unwrap();
        assert_eq!(a.samples, b.samples);
        assert_eq!(a.kind, SignalKind::FireworkBurst);
        assert!(a.samples.iter().all(|sample| sample.is_finite()));

        let peak = a.samples.iter().copied().map(f32::abs).fold(0.0, f32::max);
        let rms = rms(&a.samples);
        let crest_factor = peak / rms;
        assert!(crest_factor > 8.0, "crest factor={crest_factor:.3}");
        let onset_peak = a.samples[..480]
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0, f32::max);
        let later_peak = a.samples[24_000..]
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0, f32::max);
        assert!(onset_peak > later_peak * 4.0);
    }

    #[test]
    fn convenience_analysis_measures_normalized_pcm_without_hiding_normalization() {
        let generated = sine(spec(1), 1_000.0, 48_000, -20.0).unwrap();
        let normalization = generated.normalization;
        let analyzed = generated.analyze().unwrap();

        assert!((analyzed.analysis().program_rms_dbfs - -20.0).abs() < 0.001);
        assert_eq!(
            analyzed.analysis().measurement_provenance.method_id,
            crate::analysis::ASSET_ANALYSIS_METHOD_ID
        );
        assert_eq!(generated.normalization, normalization);
        assert_ne!(
            generated.normalization.normalization_gain_db,
            analyzed.analysis().program_rms_dbfs
        );
    }

    #[test]
    fn rejects_bad_frequency_and_target() {
        let spec = spec(1);
        assert_eq!(
            sine(spec, 0.0, 100, -20.0).unwrap_err(),
            SignalError::InvalidFrequency
        );
        assert_eq!(
            sine(spec, 30_000.0, 100, -20.0).unwrap_err(),
            SignalError::InvalidFrequency
        );
        // NaN/Inf targets are not finite.
        assert_eq!(
            sine(spec, 1_000.0, 100, f32::NAN).unwrap_err(),
            SignalError::InvalidTarget
        );
        assert_eq!(
            sine(spec, 1_000.0, 100, f32::INFINITY).unwrap_err(),
            SignalError::InvalidTarget
        );
    }

    #[test]
    fn target_must_be_finite_and_strictly_below_zero_dbfs() {
        let spec = spec(1);
        // Zero and positive targets are rejected (must be strictly below 0).
        assert_eq!(
            sine(spec, 1_000.0, 480, 0.0).unwrap_err(),
            SignalError::InvalidTarget
        );
        assert_eq!(
            sine(spec, 1_000.0, 480, 5.0).unwrap_err(),
            SignalError::InvalidTarget
        );
        assert_eq!(
            sine(spec, 1_000.0, 480, f32::NEG_INFINITY).unwrap_err(),
            SignalError::InvalidTarget
        );
        // Targets at or below the raw reference (-3.0103 dBFS for a full-scale
        // sine) apply zero/negative gain and stay within full scale.
        assert!(sine(spec, 1_000.0, 480, -3.1).is_ok());
        assert!(sine(spec, 1_000.0, 480, -120.0).is_ok());
    }

    #[test]
    fn rejects_target_that_pushes_peak_past_full_scale() {
        let spec = spec(1);
        // A raw full-scale sine references ~-3.0103 dBFS. Any target above that
        // reference demands positive gain and pushes the peak past 1.0.
        for too_hot in [-3.0, -2.0, -1.0, -0.001] {
            assert_eq!(
                sine(spec, 1_000.0, 48_000, too_hot).unwrap_err(),
                SignalError::PeakExceedsFullScale,
                "target {too_hot} should clip"
            );
        }
        // A target at or below the raw reference applies zero/negative gain, so
        // the peak stays within full scale.
        assert!(sine(spec, 1_000.0, 48_000, -3.1).is_ok());
        assert!(sine(spec, 1_000.0, 48_000, -20.0).is_ok());
    }
}
