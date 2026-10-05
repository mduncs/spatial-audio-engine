//! Slice-based perceptual extractors adapted from `ssim-ears`.

use super::dsp::{
    EPS, db20, frame_rms, goertzel, low_high_ratio_db, mean, median, mono, normalized_correlation,
    profile_distance, rms, robust_sigma, spectral_profile,
};

/// Coherence-band centers used by [`ExtractorMetrics::coherence_spectrum`].
pub const COHERENCE_BANDS_HZ: [f64; 8] = [
    125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 12_000.0,
];

/// A borrowed stereo PCM capture. Channels are deinterleaved and equally long.
#[derive(Debug, Clone, Copy)]
pub struct Pcm<'a> {
    pub left: &'a [f32],
    pub right: &'a [f32],
    pub sample_rate_hz: u32,
}

impl Pcm<'_> {
    pub(crate) fn validate(self) -> Result<(), AnalysisError> {
        if self.sample_rate_hz < 8_000 {
            return Err(AnalysisError::UnsupportedSampleRate(self.sample_rate_hz));
        }
        if self.left.len() != self.right.len() {
            return Err(AnalysisError::ChannelLengthMismatch {
                left: self.left.len(),
                right: self.right.len(),
            });
        }
        if self.left.len() < self.sample_rate_hz as usize / 2 {
            return Err(AnalysisError::CaptureTooShort {
                samples: self.left.len(),
            });
        }
        if self
            .left
            .iter()
            .chain(self.right)
            .any(|sample| !sample.is_finite())
        {
            return Err(AnalysisError::NonFiniteSample);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalysisError {
    /// The extractor requires at least an 8 kHz sample rate.
    UnsupportedSampleRate(u32),
    /// Deinterleaved stereo channels have different frame counts.
    ChannelLengthMismatch { left: usize, right: usize },
    /// The capture is shorter than the extractor's minimum evidence window.
    CaptureTooShort { samples: usize },
    /// PCM or derived input contains a non-finite sample.
    NonFiniteSample,
    /// A geometry track is not aligned one-for-one with the PCM frames.
    TrackLengthMismatch {
        frames: usize,
        distances: usize,
        angular_subtenses: Option<usize>,
    },
    /// A geometry track contains a non-finite or out-of-domain value.
    InvalidTrack,
    /// Window, hop, frequency, lag, clarity, or energy settings are invalid.
    InvalidConfiguration,
}

/// Scalar evidence used by Gate 0 and later capture comparisons.
#[derive(Debug, Clone, Copy, Default)]
pub struct ExtractorMetrics {
    /// Strongest normalized mono autocorrelation in the 12–80 ms echo range.
    pub slapback_correlation: f64,
    /// Strongest normalized mono autocorrelation in the 0.5–8 ms comb range.
    pub comb_correlation: f64,
    /// Robust maximum adjacent 20 ms envelope step, in dB.
    pub zipper_step_db: f64,
    /// Median interaural delay. Positive means the right channel leads.
    pub itd_ms: f64,
    /// Broadband `L/R` level difference.
    pub ild_db: f64,
    /// Fraction of frames whose ITD and ILD signs violate the expected opposing cue signs.
    pub cue_conflict_fraction: f64,
    /// Peak absolute interaural cross-correlation over ±1 ms.
    pub iacc: f64,
    /// Welch-style magnitude-squared interaural coherence at [`COHERENCE_BANDS_HZ`].
    pub coherence_spectrum: [f64; 8],
    /// `1 - IACC`, the donor width convention.
    pub width: f64,
    /// Overall stereo RMS level.
    pub loudness_dbfs: f64,
    /// Low-band to high-band energy ratio.
    pub spectral_tilt_db: f64,
    /// Maximum positive spectral-profile change between adjacent 40 ms frames.
    pub spectral_flux: f64,
    /// Largest sample derivative in robust-sigma units.
    pub click_derivative_z: f64,
    /// Largest inter-frame pitch step from normalized autocorrelation.
    pub pitch_step_cents: f64,
    /// RMS-envelope energy at 0.5–8 Hz relative to the envelope mean.
    pub pump_modulation: f64,
    /// Largest adjacent 250 ms spectral-character change.
    pub enclosure_step: f64,
    /// Fraction of the capture occupied by terminal digital silence.
    pub trailing_silence_fraction: f64,
    /// Abel–Huang normalized echo density in the second half of the capture.
    pub reflection_density: f64,
}

/// Run the Gate 0 extractor set over deinterleaved f32 PCM.
pub fn analyze(pcm: Pcm<'_>) -> Result<ExtractorMetrics, AnalysisError> {
    pcm.validate()?;
    let sample_rate = pcm.sample_rate_hz as usize;
    let mono = mono(pcm.left, pcm.right);
    let envelope_frame = (sample_rate / 50).max(16);
    let envelope = frame_rms(&mono, envelope_frame, envelope_frame);
    let loudness = (0.5 * (rms(pcm.left).powi(2) + rms(pcm.right).powi(2))).sqrt();

    let (itd_ms, ild_db, cue_conflict_fraction) = itd_ild_trajectory(pcm);
    let iacc = iacc(pcm);

    Ok(ExtractorMetrics {
        slapback_correlation: peak_autocorrelation(
            &mono,
            (0.012 * sample_rate as f64) as usize,
            (0.080 * sample_rate as f64) as usize,
            8,
        ),
        comb_correlation: peak_autocorrelation(
            &mono,
            (0.0005 * sample_rate as f64) as usize,
            (0.008 * sample_rate as f64) as usize,
            2,
        ),
        zipper_step_db: envelope_step_db(&envelope),
        itd_ms,
        ild_db,
        cue_conflict_fraction,
        iacc,
        coherence_spectrum: coherence_spectrum(pcm),
        width: 1.0 - iacc,
        loudness_dbfs: db20(loudness),
        spectral_tilt_db: low_high_ratio_db(&mono, pcm.sample_rate_hz),
        spectral_flux: spectral_flux(&mono, pcm.sample_rate_hz),
        click_derivative_z: click_derivative_z(&mono),
        pitch_step_cents: pitch_step_cents(&mono, pcm.sample_rate_hz),
        pump_modulation: pump_modulation(&envelope, 50.0),
        enclosure_step: enclosure_step(&mono, pcm.sample_rate_hz),
        trailing_silence_fraction: trailing_silence_fraction(&mono),
        reflection_density: normalized_echo_density(&mono, sample_rate),
    })
}

fn peak_autocorrelation(signal: &[f32], first_lag: usize, last_lag: usize, step: usize) -> f64 {
    (first_lag..=last_lag)
        .step_by(step)
        .map(|lag| normalized_correlation(signal, signal, lag as isize, 8).abs())
        .fold(0.0, f64::max)
}

fn envelope_step_db(envelope: &[f64]) -> f64 {
    if envelope.len() < 2 {
        return 0.0;
    }
    let levels: Vec<f64> = envelope.iter().map(|value| db20(*value)).collect();
    let changes: Vec<f64> = levels
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .collect();
    let center = median(&changes);
    let sigma = robust_sigma(&changes);
    changes
        .iter()
        .map(|change| (change - center - 2.0 * sigma).max(0.0))
        .fold(0.0, f64::max)
}

fn itd_ild_trajectory(pcm: Pcm<'_>) -> (f64, f64, f64) {
    let frame = (pcm.sample_rate_hz as usize / 25).max(128);
    let hop = frame / 2;
    let max_lag = (pcm.sample_rate_hz as usize / 1_000).max(1);
    let mut delays = Vec::new();
    let mut ilds = Vec::new();
    let mut conflicts = 0;
    let mut frames = 0;
    for start in (0..=pcm.left.len() - frame).step_by(hop) {
        let left = &pcm.left[start..start + frame];
        let right = &pcm.right[start..start + frame];
        let mut best_lag = 0;
        let mut best = f64::NEG_INFINITY;
        for lag in -(max_lag as isize)..=max_lag as isize {
            let value = normalized_correlation(left, right, lag, 1);
            if value > best {
                best = value;
                best_lag = lag;
            }
        }
        let itd = -1_000.0 * best_lag as f64 / pcm.sample_rate_hz as f64;
        let ild = db20(rms(left) / (rms(right) + EPS));
        delays.push(itd);
        ilds.push(ild);
        if itd.abs() > 0.05 && ild.abs() > 0.5 && itd.signum() == ild.signum() {
            conflicts += 1;
        }
        frames += 1;
    }
    (
        median(&delays),
        median(&ilds),
        conflicts as f64 / frames.max(1) as f64,
    )
}

fn iacc(pcm: Pcm<'_>) -> f64 {
    let max_lag = (pcm.sample_rate_hz as usize / 1_000).max(1);
    (-(max_lag as isize)..=max_lag as isize)
        .map(|lag| normalized_correlation(pcm.left, pcm.right, lag, 4).abs())
        .fold(0.0, f64::max)
}

fn coherence_spectrum(pcm: Pcm<'_>) -> [f64; 8] {
    let frame = 2_048.min(pcm.left.len());
    let hop = frame / 2;
    let mut cross_real = [0.0_f64; 8];
    let mut cross_imaginary = [0.0_f64; 8];
    let mut left_power = [0.0_f64; 8];
    let mut right_power = [0.0_f64; 8];
    for start in (0..=pcm.left.len() - frame).step_by(hop) {
        let left = &pcm.left[start..start + frame];
        let right = &pcm.right[start..start + frame];
        for (band, &frequency) in COHERENCE_BANDS_HZ.iter().enumerate() {
            let (left_real, left_imaginary) = goertzel(left, pcm.sample_rate_hz, frequency);
            let (right_real, right_imaginary) = goertzel(right, pcm.sample_rate_hz, frequency);
            cross_real[band] += left_real * right_real + left_imaginary * right_imaginary;
            cross_imaginary[band] += left_imaginary * right_real - left_real * right_imaginary;
            left_power[band] += left_real * left_real + left_imaginary * left_imaginary;
            right_power[band] += right_real * right_real + right_imaginary * right_imaginary;
        }
    }
    let mut coherence = [0.0; 8];
    for band in 0..coherence.len() {
        coherence[band] = ((cross_real[band].powi(2) + cross_imaginary[band].powi(2))
            / (left_power[band] * right_power[band] + EPS))
            .clamp(0.0, 1.0);
    }
    coherence
}

fn spectral_flux(signal: &[f32], sample_rate_hz: u32) -> f64 {
    let frame = (sample_rate_hz as usize * 40 / 1_000).max(64);
    let hop = frame / 2;
    if signal.len() < frame {
        return 0.0;
    }
    let profiles: Vec<[f64; 8]> = (0..=signal.len() - frame)
        .step_by(hop)
        .map(|start| spectral_profile(&signal[start..start + frame], sample_rate_hz))
        .collect();
    let changes: Vec<f64> = profiles
        .windows(2)
        .map(|pair| profile_distance(&pair[0], &pair[1]))
        .collect();
    let center = median(&changes);
    let sigma = robust_sigma(&changes);
    changes
        .iter()
        .map(|change| (change - center - 3.0 * sigma).max(0.0))
        .fold(0.0, f64::max)
}

fn click_derivative_z(signal: &[f32]) -> f64 {
    let derivatives: Vec<f64> = signal
        .windows(2)
        .map(|pair| pair[1] as f64 - pair[0] as f64)
        .collect();
    let center = median(&derivatives);
    let sigma = robust_sigma(&derivatives);
    if sigma <= 1.0e-12 {
        return 0.0;
    }
    derivatives
        .iter()
        .map(|value| (value - center).abs() / sigma)
        .fold(0.0, f64::max)
}

fn pitch_step_cents(signal: &[f32], sample_rate_hz: u32) -> f64 {
    let frame = (sample_rate_hz as usize * 40 / 1_000).max(256);
    let hop = frame / 2;
    let min_lag = (sample_rate_hz as f64 / 700.0) as usize;
    let max_lag = (sample_rate_hz as f64 / 250.0) as usize;
    let mut pitches = Vec::new();
    for start in (0..=signal.len() - frame).step_by(hop) {
        let window = &signal[start..start + frame];
        let mut best_lag = min_lag;
        let mut best = f64::NEG_INFINITY;
        for lag in min_lag..=max_lag {
            let value = normalized_correlation(window, window, lag as isize, 1);
            if value > best {
                best = value;
                best_lag = lag;
            }
        }
        if best > 0.75 {
            pitches.push(sample_rate_hz as f64 / best_lag as f64);
        }
    }
    pitches
        .windows(2)
        .map(|pair| 1_200.0 * (pair[1] / pair[0]).log2().abs())
        .fold(0.0, f64::max)
}

fn pump_modulation(envelope: &[f64], envelope_rate_hz: f64) -> f64 {
    if envelope.is_empty() {
        return 0.0;
    }
    let center = mean(envelope);
    let mut energy = 0.0_f64;
    for frequency_tenths in 5..=80 {
        let frequency = frequency_tenths as f64 / 10.0;
        let mut cosine = 0.0;
        let mut sine = 0.0;
        for (index, &value) in envelope.iter().enumerate() {
            let phase = std::f64::consts::TAU * frequency * index as f64 / envelope_rate_hz;
            let value = value - center;
            cosine += value * phase.cos();
            sine += value * phase.sin();
        }
        energy = energy.max((cosine * cosine + sine * sine).sqrt());
    }
    2.0 * energy / (envelope.len() as f64 * center.abs().max(1.0e-12))
}

fn enclosure_step(signal: &[f32], sample_rate_hz: u32) -> f64 {
    let frame = (sample_rate_hz as usize / 4).max(64);
    if signal.len() < frame * 2 {
        return 0.0;
    }
    let lag_ms = [2, 5, 9, 17];
    let profiles: Vec<[f64; 4]> = (0..=signal.len() - frame)
        .step_by(frame)
        .map(|start| {
            let window = &signal[start..start + frame];
            let mut profile = [0.0; 4];
            for (value, milliseconds) in profile.iter_mut().zip(lag_ms) {
                let lag = sample_rate_hz as usize * milliseconds / 1_000;
                *value = normalized_correlation(window, window, lag as isize, 1);
            }
            profile
        })
        .collect();
    let changes: Vec<f64> = profiles
        .windows(2)
        .map(|pair| {
            pair[0]
                .iter()
                .zip(pair[1])
                .map(|(left, right)| (left - right).abs())
                .sum()
        })
        .collect();
    let center = median(&changes);
    let sigma = robust_sigma(&changes);
    changes
        .iter()
        .map(|change| (change - center - 3.0 * sigma).max(0.0))
        .fold(0.0, f64::max)
}

fn trailing_silence_fraction(signal: &[f32]) -> f64 {
    let silent = signal
        .iter()
        .rev()
        .take_while(|sample| sample.abs() <= 1.0e-8)
        .count();
    silent as f64 / signal.len().max(1) as f64
}

fn normalized_echo_density(signal: &[f32], sample_rate: usize) -> f64 {
    const GAUSSIAN_EXCEEDANCE: f64 = 0.317_310_507_862_914_15;
    let frame = (sample_rate * 20 / 1_000).max(64);
    let hop = frame / 2;
    let mut densities = Vec::new();
    for start in (signal.len() / 2..=signal.len() - frame).step_by(hop) {
        let window = &signal[start..start + frame];
        let center = window.iter().map(|&v| v as f64).sum::<f64>() / frame as f64;
        let sigma = (window
            .iter()
            .map(|&value| (value as f64 - center).powi(2))
            .sum::<f64>()
            / frame as f64)
            .sqrt();
        if sigma > 1.0e-15 {
            let exceedance = window
                .iter()
                .filter(|&&value| (value as f64 - center).abs() > sigma)
                .count() as f64
                / frame as f64;
            densities.push(exceedance / GAUSSIAN_EXCEEDANCE);
        }
    }
    mean(&densities)
}

/// Backward-integrated energy-decay result, adapted from the donor's Schroeder extractor.
#[derive(Debug, Clone)]
pub struct SchroederDecay {
    pub energy_decay_db: Vec<f64>,
    pub slope_db_per_s: Option<f64>,
}

/// Compute a broadband Schroeder energy-decay curve and its −5 to −35 dB slope.
pub fn schroeder_decay(signal: &[f32], sample_rate_hz: u32) -> SchroederDecay {
    let mut backward = vec![0.0; signal.len()];
    let mut energy = 0.0;
    for index in (0..signal.len()).rev() {
        let sample = signal[index] as f64;
        energy += sample * sample;
        backward[index] = energy;
    }
    let total = backward.first().copied().unwrap_or(0.0).max(EPS);
    let energy_decay_db: Vec<f64> = backward
        .iter()
        .map(|energy| 10.0 * (energy / total).max(EPS).log10())
        .collect();
    let points: Vec<(f64, f64)> = energy_decay_db
        .iter()
        .enumerate()
        .filter(|(_, db)| **db <= -5.0 && **db >= -35.0)
        .map(|(index, &db)| (index as f64 / sample_rate_hz as f64, db))
        .collect();
    let slope_db_per_s = if points.len() < 32 {
        None
    } else {
        let mean_time = points.iter().map(|point| point.0).sum::<f64>() / points.len() as f64;
        let mean_db = points.iter().map(|point| point.1).sum::<f64>() / points.len() as f64;
        let numerator = points
            .iter()
            .map(|point| (point.0 - mean_time) * (point.1 - mean_db))
            .sum::<f64>();
        let denominator = points
            .iter()
            .map(|point| (point.0 - mean_time).powi(2))
            .sum::<f64>();
        (denominator > EPS).then_some(numerator / denominator)
    };
    SchroederDecay {
        energy_decay_db,
        slope_db_per_s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_channels() {
        let error = analyze(Pcm {
            left: &vec![0.0; 24_000],
            right: &vec![0.0; 23_999],
            sample_rate_hz: 48_000,
        })
        .unwrap_err();
        assert!(matches!(error, AnalysisError::ChannelLengthMismatch { .. }));
    }

    #[test]
    fn schroeder_decay_uses_f64_backward_energy() {
        let signal: Vec<f32> = (0..48_000)
            .map(|index| 10.0_f64.powf(-60.0 * index as f64 / 48_000.0 / 20.0) as f32)
            .collect();
        let decay = schroeder_decay(&signal, 48_000);
        assert!(
            decay
                .energy_decay_db
                .windows(2)
                .all(|pair| pair[0] >= pair[1])
        );
        assert!(decay.slope_db_per_s.unwrap() < -50.0);
    }
}
