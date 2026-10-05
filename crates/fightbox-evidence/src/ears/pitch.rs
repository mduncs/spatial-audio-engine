//! Windowed fundamental tracking for the tone-only Doppler evidence gate.

use super::dsp::{EPS, db20, rms};
use super::{AnalysisError, Pcm};

/// Window, search-band, and admission settings for [`windowed_pitch_track`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchTrackConfig {
    /// Number of PCM frames in each pitch window.
    pub window_frames: usize,
    /// Number of PCM frames between adjacent window starts.
    pub hop_frames: usize,
    /// Lowest admitted fundamental in hertz.
    pub minimum_frequency_hz: f64,
    /// Highest admitted fundamental in hertz.
    pub maximum_frequency_hz: f64,
    /// Minimum normalized autocorrelation peak required for a pitch estimate.
    pub minimum_clarity: f64,
    /// Mono-sum RMS floor below which no pitch is reported.
    pub minimum_rms_dbfs: f64,
}

impl PitchTrackConfig {
    /// Construct the Wave 11 settings for the Brown Line's 1 kHz probe tone.
    ///
    /// The tracker uses 250 ms windows at 50 percent overlap and admits
    /// 700–1,300 Hz. That deliberately narrow octave-free range prevents a
    /// harmonic choice from weakening the exact Doppler comparison.
    #[must_use]
    pub fn wave11(sample_rate_hz: u32) -> Self {
        let window_frames = (sample_rate_hz as usize / 4).max(256);
        Self {
            window_frames,
            hop_frames: window_frames / 2,
            minimum_frequency_hz: 700.0,
            maximum_frequency_hz: 1_300.0,
            minimum_clarity: 0.80,
            minimum_rms_dbfs: -60.0,
        }
    }
}

/// One window of tone-pitch evidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchWindow {
    /// First PCM frame included in the window.
    pub start_frame: usize,
    /// Time at the center of the window, in seconds from capture start.
    pub center_time_s: f64,
    /// Estimated fundamental, or `None` when energy/clarity is insufficient.
    pub fundamental_hz: Option<f64>,
    /// Strongest normalized autocorrelation in the configured period range.
    pub clarity: f64,
    /// RMS of the `(L + R) / 2` signal used by the estimator.
    pub rms_dbfs: f64,
}

impl PitchWindow {
    /// Signed pitch difference from `reference_hz`, in cents.
    ///
    /// Returns `None` when this window has no admitted estimate or the
    /// reference is non-finite/non-positive.
    #[must_use]
    pub fn cents_from(self, reference_hz: f64) -> Option<f64> {
        self.fundamental_hz
            .filter(|_| reference_hz.is_finite() && reference_hz > 0.0)
            .map(|frequency_hz| 1_200.0 * (frequency_hz / reference_hz).log2())
    }
}

/// A deterministic sequence of pitch estimates in increasing time order.
#[derive(Debug, Clone, PartialEq)]
pub struct PitchTrack {
    /// Capture sample rate copied from the input PCM.
    pub sample_rate_hz: u32,
    /// Settings used to produce every window.
    pub config: PitchTrackConfig,
    /// Complete analysis windows in increasing `start_frame` order.
    pub windows: Vec<PitchWindow>,
}

/// Estimate a per-window fundamental from the stereo mono sum.
///
/// This extractor is intentionally specialized for the Brown Line's
/// monophonic 1 kHz qualification tone, not for speech, chords, or arbitrary
/// train material. It first selects a period by normalized autocorrelation,
/// then maximizes a Hann-windowed sinusoidal projection around that candidate
/// for sub-hertz resolution. A changing tone is therefore summarized near the
/// window center; motion faster than the window is a documented average rather
/// than an instantaneous-frequency claim.
pub fn windowed_pitch_track(
    pcm: Pcm<'_>,
    config: PitchTrackConfig,
) -> Result<PitchTrack, AnalysisError> {
    pcm.validate()?;
    let nyquist_hz = 0.5 * pcm.sample_rate_hz as f64;
    if config.window_frames < 256
        || config.window_frames > pcm.left.len()
        || config.hop_frames == 0
        || !config.minimum_frequency_hz.is_finite()
        || !config.maximum_frequency_hz.is_finite()
        || config.minimum_frequency_hz <= 0.0
        || config.maximum_frequency_hz <= config.minimum_frequency_hz
        || config.maximum_frequency_hz >= nyquist_hz
        || !config.minimum_clarity.is_finite()
        || !(0.0..=1.0).contains(&config.minimum_clarity)
        || !config.minimum_rms_dbfs.is_finite()
        || config.minimum_rms_dbfs > 0.0
    {
        return Err(AnalysisError::InvalidConfiguration);
    }
    let first_lag = (pcm.sample_rate_hz as f64 / config.maximum_frequency_hz)
        .floor()
        .max(2.0) as usize;
    let last_lag = (pcm.sample_rate_hz as f64 / config.minimum_frequency_hz).ceil() as usize;
    if last_lag + 1 >= config.window_frames || first_lag >= last_lag {
        return Err(AnalysisError::InvalidConfiguration);
    }

    let mono = pcm
        .left
        .iter()
        .zip(pcm.right)
        .map(|(&left, &right)| 0.5 * (left + right))
        .collect::<Vec<_>>();
    let minimum_rms = 10.0_f64.powf(config.minimum_rms_dbfs / 20.0);
    let mut windows = Vec::new();
    for start in (0..=mono.len() - config.window_frames).step_by(config.hop_frames) {
        let samples = &mono[start..start + config.window_frames];
        let window_rms = rms(samples);
        let rms_dbfs = db20(window_rms);
        let (candidate_hz, clarity) = estimate_fundamental(
            samples,
            pcm.sample_rate_hz,
            first_lag,
            last_lag,
            config.minimum_frequency_hz,
            config.maximum_frequency_hz,
        );
        let fundamental_hz = (window_rms >= minimum_rms && clarity >= config.minimum_clarity)
            .then_some(candidate_hz);
        windows.push(PitchWindow {
            start_frame: start,
            center_time_s: (start as f64 + 0.5 * config.window_frames as f64)
                / pcm.sample_rate_hz as f64,
            fundamental_hz,
            clarity,
            rms_dbfs,
        });
    }

    Ok(PitchTrack {
        sample_rate_hz: pcm.sample_rate_hz,
        config,
        windows,
    })
}

fn estimate_fundamental(
    samples: &[f32],
    sample_rate_hz: u32,
    first_lag: usize,
    last_lag: usize,
    minimum_frequency_hz: f64,
    maximum_frequency_hz: f64,
) -> (f64, f64) {
    let mean = samples.iter().map(|sample| f64::from(*sample)).sum::<f64>() / samples.len() as f64;
    let centered = samples
        .iter()
        .map(|sample| f64::from(*sample) - mean)
        .collect::<Vec<_>>();
    let correlations = (first_lag.saturating_sub(1)..=last_lag + 1)
        .map(|lag| lag_correlation(&centered, lag))
        .collect::<Vec<_>>();
    let search_offset = first_lag.saturating_sub(1);
    let (best_lag, clarity) = (first_lag..=last_lag)
        .map(|lag| (lag, correlations[lag - search_offset]))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .unwrap_or((first_lag, 0.0));
    let center = correlations[best_lag - search_offset];
    let before = correlations[best_lag - 1 - search_offset];
    let after = correlations[best_lag + 1 - search_offset];
    let curvature = before - 2.0 * center + after;
    let fractional_offset = if curvature.abs() > EPS {
        (0.5 * (before - after) / curvature).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let refined_lag = best_lag as f64 + fractional_offset;
    let coarse_hz = sample_rate_hz as f64 / refined_lag.max(1.0);

    let weighted = centered
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let hann = 0.5
                - 0.5 * (std::f64::consts::TAU * index as f64 / (centered.len() - 1) as f64).cos();
            sample * hann
        })
        .collect::<Vec<_>>();
    let bin_hz = sample_rate_hz as f64 / samples.len() as f64;
    let search_low = (coarse_hz - 2.0 * bin_hz).max(minimum_frequency_hz);
    let search_high = (coarse_hz + 2.0 * bin_hz).min(maximum_frequency_hz);
    let frequency_hz = maximize_tone_power(&weighted, sample_rate_hz, search_low, search_high);
    (frequency_hz, clarity.clamp(0.0, 1.0))
}

fn lag_correlation(centered: &[f64], lag: usize) -> f64 {
    let count = centered.len().saturating_sub(lag);
    if count == 0 {
        return 0.0;
    }
    let mut cross = 0.0;
    let mut early_energy = 0.0;
    let mut late_energy = 0.0;
    for index in 0..count {
        let early = centered[index];
        let late = centered[index + lag];
        cross += early * late;
        early_energy += early * early;
        late_energy += late * late;
    }
    cross / (early_energy * late_energy + EPS).sqrt()
}

fn maximize_tone_power(
    weighted: &[f64],
    sample_rate_hz: u32,
    mut low_hz: f64,
    mut high_hz: f64,
) -> f64 {
    const GOLDEN_RATIO_CONJUGATE: f64 = 0.618_033_988_749_894_9;
    let mut left_hz = high_hz - GOLDEN_RATIO_CONJUGATE * (high_hz - low_hz);
    let mut right_hz = low_hz + GOLDEN_RATIO_CONJUGATE * (high_hz - low_hz);
    let mut left_power = tone_power(weighted, sample_rate_hz, left_hz);
    let mut right_power = tone_power(weighted, sample_rate_hz, right_hz);
    for _ in 0..18 {
        if left_power < right_power {
            low_hz = left_hz;
            left_hz = right_hz;
            left_power = right_power;
            right_hz = low_hz + GOLDEN_RATIO_CONJUGATE * (high_hz - low_hz);
            right_power = tone_power(weighted, sample_rate_hz, right_hz);
        } else {
            high_hz = right_hz;
            right_hz = left_hz;
            right_power = left_power;
            left_hz = high_hz - GOLDEN_RATIO_CONJUGATE * (high_hz - low_hz);
            left_power = tone_power(weighted, sample_rate_hz, left_hz);
        }
    }
    0.5 * (low_hz + high_hz)
}

fn tone_power(weighted: &[f64], sample_rate_hz: u32, frequency_hz: f64) -> f64 {
    let omega = std::f64::consts::TAU * frequency_hz / sample_rate_hz as f64;
    let (step_sine, step_cosine) = omega.sin_cos();
    let mut phase_cosine = 1.0;
    let mut phase_sine = 0.0;
    let mut real = 0.0;
    let mut imaginary = 0.0;
    for &sample in weighted {
        real += sample * phase_cosine;
        imaginary -= sample * phase_sine;
        let next_cosine = phase_cosine * step_cosine - phase_sine * step_sine;
        phase_sine = phase_cosine * step_sine + phase_sine * step_cosine;
        phase_cosine = next_cosine;
    }
    real * real + imaginary * imaginary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_constructed_doppler_ramp_and_rejects_pitch_smear() {
        const SAMPLE_RATE: u32 = 48_000;
        const DURATION_S: usize = 4;
        const FRAMES: usize = SAMPLE_RATE as usize * DURATION_S;
        const START_HZ: f64 = 970.0;
        const END_HZ: f64 = 1_030.0;
        let slope_hz_per_s = (END_HZ - START_HZ) / DURATION_S as f64;
        let phi = 35.0_f64.to_radians();
        let mut phase = 0.0_f64;
        let mut smeared_phase = 0.0_f64;
        let mut point_left = Vec::with_capacity(FRAMES);
        let mut point_right = Vec::with_capacity(FRAMES);
        let mut wide_left = Vec::with_capacity(FRAMES);
        let mut wide_right = Vec::with_capacity(FRAMES);
        let mut smeared = Vec::with_capacity(FRAMES);
        for frame in 0..FRAMES {
            let time_s = (frame as f64 + 0.5) / SAMPLE_RATE as f64;
            let frequency_hz = START_HZ + slope_hz_per_s * time_s;
            phase += std::f64::consts::TAU * frequency_hz / SAMPLE_RATE as f64;
            smeared_phase += std::f64::consts::TAU * (frequency_hz + 20.0) / SAMPLE_RATE as f64;
            let center = 0.20 * phase.sin();
            let quadrature = 0.20 * phase.cos();
            point_left.push(center as f32);
            point_right.push(center as f32);
            wide_left.push((phi.cos() * center + phi.sin() * quadrature) as f32);
            wide_right.push((phi.cos() * center - phi.sin() * quadrature) as f32);
            smeared.push((0.10 * (phase.sin() + smeared_phase.sin())) as f32);
        }
        let config = PitchTrackConfig::wave11(SAMPLE_RATE);
        let point = windowed_pitch_track(
            Pcm {
                left: &point_left,
                right: &point_right,
                sample_rate_hz: SAMPLE_RATE,
            },
            config,
        )
        .unwrap();
        let wide = windowed_pitch_track(
            Pcm {
                left: &wide_left,
                right: &wide_right,
                sample_rate_hz: SAMPLE_RATE,
            },
            config,
        )
        .unwrap();
        let smeared_track = windowed_pitch_track(
            Pcm {
                left: &smeared,
                right: &smeared,
                sample_rate_hz: SAMPLE_RATE,
            },
            config,
        )
        .unwrap();

        let mut maximum_ground_truth_error_hz = 0.0_f64;
        let mut maximum_width_point_difference_hz = 0.0_f64;
        let mut previous_residual_cents: Option<f64> = None;
        let mut maximum_residual_step_cents = 0.0_f64;
        let mut smear_failure_count = 0;
        for ((point, wide), smeared) in point
            .windows
            .iter()
            .zip(&wide.windows)
            .zip(&smeared_track.windows)
        {
            let expected_hz = START_HZ + slope_hz_per_s * point.center_time_s;
            let point_hz = point.fundamental_hz.expect("point tone must track");
            let wide_hz = wide.fundamental_hz.expect("wide tone must track");
            let smeared_hz = smeared
                .fundamental_hz
                .expect("smear must remain measurable");
            maximum_ground_truth_error_hz =
                maximum_ground_truth_error_hz.max((point_hz - expected_hz).abs());
            maximum_width_point_difference_hz =
                maximum_width_point_difference_hz.max((wide_hz - point_hz).abs());
            let residual_cents = 1_200.0 * (point_hz / expected_hz).log2();
            if let Some(previous) = previous_residual_cents {
                maximum_residual_step_cents =
                    maximum_residual_step_cents.max((residual_cents - previous).abs());
            }
            previous_residual_cents = Some(residual_cents);
            if (smeared_hz - point_hz).abs() > 0.75 {
                smear_failure_count += 1;
            }
            assert!((point_hz - expected_hz).abs() <= 0.75);
            assert!((point_hz / expected_hz - 1.0).abs() <= 0.001);
        }
        eprintln!(
            "pitch ground truth: max_error_hz={maximum_ground_truth_error_hz:.6}, width_vs_point_hz={maximum_width_point_difference_hz:.6}, residual_step_cents={maximum_residual_step_cents:.6}, smear_failures={smear_failure_count}/{}",
            point.windows.len()
        );

        assert!(maximum_ground_truth_error_hz < 0.25);
        assert!(maximum_width_point_difference_hz < 0.02);
        assert!(maximum_residual_step_cents < 1.0);
        assert_eq!(smear_failure_count, point.windows.len());
    }
}
