//! Honest, bounded metrics for evidence captures.
//!
//! These operate on raw interleaved `f32` buffers. They report what is measured
//! and never derive a gate pass from configuration. The spectral comparison and
//! continuity checks are deliberately simple (a naive bounded DFT and a
//! first-difference check); they are suitable for small evidence windows, not a
//! substitute for the perceptual suite that starts counting after Gate 0.
//!
//! Authority note §ν is explicit that smoothness is asserted on the **summed
//! output**, never per path: the caller is responsible for passing the sum to
//! [`continuity`].

use crate::json::JsonObject;
use crate::wav::{WavSpec, validate_spec};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetricError {
    BadSpec,
    FrameChannelMismatch,
    EmptyBins,
    StereoRequired,
    InvalidWindow,
    InvalidThreshold,
}

/// Per-channel health and level summary for a finite interleaved buffer.
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelMetrics {
    pub frame_count: usize,
    pub channels: u16,
    pub sample_rate_hz: u32,
    /// `true` only if every sample is finite.
    pub all_finite: bool,
    pub peak_per_channel: Vec<f32>,
    pub rms_per_channel: Vec<f32>,
    /// 20·log10(rms) per channel, or `None` for a silent channel (avoids
    /// emitting invalid JSON `-Infinity`).
    pub rms_dbfs_per_channel: Vec<Option<f32>>,
    /// Number of channels whose RMS is exactly zero (degenerate/silent).
    pub silent_channel_count: usize,
    /// Stereo side-channel RMS = RMS of `(L - R)` per frame; `None` when mono.
    pub stereo_difference_rms: Option<f32>,
}

impl ChannelMetrics {
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut peaks = String::from("[");
        let mut rms = String::from("[");
        let mut dbfs = String::from("[");
        for (i, ch) in self.peak_per_channel.iter().enumerate() {
            if i > 0 {
                peaks.push(',');
                rms.push(',');
                dbfs.push(',');
            }
            peaks.push_str(&crate::json::json_f32(*ch));
            rms.push_str(&crate::json::json_f32(self.rms_per_channel[i]));
            dbfs.push_str(&crate::json::json_opt_f32(self.rms_dbfs_per_channel[i]));
        }
        peaks.push(']');
        rms.push(']');
        dbfs.push(']');

        let mut o = JsonObject::new();
        o.num_usize("frame_count", self.frame_count);
        o.num_u32("channels", u32::from(self.channels));
        o.num_u32("sample_rate_hz", self.sample_rate_hz);
        o.boolean("all_finite", self.all_finite);
        o.raw_value("peak_per_channel", &peaks);
        o.raw_value("rms_per_channel", &rms);
        o.raw_value("rms_dbfs_per_channel", &dbfs);
        o.num_usize("silent_channel_count", self.silent_channel_count);
        o.opt_f32("stereo_difference_rms", self.stereo_difference_rms);
        o.finish()
    }
}

/// Compute channel metrics from an interleaved buffer.
pub fn channel_metrics(spec: WavSpec, samples: &[f32]) -> Result<ChannelMetrics, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    let channels = usize::from(spec.channels);
    if channels != 0 && samples.len() % channels != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    let frame_count = if channels == 0 {
        0
    } else {
        samples.len() / channels
    };

    let mut peak = vec![0.0_f32; channels];
    let mut sum_sq = vec![0.0_f64; channels];
    let mut all_finite = true;
    let mut stereo_diff_sq_sum = 0.0_f64;

    for frame in 0..frame_count {
        for c in 0..channels {
            let s = samples[frame * channels + c];
            if !s.is_finite() {
                all_finite = false;
            } else {
                let abs = s.abs();
                if abs > peak[c] {
                    peak[c] = abs;
                }
                sum_sq[c] += (s as f64) * (s as f64);
            }
        }
        if channels == 2 {
            let l = samples[frame * 2];
            let r = samples[frame * 2 + 1];
            if l.is_finite() && r.is_finite() {
                let d = l - r;
                stereo_diff_sq_sum += (d as f64) * (d as f64);
            }
        }
    }

    let rms_per_channel: Vec<f32> = (0..channels)
        .map(|c| ((sum_sq[c] / frame_count.max(1) as f64).sqrt()) as f32)
        .collect();
    let rms_dbfs_per_channel: Vec<Option<f32>> = rms_per_channel
        .iter()
        .map(|&r| {
            if r > 0.0 {
                Some(20.0 * r.log10())
            } else {
                None
            }
        })
        .collect();
    let silent_channel_count = rms_per_channel.iter().filter(|&&r| r == 0.0).count();
    let stereo_difference_rms =
        (channels == 2).then(|| ((stereo_diff_sq_sum / frame_count.max(1) as f64).sqrt()) as f32);

    Ok(ChannelMetrics {
        frame_count,
        channels: spec.channels,
        sample_rate_hz: spec.sample_rate_hz,
        all_finite,
        peak_per_channel: peak,
        rms_per_channel,
        rms_dbfs_per_channel,
        silent_channel_count,
        stereo_difference_rms,
    })
}

/// Deterministic level + spectral comparison of pathing-on vs pathing-off sums.
///
/// Spectra are naive bounded DFT magnitudes at `bins_hz`, computed on the mono
/// mixdown and normalized by sample count. `differs` flags a difference above the
/// documented defaults (0.5 dB level, `1e-3` normalized spectral L1), or an
/// energetic/silent mismatch. Two silent captures never differ. Callers may
/// re-threshold from the raw fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComparisonEnergy {
    BothEnergetic,
    OnOnly,
    OffOnly,
    BothSilent,
}

impl ComparisonEnergy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BothEnergetic => "both_energetic",
            Self::OnOnly => "on_only",
            Self::OffOnly => "off_only",
            Self::BothSilent => "both_silent",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpectralComparison {
    pub bins_hz: Vec<f32>,
    pub on_rms_dbfs: Option<f32>,
    pub off_rms_dbfs: Option<f32>,
    /// `on - off` in dB when both captures are energetic. `None` when either
    /// side is silent because no finite dB ratio exists in that case.
    pub level_difference_db: Option<f32>,
    /// Explicit energy state for interpreting an absent level difference.
    pub energy: ComparisonEnergy,
    pub spectral_l1_difference: f32,
    pub spectral_l2_difference: f32,
    pub differs: bool,
}

impl SpectralComparison {
    #[must_use]
    pub fn to_json(&self) -> String {
        // `bins_hz` are finite JSON numbers (the comparison rejects non-finite
        // bins upstream), never quoted strings, so consumers can plot them.
        let mut bins = String::from("[");
        for (i, &bin) in self.bins_hz.iter().enumerate() {
            if i > 0 {
                bins.push(',');
            }
            bins.push_str(&crate::json::json_f32(bin));
        }
        bins.push(']');
        let mut o = JsonObject::new();
        o.raw_value("bins_hz", &bins);
        o.opt_f32("on_rms_dbfs", self.on_rms_dbfs);
        o.opt_f32("off_rms_dbfs", self.off_rms_dbfs);
        o.opt_f32("level_difference_db", self.level_difference_db);
        o.str("energy", self.energy.as_str());
        o.num_f32("spectral_l1_difference", self.spectral_l1_difference);
        o.num_f32("spectral_l2_difference", self.spectral_l2_difference);
        o.boolean("differs", self.differs);
        o.finish()
    }
}

/// Documented default thresholds for [`compare_pathing`]'s `differs` flag.
pub const DEFAULT_LEVEL_DIFFERENCE_DB: f32 = 0.5;
pub const DEFAULT_SPECTRAL_L1_DIFFERENCE: f32 = 1e-3;

/// Compare two summed captures (pathing on vs off) by level and bounded spectrum.
pub fn compare_pathing(
    spec: WavSpec,
    on: &[f32],
    off: &[f32],
    bins_hz: &[f32],
) -> Result<SpectralComparison, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if bins_hz.is_empty() {
        return Err(MetricError::EmptyBins);
    }
    let on_dbfs = mono_rms_dbfs(spec, on)?;
    let off_dbfs = mono_rms_dbfs(spec, off)?;
    let (level_difference_db, energy) = match (on_dbfs, off_dbfs) {
        (Some(a), Some(b)) => (Some(a - b), ComparisonEnergy::BothEnergetic),
        (Some(_), None) => (None, ComparisonEnergy::OnOnly),
        (None, Some(_)) => (None, ComparisonEnergy::OffOnly),
        (None, None) => (None, ComparisonEnergy::BothSilent),
    };

    let on_mix = mono_mixdown(spec, on)?;
    let off_mix = mono_mixdown(spec, off)?;
    let mut l1 = 0.0_f64;
    let mut l2 = 0.0_f64;
    for &bin in bins_hz {
        let m_on = f64::from(dft_magnitude(spec.sample_rate_hz, &on_mix, bin));
        let m_off = f64::from(dft_magnitude(spec.sample_rate_hz, &off_mix, bin));
        let diff = m_on - m_off;
        l1 += diff.abs();
        l2 += diff * diff;
    }

    let spectral_l1_difference = l1 as f32;
    let spectral_l2_difference = (l2.sqrt()) as f32;
    let differs = match energy {
        ComparisonEnergy::BothSilent => false,
        ComparisonEnergy::OnOnly | ComparisonEnergy::OffOnly => true,
        ComparisonEnergy::BothEnergetic => {
            level_difference_db
                .is_some_and(|difference| difference.abs() > DEFAULT_LEVEL_DIFFERENCE_DB)
                || spectral_l1_difference > DEFAULT_SPECTRAL_L1_DIFFERENCE
        }
    };

    Ok(SpectralComparison {
        bins_hz: bins_hz.to_vec(),
        on_rms_dbfs: on_dbfs,
        off_rms_dbfs: off_dbfs,
        level_difference_db,
        energy,
        spectral_l1_difference,
        spectral_l2_difference,
        differs,
    })
}

/// Adjacent-frame discontinuity / handoff check for a summed capture.
///
/// Reports the largest single-sample step `|x[n] - x[n-1]|`, the buffer peak,
/// and the step-to-peak ratio against `click_ratio_threshold`. Pass the summed
/// output, never a single path (authority note §ν).
#[derive(Clone, Debug, PartialEq)]
pub struct ContinuityReport {
    pub max_adjacent_delta: f32,
    pub peak: f32,
    pub max_delta_to_peak_ratio: f32,
    pub click_ratio_threshold: f32,
    pub click_budget_exceeded: bool,
}

impl ContinuityReport {
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut o = JsonObject::new();
        o.num_f32("max_adjacent_delta", self.max_adjacent_delta);
        o.num_f32("peak", self.peak);
        o.num_f32("max_delta_to_peak_ratio", self.max_delta_to_peak_ratio);
        o.num_f32("click_ratio_threshold", self.click_ratio_threshold);
        o.boolean("click_budget_exceeded", self.click_budget_exceeded);
        o.finish()
    }
}

/// A conservative default click ratio: a single-sample step larger than half the
/// buffer peak is treated as a discontinuity.
pub const DEFAULT_CLICK_RATIO_THRESHOLD: f32 = 0.5;

/// Compute the continuity report on the first channel of `summed`.
pub fn continuity(
    spec: WavSpec,
    summed: &[f32],
    click_ratio_threshold: f32,
) -> Result<ContinuityReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    let channels = usize::from(spec.channels);
    if channels != 0 && summed.len() % channels != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    let mono = mono_mixdown(spec, summed)?;

    let mut peak = 0.0_f32;
    for &s in &mono {
        let abs = s.abs();
        if abs > peak {
            peak = abs;
        }
    }
    let mut max_delta = 0.0_f32;
    for window in mono.windows(2) {
        let d = (window[1] - window[0]).abs();
        if d > max_delta {
            max_delta = d;
        }
    }
    let ratio = if peak > 0.0 { max_delta / peak } else { 0.0 };
    Ok(ContinuityReport {
        max_adjacent_delta: max_delta,
        peak,
        max_delta_to_peak_ratio: ratio,
        click_ratio_threshold,
        click_budget_exceeded: ratio > click_ratio_threshold,
    })
}

/// Maximum absolute normalized interaural cross-correlation in a bounded
/// stereo window.
///
/// `max_lag_samples` states the lag search explicitly. For perceptual IACC,
/// callers normally use ±1 ms (48 samples at 48 kHz). The means are removed
/// independently at every lag before normalization, and silent lag overlaps
/// contribute zero rather than a non-finite value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct IaccReport {
    pub window_start_frame: usize,
    pub window_frames: usize,
    pub max_lag_samples: usize,
    pub coefficient: f32,
    /// Signed lag at the maximum: positive values correlate `L[n]` with
    /// `R[n + lag]`.
    pub lag_samples: isize,
}

pub fn interaural_cross_correlation(
    spec: WavSpec,
    stereo: &[f32],
    window_start_frame: usize,
    window_frames: usize,
    max_lag_samples: usize,
) -> Result<IaccReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if spec.channels != 2 {
        return Err(MetricError::StereoRequired);
    }
    if stereo.len() % 2 != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    let total_frames = stereo.len() / 2;
    let window_end = window_start_frame
        .checked_add(window_frames)
        .ok_or(MetricError::InvalidWindow)?;
    if window_frames < 2 || window_end > total_frames || max_lag_samples >= window_frames {
        return Err(MetricError::InvalidWindow);
    }

    let left = (window_start_frame..window_end)
        .map(|frame| stereo[frame * 2])
        .collect::<Vec<_>>();
    let right = (window_start_frame..window_end)
        .map(|frame| stereo[frame * 2 + 1])
        .collect::<Vec<_>>();
    let mut maximum = 0.0_f64;
    let mut best_lag = 0_isize;
    for lag in -(max_lag_samples as isize)..=(max_lag_samples as isize) {
        let (left_start, right_start, count) = if lag >= 0 {
            (0, lag as usize, window_frames - lag as usize)
        } else {
            ((-lag) as usize, 0, window_frames - (-lag) as usize)
        };
        let left_slice = &left[left_start..left_start + count];
        let right_slice = &right[right_start..right_start + count];
        let left_mean = left_slice
            .iter()
            .map(|sample| f64::from(*sample))
            .sum::<f64>()
            / count as f64;
        let right_mean = right_slice
            .iter()
            .map(|sample| f64::from(*sample))
            .sum::<f64>()
            / count as f64;
        let mut cross = 0.0_f64;
        let mut left_energy = 0.0_f64;
        let mut right_energy = 0.0_f64;
        for (&left, &right) in left_slice.iter().zip(right_slice) {
            let left = f64::from(left) - left_mean;
            let right = f64::from(right) - right_mean;
            cross += left * right;
            left_energy += left * left;
            right_energy += right * right;
        }
        let denominator = (left_energy * right_energy).sqrt();
        let coefficient = if denominator > 0.0 {
            (cross / denominator).abs()
        } else {
            0.0
        };
        if coefficient > maximum {
            maximum = coefficient;
            best_lag = lag;
        }
    }

    Ok(IaccReport {
        window_start_frame,
        window_frames,
        max_lag_samples,
        coefficient: maximum as f32,
        lag_samples: best_lag,
    })
}

/// Arrival count and rate in a bounded indirect-response window.
///
/// The extractor converts the interleaved response to a per-frame RMS
/// magnitude, smooths it with a 0.5 ms centered box window, then counts local
/// maxima above `relative_peak_threshold * window_peak`, enforcing the stated
/// minimum separation. The caller must pass an indirect stem (normally the
/// reflections stem), not a direct+indirect sum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReflectionDensityReport {
    pub window_start_frame: usize,
    pub window_frames: usize,
    pub relative_peak_threshold: f32,
    pub minimum_separation_samples: usize,
    pub arrival_count: usize,
    pub arrivals_per_second: f32,
}

pub fn reflection_density(
    spec: WavSpec,
    indirect: &[f32],
    window_start_frame: usize,
    window_frames: usize,
    relative_peak_threshold: f32,
    minimum_separation_samples: usize,
) -> Result<ReflectionDensityReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    let channels = usize::from(spec.channels);
    if indirect.len() % channels != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    if !relative_peak_threshold.is_finite()
        || !(0.0..=1.0).contains(&relative_peak_threshold)
        || minimum_separation_samples == 0
    {
        return Err(MetricError::InvalidThreshold);
    }
    let total_frames = indirect.len() / channels;
    let window_end = window_start_frame
        .checked_add(window_frames)
        .ok_or(MetricError::InvalidWindow)?;
    if window_frames < 3 || window_end > total_frames {
        return Err(MetricError::InvalidWindow);
    }

    let magnitude = (0..total_frames)
        .map(|frame| {
            let energy = (0..channels)
                .map(|channel| {
                    let sample = f64::from(indirect[frame * channels + channel]);
                    sample * sample
                })
                .sum::<f64>();
            (energy / channels as f64).sqrt() as f32
        })
        .collect::<Vec<_>>();
    let smoothing_radius = (spec.sample_rate_hz as usize / 4_000).max(1);
    let mut prefix = Vec::with_capacity(magnitude.len() + 1);
    prefix.push(0.0_f64);
    for sample in magnitude {
        prefix.push(prefix.last().copied().unwrap_or(0.0) + f64::from(sample));
    }
    let smoothed = (0..total_frames)
        .map(|frame| {
            let start = frame.saturating_sub(smoothing_radius);
            let end = (frame + smoothing_radius + 1).min(total_frames);
            ((prefix[end] - prefix[start]) / (end - start) as f64) as f32
        })
        .collect::<Vec<_>>();
    let peak = smoothed[window_start_frame..window_end]
        .iter()
        .copied()
        .fold(0.0_f32, f32::max);
    let threshold = peak * relative_peak_threshold;
    let mut arrivals = Vec::<(usize, f32)>::new();
    for frame in window_start_frame + 1..window_end - 1 {
        let value = smoothed[frame];
        if value < threshold || value < smoothed[frame - 1] || value <= smoothed[frame + 1] {
            continue;
        }
        if let Some((previous_frame, previous_value)) = arrivals.last_mut()
            && frame - *previous_frame < minimum_separation_samples
        {
            if value > *previous_value {
                *previous_frame = frame;
                *previous_value = value;
            }
        } else {
            arrivals.push((frame, value));
        }
    }

    let arrival_count = arrivals.len();
    Ok(ReflectionDensityReport {
        window_start_frame,
        window_frames,
        relative_peak_threshold,
        minimum_separation_samples,
        arrival_count,
        arrivals_per_second: arrival_count as f32 * spec.sample_rate_hz as f32
            / window_frames as f32,
    })
}

/// Summed-binaural continuity across fixed render blocks.
///
/// Block level is stereo RMS with a -120 dBFS measurement floor. A detected
/// click is a boundary sample step larger than `click_ratio_threshold` times
/// the local stereo peak in `click_window_frames` on both sides. This targets
/// state-update discontinuities without misclassifying ordinary within-block
/// waveform slope.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SummedOutputContinuity {
    pub block_frames: usize,
    pub max_inter_block_level_step_db: f32,
    pub click_ratio_threshold: f32,
    pub max_boundary_step_to_peak_ratio: f32,
    pub detected_click_count: usize,
}

pub fn summed_output_continuity(
    spec: WavSpec,
    summed_stereo: &[f32],
    block_frames: usize,
    click_window_frames: usize,
    click_ratio_threshold: f32,
) -> Result<SummedOutputContinuity, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if spec.channels != 2 {
        return Err(MetricError::StereoRequired);
    }
    if summed_stereo.len() % 2 != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    let total_frames = summed_stereo.len() / 2;
    if block_frames == 0
        || click_window_frames == 0
        || total_frames < block_frames
        || total_frames % block_frames != 0
    {
        return Err(MetricError::InvalidWindow);
    }
    if !click_ratio_threshold.is_finite() || click_ratio_threshold <= 0.0 {
        return Err(MetricError::InvalidThreshold);
    }

    let level_db = summed_stereo
        .chunks_exact(block_frames * 2)
        .map(|block| {
            let mean_square = block
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>()
                / block.len() as f64;
            (20.0 * (mean_square.sqrt().max(1.0e-6)).log10()) as f32
        })
        .collect::<Vec<_>>();
    let max_level_step = level_db
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .fold(0.0_f32, f32::max);

    let mut maximum_ratio = 0.0_f32;
    let mut click_count = 0;
    for boundary in (block_frames..total_frames).step_by(block_frames) {
        let start = boundary.saturating_sub(click_window_frames);
        let end = (boundary + click_window_frames).min(total_frames);
        let local_peak = summed_stereo[start * 2..end * 2]
            .iter()
            .copied()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        let mut boundary_step = 0.0_f32;
        for channel in 0..2 {
            boundary_step = boundary_step.max(
                (summed_stereo[boundary * 2 + channel]
                    - summed_stereo[(boundary - 1) * 2 + channel])
                    .abs(),
            );
        }
        let ratio = if local_peak > 0.0 {
            boundary_step / local_peak
        } else {
            0.0
        };
        maximum_ratio = maximum_ratio.max(ratio);
        if ratio > click_ratio_threshold {
            click_count += 1;
        }
    }

    Ok(SummedOutputContinuity {
        block_frames,
        max_inter_block_level_step_db: max_level_step,
        click_ratio_threshold,
        max_boundary_step_to_peak_ratio: maximum_ratio,
        detected_click_count: click_count,
    })
}

/// Default rejection ceiling for [`time_varying_spectral_notches`].
///
/// A moving, regularly spaced notch family must reach 15 dB before it is
/// classified as coherent comb coloration. The detector already requires at
/// least five notches and motion between adjacent analysis windows; the 15 dB
/// ceiling leaves margin for the roughly 0.5 dB run-to-run ray-simulation
/// variance in the linked Steam Audio qualification.
pub const DEFAULT_MOVING_NOTCH_THRESHOLD_DB: f32 = 15.0;

/// Time-varying comb-filter evidence on a summed binaural output.
///
/// Each Hann-windowed output spectrum is divided by the matching source
/// spectrum, so musical harmonics are not mistaken for render coloration.
/// Bins more than 45 dB below the source-window peak are excluded. A local
/// minimum is a notch when its three-bin shoulders exceed it by at least 3 dB.
/// A regular family contains at least five such minima at a common spacing
/// (±8%, at least two bins). A family is "moving" only when its spacing changes
/// by at least 5% (at least two bins) in adjacent windows. The reported gate
/// metric is the deepest weaker member of any moving pair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MovingSpectralNotchReport {
    pub window_frames: usize,
    pub hop_frames: usize,
    pub analyzed_windows: usize,
    pub regularly_spaced_window_count: usize,
    pub moving_window_pair_count: usize,
    pub deepest_regular_notch_db: f32,
    pub max_moving_notch_depth_db: f32,
}

/// Added moving-notch evidence relative to a matched stereo point render.
///
/// Every channel report analyzes the transfer `test / point_reference`; an
/// HRTF notch shared by the same trajectory is therefore removed before notch
/// families are classified. `maximum_added_moving_notch_depth_db` is the
/// maximum of the left-ear, right-ear, and mono-sum reports and is the scalar a
/// gate normally compares with [`DEFAULT_MOVING_NOTCH_THRESHOLD_DB`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StereoReferenceMovingNotchReport {
    /// Added moving regular-notch families in the left ear.
    pub left: MovingSpectralNotchReport,
    /// Added moving regular-notch families in the right ear.
    pub right: MovingSpectralNotchReport,
    /// Added moving regular-notch families after `(L + R) / 2` summation.
    pub mono_sum: MovingSpectralNotchReport,
    /// Deepest added moving-notch family found in any analyzed channel.
    pub maximum_added_moving_notch_depth_db: f32,
}

/// Detect moving regularly spaced spectral notches on the summed output.
///
/// `reference_mono` is the exact point-source program sent to the renderer. It
/// is used only to remove the recording's own spectrum; no individual render
/// path is inspected. `window_frames` must be a power of two.
pub fn time_varying_spectral_notches(
    spec: WavSpec,
    summed_stereo: &[f32],
    reference_mono: &[f32],
    window_frames: usize,
    hop_frames: usize,
) -> Result<MovingSpectralNotchReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if spec.channels != 2 {
        return Err(MetricError::StereoRequired);
    }
    if summed_stereo.len() % 2 != 0 || summed_stereo.len() / 2 != reference_mono.len() {
        return Err(MetricError::FrameChannelMismatch);
    }
    let mono = summed_stereo
        .chunks_exact(2)
        .map(|frame| 0.5 * (frame[0] + frame[1]))
        .collect::<Vec<_>>();
    moving_spectral_notches(
        spec.sample_rate_hz,
        &mono,
        reference_mono,
        window_frames,
        hop_frames,
    )
}

/// Detect renderer-added moving notch families against a matched point render.
///
/// `test_stereo` and `point_reference_stereo` must be sample-aligned renders of
/// the same program, listener/source trajectory, propagation, room, and HRTF;
/// only source-extent rendering should differ. The detector evaluates left,
/// right, and mono sum independently over 200 Hz–8 kHz. It excludes bins whose
/// point-reference magnitude is more than 45 dB below that window's reference
/// peak and otherwise retains [`time_varying_spectral_notches`]' regular-family
/// and motion rules. `window_frames` must be a power of two.
///
/// This differencing deliberately cannot reveal coloration already present in
/// both captures. It can also miss an added notch hidden under an excluded deep
/// reference notch, stationary/fewer-than-five notches, artifacts outside the
/// analyzed band, or events shorter than the window. Timing, gain, HRTF,
/// program, or stochastic-room mismatches can instead appear as false residual
/// structure, so provenance matching is part of the gate contract.
pub fn stereo_reference_moving_spectral_notches(
    spec: WavSpec,
    test_stereo: &[f32],
    point_reference_stereo: &[f32],
    window_frames: usize,
    hop_frames: usize,
) -> Result<StereoReferenceMovingNotchReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if spec.channels != 2 {
        return Err(MetricError::StereoRequired);
    }
    if !test_stereo.len().is_multiple_of(2) || test_stereo.len() != point_reference_stereo.len() {
        return Err(MetricError::FrameChannelMismatch);
    }
    let frame_count = test_stereo.len() / 2;
    let mut test_left = Vec::with_capacity(frame_count);
    let mut test_right = Vec::with_capacity(frame_count);
    let mut test_sum = Vec::with_capacity(frame_count);
    let mut reference_left = Vec::with_capacity(frame_count);
    let mut reference_right = Vec::with_capacity(frame_count);
    let mut reference_sum = Vec::with_capacity(frame_count);
    for (test, reference) in test_stereo
        .chunks_exact(2)
        .zip(point_reference_stereo.chunks_exact(2))
    {
        test_left.push(test[0]);
        test_right.push(test[1]);
        test_sum.push(0.5 * (test[0] + test[1]));
        reference_left.push(reference[0]);
        reference_right.push(reference[1]);
        reference_sum.push(0.5 * (reference[0] + reference[1]));
    }
    let left = moving_spectral_notches(
        spec.sample_rate_hz,
        &test_left,
        &reference_left,
        window_frames,
        hop_frames,
    )?;
    let right = moving_spectral_notches(
        spec.sample_rate_hz,
        &test_right,
        &reference_right,
        window_frames,
        hop_frames,
    )?;
    let mono_sum = moving_spectral_notches(
        spec.sample_rate_hz,
        &test_sum,
        &reference_sum,
        window_frames,
        hop_frames,
    )?;
    let maximum_added_moving_notch_depth_db = left
        .max_moving_notch_depth_db
        .max(right.max_moving_notch_depth_db)
        .max(mono_sum.max_moving_notch_depth_db);
    Ok(StereoReferenceMovingNotchReport {
        left,
        right,
        mono_sum,
        maximum_added_moving_notch_depth_db,
    })
}

fn moving_spectral_notches(
    sample_rate_hz: u32,
    output: &[f32],
    reference: &[f32],
    window_frames: usize,
    hop_frames: usize,
) -> Result<MovingSpectralNotchReport, MetricError> {
    if output.len() != reference.len() {
        return Err(MetricError::FrameChannelMismatch);
    }
    if window_frames < 256
        || !window_frames.is_power_of_two()
        || hop_frames == 0
        || reference.len() < window_frames
        || output
            .iter()
            .chain(reference)
            .any(|sample| !sample.is_finite())
    {
        return Err(MetricError::InvalidWindow);
    }
    let first_bin = ((200.0 * window_frames as f64 / sample_rate_hz as f64).ceil() as usize).max(1);
    let last_bin = ((8_000.0 * window_frames as f64 / sample_rate_hz as f64).floor() as usize)
        .min(window_frames / 2 - 1);
    if last_bin <= first_bin + 8 {
        return Err(MetricError::InvalidWindow);
    }

    let mut families = Vec::<Option<NotchFamily>>::new();
    for start in (0..=reference.len() - window_frames).step_by(hop_frames) {
        let output_spectrum = windowed_magnitude_spectrum(&output[start..start + window_frames]);
        let reference_spectrum =
            windowed_magnitude_spectrum(&reference[start..start + window_frames]);
        let reference_peak = reference_spectrum[first_bin..=last_bin]
            .iter()
            .copied()
            .fold(0.0_f64, f64::max);
        if reference_peak <= f64::EPSILON {
            families.push(None);
            continue;
        }
        let eligibility_floor = reference_peak * 10.0_f64.powf(-45.0 / 20.0);
        let transfer_db = (first_bin..=last_bin)
            .map(|bin| {
                (reference_spectrum[bin] >= eligibility_floor).then(|| {
                    20.0 * ((output_spectrum[bin] + 1.0e-20) / (reference_spectrum[bin] + 1.0e-20))
                        .log10()
                })
            })
            .collect::<Vec<_>>();
        families.push(strongest_notch_family(&transfer_db));
    }

    let deepest_regular_notch_db = families
        .iter()
        .flatten()
        .map(|family| family.depth_db)
        .fold(0.0_f32, f32::max);
    let regularly_spaced_window_count = families.iter().flatten().count();
    let mut moving_window_pair_count = 0;
    let mut max_moving_notch_depth_db = 0.0_f32;
    for pair in families.windows(2) {
        let (Some(left), Some(right)) = (pair[0], pair[1]) else {
            continue;
        };
        let required_motion = 2.0_f32.max(left.spacing_bins.min(right.spacing_bins) * 0.05);
        if (left.spacing_bins - right.spacing_bins).abs() >= required_motion {
            moving_window_pair_count += 1;
            max_moving_notch_depth_db =
                max_moving_notch_depth_db.max(left.depth_db.min(right.depth_db));
        }
    }

    Ok(MovingSpectralNotchReport {
        window_frames,
        hop_frames,
        analyzed_windows: families.len(),
        regularly_spaced_window_count,
        moving_window_pair_count,
        deepest_regular_notch_db,
        max_moving_notch_depth_db,
    })
}

/// Default normalized 0.5–8 Hz gain-modulation ceiling for
/// [`summed_output_pump`].
pub const DEFAULT_PUMP_MODULATION_THRESHOLD: f32 = 0.20;

/// Default allowed fall between adjacent program-compensated approach windows.
///
/// The linked qualification uses two-second windows. A 0.75 dB allowance
/// rejects a real downward gain move while covering Steam Audio's known
/// approximately 0.5 dB stochastic ray-simulation variance.
pub const DEFAULT_APPROACH_DROP_TOLERANCE_DB: f32 = 0.75;

/// Pumping and monotonic-approach evidence on the summed binaural output.
///
/// Both measurements divide output RMS by the matching source-program RMS.
/// This removes the recording's intended vocal dynamics without inspecting an
/// individual render path. `modulation_depth` is the strongest 0.5–8 Hz
/// component of the 50 ms gain envelope after subtraction of a centered
/// one-second trend. `max_approach_drop_db` and
/// `approach_violation_count` apply the authority's monotonic law to adjacent
/// non-overlapping tolerance windows, stopping if summed output reaches the
/// stated safety ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SummedOutputPumpReport {
    pub envelope_window_frames: usize,
    pub monotonic_window_frames: usize,
    pub eligible_envelope_windows: usize,
    pub modulation_depth: f32,
    pub approach_window_count: usize,
    pub max_approach_drop_db: f32,
    pub approach_drop_tolerance_db: f32,
    pub approach_violation_count: usize,
    pub safety_ceiling_dbfs: f32,
    pub safety_ceiling_reached: bool,
}

pub fn summed_output_pump(
    spec: WavSpec,
    summed_stereo: &[f32],
    reference_mono: &[f32],
    approach_end_frame: usize,
    envelope_window_frames: usize,
    monotonic_window_frames: usize,
    safety_ceiling_dbfs: f32,
    approach_drop_tolerance_db: f32,
) -> Result<SummedOutputPumpReport, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    if spec.channels != 2 {
        return Err(MetricError::StereoRequired);
    }
    if summed_stereo.len() % 2 != 0 || summed_stereo.len() / 2 != reference_mono.len() {
        return Err(MetricError::FrameChannelMismatch);
    }
    let frame_count = reference_mono.len();
    if envelope_window_frames == 0
        || monotonic_window_frames == 0
        || frame_count < envelope_window_frames
        || approach_end_frame < monotonic_window_frames * 2
        || approach_end_frame > frame_count
    {
        return Err(MetricError::InvalidWindow);
    }
    if !safety_ceiling_dbfs.is_finite()
        || safety_ceiling_dbfs > 0.0
        || !approach_drop_tolerance_db.is_finite()
        || approach_drop_tolerance_db < 0.0
        || summed_stereo
            .iter()
            .chain(reference_mono)
            .any(|sample| !sample.is_finite())
    {
        return Err(MetricError::InvalidThreshold);
    }

    let global_reference_rms = rms_mono(reference_mono);
    if global_reference_rms <= f64::EPSILON {
        return Err(MetricError::InvalidWindow);
    }
    let reference_floor = global_reference_rms * 10.0_f64.powf(-36.0 / 20.0);
    let mut gain_envelope = Vec::new();
    let mut eligible_envelope_windows = 0;
    let mut held_gain = 0.0_f64;
    for start in (0..=frame_count - envelope_window_frames).step_by(envelope_window_frames) {
        let end = start + envelope_window_frames;
        let reference_rms = rms_mono(&reference_mono[start..end]);
        if reference_rms >= reference_floor {
            held_gain = stereo_rms(&summed_stereo[start * 2..end * 2]) / reference_rms;
            eligible_envelope_windows += 1;
        }
        gain_envelope.push(held_gain);
    }
    let envelope_rate_hz = spec.sample_rate_hz as f64 / envelope_window_frames as f64;
    let trend_radius = (envelope_rate_hz / 2.0).round().max(1.0) as usize;
    let residual = centered_fractional_residual(&gain_envelope, trend_radius);
    let modulation_depth = strongest_modulation(&residual, envelope_rate_hz, 0.5, 8.0) as f32;

    let mut approach_levels = Vec::new();
    let mut safety_ceiling_reached = false;
    for start in (0..=approach_end_frame - monotonic_window_frames).step_by(monotonic_window_frames)
    {
        let end = start + monotonic_window_frames;
        let reference_rms = rms_mono(&reference_mono[start..end]);
        if reference_rms < reference_floor {
            continue;
        }
        let output_rms = stereo_rms(&summed_stereo[start * 2..end * 2]);
        let output_dbfs = 20.0 * output_rms.max(1.0e-12).log10();
        if output_dbfs >= f64::from(safety_ceiling_dbfs) {
            safety_ceiling_reached = true;
            break;
        }
        approach_levels.push((20.0 * (output_rms / reference_rms).max(1.0e-12).log10()) as f32);
    }
    let mut max_approach_drop_db = 0.0_f32;
    let mut approach_violation_count = 0;
    for pair in approach_levels.windows(2) {
        let drop_db = (pair[0] - pair[1]).max(0.0);
        max_approach_drop_db = max_approach_drop_db.max(drop_db);
        if drop_db > approach_drop_tolerance_db {
            approach_violation_count += 1;
        }
    }

    Ok(SummedOutputPumpReport {
        envelope_window_frames,
        monotonic_window_frames,
        eligible_envelope_windows,
        modulation_depth,
        approach_window_count: approach_levels.len(),
        max_approach_drop_db,
        approach_drop_tolerance_db,
        approach_violation_count,
        safety_ceiling_dbfs,
        safety_ceiling_reached,
    })
}

#[derive(Clone, Copy)]
struct NotchFamily {
    spacing_bins: f32,
    depth_db: f32,
}

fn strongest_notch_family(transfer_db: &[Option<f64>]) -> Option<NotchFamily> {
    let mut candidates = Vec::<(usize, f32)>::new();
    for bin in 4..transfer_db.len().saturating_sub(4) {
        let Some(center) = transfer_db[bin] else {
            continue;
        };
        let left = transfer_db[bin - 4..bin - 1]
            .iter()
            .copied()
            .flatten()
            .sum::<f64>()
            / 3.0;
        let right = transfer_db[bin + 2..=bin + 4]
            .iter()
            .copied()
            .flatten()
            .sum::<f64>()
            / 3.0;
        if transfer_db[bin - 4..bin - 1].iter().any(Option::is_none)
            || transfer_db[bin + 2..=bin + 4].iter().any(Option::is_none)
        {
            continue;
        }
        let depth = left.min(right) - center;
        if depth >= 3.0
            && transfer_db[bin - 1].is_some_and(|value| center < value)
            && transfer_db[bin + 1].is_some_and(|value| center <= value)
        {
            candidates.push((bin, depth as f32));
        }
    }

    let mut strongest = None;
    for first in 0..candidates.len() {
        for second in first + 1..candidates.len() {
            let spacing = candidates[second].0 - candidates[first].0;
            if !(4..=256).contains(&spacing) {
                continue;
            }
            let tolerance = ((spacing as f32 * 0.08).round() as usize).max(2);
            let mut depths = vec![candidates[first].1, candidates[second].1];
            let mut expected = candidates[second].0 + spacing;
            let mut search = second + 1;
            while expected < transfer_db.len() && search < candidates.len() {
                while search < candidates.len()
                    && candidates[search].0.saturating_add(tolerance) < expected
                {
                    search += 1;
                }
                if search < candidates.len() && candidates[search].0.abs_diff(expected) <= tolerance
                {
                    depths.push(candidates[search].1);
                    expected = candidates[search].0 + spacing;
                    search += 1;
                } else {
                    expected += spacing;
                }
            }
            if depths.len() < 5 {
                continue;
            }
            depths.sort_by(f32::total_cmp);
            let depth_db = depths[depths.len() / 2];
            if strongest.is_none_or(|family: NotchFamily| depth_db > family.depth_db) {
                strongest = Some(NotchFamily {
                    spacing_bins: spacing as f32,
                    depth_db,
                });
            }
        }
    }
    strongest
}

fn windowed_magnitude_spectrum(samples: &[f32]) -> Vec<f64> {
    let n = samples.len();
    let mut real = samples
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let window = 0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (n - 1) as f64).cos();
            f64::from(*sample) * window
        })
        .collect::<Vec<_>>();
    let mut imaginary = vec![0.0_f64; n];
    fft_in_place(&mut real, &mut imaginary);
    real.into_iter()
        .zip(imaginary)
        .map(|(real, imaginary)| (real * real + imaginary * imaginary).sqrt())
        .collect()
}

fn fft_in_place(real: &mut [f64], imaginary: &mut [f64]) {
    let n = real.len();
    debug_assert!(n.is_power_of_two() && imaginary.len() == n);
    let mut target = 0;
    for source in 1..n {
        let mut bit = n >> 1;
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
    let mut length = 2;
    while length <= n {
        let angle = -std::f64::consts::TAU / length as f64;
        let step_real = angle.cos();
        let step_imaginary = angle.sin();
        for start in (0..n).step_by(length) {
            let mut twiddle_real = 1.0;
            let mut twiddle_imaginary = 0.0;
            for offset in 0..length / 2 {
                let even = start + offset;
                let odd = even + length / 2;
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
        length *= 2;
    }
}

fn rms_mono(samples: &[f32]) -> f64 {
    (samples
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / samples.len().max(1) as f64)
        .sqrt()
}

fn stereo_rms(interleaved: &[f32]) -> f64 {
    (interleaved
        .iter()
        .map(|sample| f64::from(*sample) * f64::from(*sample))
        .sum::<f64>()
        / interleaved.len().max(1) as f64)
        .sqrt()
}

fn centered_fractional_residual(values: &[f64], radius: usize) -> Vec<f64> {
    (0..values.len())
        .map(|index| {
            let start = index.saturating_sub(radius);
            let end = (index + radius + 1).min(values.len());
            let trend = values[start..end].iter().sum::<f64>() / (end - start) as f64;
            if trend.abs() > 1.0e-12 {
                values[index] / trend - 1.0
            } else {
                0.0
            }
        })
        .collect()
}

fn strongest_modulation(
    residual: &[f64],
    envelope_rate_hz: f64,
    minimum_hz: f64,
    maximum_hz: f64,
) -> f64 {
    if residual.is_empty() || envelope_rate_hz <= minimum_hz * 2.0 {
        return 0.0;
    }
    let maximum_hz = maximum_hz.min(envelope_rate_hz * 0.5);
    let steps = ((maximum_hz - minimum_hz) * 10.0).floor().max(0.0) as usize;
    (0..=steps)
        .map(|step| minimum_hz + step as f64 / 10.0)
        .map(|frequency| {
            let (cosine, sine) = residual.iter().enumerate().fold(
                (0.0_f64, 0.0_f64),
                |(cosine, sine), (index, value)| {
                    let phase = std::f64::consts::TAU * frequency * index as f64 / envelope_rate_hz;
                    (cosine + value * phase.cos(), sine + value * phase.sin())
                },
            );
            2.0 * (cosine * cosine + sine * sine).sqrt() / residual.len() as f64
        })
        .fold(0.0_f64, f64::max)
}

/// RMS dBFS of the mono mixdown, or `None` if silent.
fn mono_rms_dbfs(spec: WavSpec, samples: &[f32]) -> Result<Option<f32>, MetricError> {
    let mono = mono_mixdown(spec, samples)?;
    if mono.is_empty() {
        return Ok(None);
    }
    let sum_sq = mono
        .iter()
        .map(|s| f64::from(*s) * f64::from(*s))
        .sum::<f64>();
    let rms = (sum_sq / mono.len() as f64).sqrt() as f32;
    Ok((rms > 0.0).then(|| 20.0 * rms.log10()))
}

/// Average all channels per frame into a mono buffer of length `frame_count`.
fn mono_mixdown(spec: WavSpec, samples: &[f32]) -> Result<Vec<f32>, MetricError> {
    validate_spec(spec).map_err(|_| MetricError::BadSpec)?;
    let channels = usize::from(spec.channels);
    if channels != 0 && samples.len() % channels != 0 {
        return Err(MetricError::FrameChannelMismatch);
    }
    let frame_count = if channels == 0 {
        0
    } else {
        samples.len() / channels
    };
    let mut out = Vec::with_capacity(frame_count);
    for frame in 0..frame_count {
        let mut acc = 0.0_f64;
        for c in 0..channels {
            acc += samples[frame * channels + c] as f64;
        }
        out.push((acc / channels as f64) as f32);
    }
    Ok(out)
}

/// Naive bounded DFT magnitude at a single frequency, normalized by N. Computed
/// in `f64` for stability on small windows.
fn dft_magnitude(sample_rate_hz: u32, samples: &[f32], frequency_hz: f32) -> f32 {
    let n = samples.len();
    if n == 0 {
        return 0.0;
    }
    let omega = 2.0 * std::f64::consts::PI * frequency_hz as f64 / sample_rate_hz as f64;
    let mut re = 0.0_f64;
    let mut im = 0.0_f64;
    for (k, &s) in samples.iter().enumerate() {
        let angle = omega * k as f64;
        re += s as f64 * angle.cos();
        im -= s as f64 * angle.sin();
    }
    ((re * re + im * im).sqrt() / n as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal;

    fn spec(channels: u16) -> WavSpec {
        WavSpec {
            sample_rate_hz: 48_000,
            channels,
        }
    }

    #[test]
    fn full_scale_sine_references_minus_three_dbfs() {
        let spec = spec(1);
        // Calibration is bypassed here: raw full-scale sine -> ~-3.01 dBFS.
        let s: Vec<f32> = (0..48_000)
            .map(|n| (2.0 * std::f32::consts::PI * 1_000.0 * n as f32 / 48_000.0).sin())
            .collect();
        let m = channel_metrics(spec, &s).unwrap();
        assert!((m.rms_dbfs_per_channel[0].unwrap() - (-3.01)).abs() < 0.01);
        assert_eq!(m.silent_channel_count, 0);
        assert_eq!(m.stereo_difference_rms, None);
    }

    #[test]
    fn stereo_difference_detects_side_content() {
        let spec = spec(2);
        // L = +1, R = -1 every frame: side RMS = 2.0.
        let correlated: Vec<f32> = (0..100).flat_map(|_| [0.5, 0.5]).collect();
        let anti: Vec<f32> = (0..100).flat_map(|_| [0.5, -0.5]).collect();
        let m_cor = channel_metrics(spec, &correlated).unwrap();
        let m_anti = channel_metrics(spec, &anti).unwrap();
        assert!(m_cor.stereo_difference_rms.unwrap() < 1e-6);
        assert!((m_anti.stereo_difference_rms.unwrap() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn comparison_rejects_silent_false_positive_and_finds_real_differences() {
        let spec = spec(1);
        let silence = vec![0.0; 4_800];
        let both_silent =
            compare_pathing(spec, &silence, &silence, &[500.0, 1_000.0, 2_000.0]).unwrap();
        assert_eq!(both_silent.energy, ComparisonEnergy::BothSilent);
        assert_eq!(both_silent.level_difference_db, None);
        assert_eq!(both_silent.spectral_l1_difference, 0.0);
        assert!(!both_silent.differs);
        let both_silent_json = both_silent.to_json();
        assert!(both_silent_json.contains(r#""level_difference_db":null"#));
        assert!(both_silent_json.contains(r#""energy":"both_silent""#));
        assert!(both_silent_json.contains(r#""differs":false"#));

        let off = signal::sine(spec, 1_000.0, 4_800, -20.0).unwrap().samples;
        let one_silent = compare_pathing(spec, &off, &silence, &[500.0, 1_000.0, 2_000.0]).unwrap();
        assert_eq!(one_silent.energy, ComparisonEnergy::OnOnly);
        assert_eq!(one_silent.level_difference_db, None);
        assert!(one_silent.differs);
        assert!(one_silent.spectral_l1_difference.is_finite());

        let other_silent =
            compare_pathing(spec, &silence, &off, &[500.0, 1_000.0, 2_000.0]).unwrap();
        assert_eq!(other_silent.energy, ComparisonEnergy::OffOnly);
        assert_eq!(other_silent.level_difference_db, None);
        assert!(other_silent.differs);

        // Identical nonzero captures do not differ.
        let same = compare_pathing(spec, &off, &off, &[500.0, 1_000.0, 2_000.0]).unwrap();
        assert_eq!(same.energy, ComparisonEnergy::BothEnergetic);
        assert_eq!(same.level_difference_db, Some(0.0));
        assert!(!same.differs);
        assert!(same.spectral_l1_difference.abs() < 1e-6);

        // Pathing "on" adds level and spectral energy at 1 kHz.
        let mut on = off.clone();
        let added = signal::sine(spec, 1_000.0, 4_800, -12.0).unwrap().samples;
        for (o, a) in on.iter_mut().zip(added.iter()) {
            *o += a;
        }
        let diff = compare_pathing(spec, &on, &off, &[500.0, 1_000.0, 2_000.0]).unwrap();
        assert_eq!(diff.energy, ComparisonEnergy::BothEnergetic);
        assert!(diff.differs);
        assert!(diff.level_difference_db.unwrap() > 0.0);
        assert!(diff.spectral_l1_difference > DEFAULT_SPECTRAL_L1_DIFFERENCE);
    }

    #[test]
    fn continuity_flags_injected_click() {
        let spec = spec(1);
        let clean = signal::sine(spec, 220.0, 4_800, -12.0).unwrap().samples;
        let report = continuity(spec, &clean, DEFAULT_CLICK_RATIO_THRESHOLD).unwrap();
        assert!(!report.click_budget_exceeded);

        let mut clicked = clean.clone();
        clicked[2_400] += 0.9;
        let bad = continuity(spec, &clicked, DEFAULT_CLICK_RATIO_THRESHOLD).unwrap();
        assert!(bad.click_budget_exceeded);
    }

    #[test]
    fn iacc_finds_correlated_and_decorrelated_stereo() {
        let spec = spec(2);
        let correlated = (0..512)
            .flat_map(|frame| {
                let sample = (frame as f32 * 0.173).sin();
                [sample, sample]
            })
            .collect::<Vec<_>>();
        let correlated_report =
            interaural_cross_correlation(spec, &correlated, 0, 512, 48).unwrap();
        assert!((correlated_report.coefficient - 1.0).abs() < 1.0e-5);
        assert_eq!(correlated_report.lag_samples, 0);

        let decorrelated = (0..512)
            .flat_map(|frame| {
                let left = (frame as f32 * 0.173).sin();
                let right = (((frame * 73 + 19) % 509) as f32 * 0.317).sin() * 0.7;
                [left, right]
            })
            .collect::<Vec<_>>();
        let decorrelated_report =
            interaural_cross_correlation(spec, &decorrelated, 0, 512, 48).unwrap();
        assert!(decorrelated_report.coefficient < 0.25);
    }

    #[test]
    fn reflection_density_counts_separated_indirect_arrivals() {
        let spec = spec(2);
        let mut indirect = vec![0.0_f32; 4_800 * 2];
        for (frame, amplitude) in [(480, 1.0), (960, 0.8), (1_440, 0.6), (2_400, 0.4)] {
            indirect[frame * 2] = amplitude;
            indirect[frame * 2 + 1] = amplitude * 0.8;
        }
        let report = reflection_density(spec, &indirect, 0, 4_800, 0.2, 96).unwrap();
        assert_eq!(report.arrival_count, 4);
        assert!((report.arrivals_per_second - 40.0).abs() < 1.0e-5);
    }

    #[test]
    fn summed_continuity_reports_level_step_and_boundary_click() {
        let spec = spec(2);
        let block_frames = 128;
        let smooth = (0..block_frames * 3)
            .flat_map(|frame| {
                let sample = (frame as f32 * 220.0 * std::f32::consts::TAU / 48_000.0).sin()
                    * if frame < block_frames { 0.1 } else { 0.2 };
                [sample, sample]
            })
            .collect::<Vec<_>>();
        let report = summed_output_continuity(spec, &smooth, block_frames, 32, 0.5).unwrap();
        assert!(report.max_inter_block_level_step_db > 5.0);
        assert_eq!(report.detected_click_count, 0);

        let mut clicked = smooth;
        clicked[block_frames * 2] += 1.0;
        let clicked_report =
            summed_output_continuity(spec, &clicked, block_frames, 32, 0.5).unwrap();
        assert_eq!(clicked_report.detected_click_count, 1);
    }

    #[test]
    fn moving_spectral_notches_separate_dry_from_time_varying_comb() {
        let spec = spec(2);
        let window_frames = 16_384;
        let mut state = 0x8f3a_21c7_d4e5_690b_u64;
        let reference = (0..window_frames * 8)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                ((state >> 40) as i32 - (1 << 23)) as f32 / (1 << 23) as f32 * 0.2
            })
            .collect::<Vec<_>>();
        let dry = reference
            .iter()
            .flat_map(|sample| [*sample, *sample])
            .collect::<Vec<_>>();
        let dry_report =
            time_varying_spectral_notches(spec, &dry, &reference, window_frames, window_frames)
                .unwrap();

        let comb_mono = reference
            .iter()
            .enumerate()
            .map(|(frame, sample)| {
                let delay = if frame < reference.len() / 2 { 48 } else { 192 };
                let window_start = frame / window_frames * window_frames;
                let offset = frame % window_frames;
                let delayed = window_start + (offset + window_frames - delay) % window_frames;
                *sample + reference[delayed] * 0.98
            })
            .collect::<Vec<_>>();
        let comb = comb_mono
            .iter()
            .flat_map(|sample| [*sample, *sample])
            .collect::<Vec<_>>();
        let comb_report =
            time_varying_spectral_notches(spec, &comb, &reference, window_frames, window_frames)
                .unwrap();

        assert!(dry_report.max_moving_notch_depth_db < 3.0, "{dry_report:?}");
        assert!(
            comb_report.max_moving_notch_depth_db > DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
            "{comb_report:?}"
        );
        assert!(comb_report.moving_window_pair_count >= 1);
    }

    #[test]
    fn stereo_reference_cancels_moving_hrtf_but_catches_walking_comb() {
        use crate::ears::corpus::{MOVING_NOTCH_WINDOW_FRAMES, Stereo, moving_notch_orbit};

        fn interleave(stereo: &Stereo) -> Vec<f32> {
            stereo
                .left
                .iter()
                .zip(&stereo.right)
                .flat_map(|(&left, &right)| [left, right])
                .collect()
        }

        let corpus = moving_notch_orbit();
        let point = interleave(&corpus.point_reference);
        let honest = interleave(&corpus.honest_width);
        let corrupt = interleave(&corpus.coherent_path_sweep);
        let raw_hrtf = time_varying_spectral_notches(
            spec(2),
            &point,
            &corpus.source_mono,
            MOVING_NOTCH_WINDOW_FRAMES,
            MOVING_NOTCH_WINDOW_FRAMES,
        )
        .unwrap();
        let point_shaped = stereo_reference_moving_spectral_notches(
            spec(2),
            &point,
            &point,
            MOVING_NOTCH_WINDOW_FRAMES,
            MOVING_NOTCH_WINDOW_FRAMES,
        )
        .unwrap();
        let honest_report = stereo_reference_moving_spectral_notches(
            spec(2),
            &honest,
            &point,
            MOVING_NOTCH_WINDOW_FRAMES,
            MOVING_NOTCH_WINDOW_FRAMES,
        )
        .unwrap();
        let corrupt_report = stereo_reference_moving_spectral_notches(
            spec(2),
            &corrupt,
            &point,
            MOVING_NOTCH_WINDOW_FRAMES,
            MOVING_NOTCH_WINDOW_FRAMES,
        )
        .unwrap();
        eprintln!(
            "moving-notch ground truth: raw_hrtf={:.3} dB, point_added={:.3} dB, honest_width_added={:.3} dB, walking_comb_added={:.3} dB (L={:.3}, R={:.3}, sum={:.3})",
            raw_hrtf.max_moving_notch_depth_db,
            point_shaped.maximum_added_moving_notch_depth_db,
            honest_report.maximum_added_moving_notch_depth_db,
            corrupt_report.maximum_added_moving_notch_depth_db,
            corrupt_report.left.max_moving_notch_depth_db,
            corrupt_report.right.max_moving_notch_depth_db,
            corrupt_report.mono_sum.max_moving_notch_depth_db,
        );

        assert!(
            raw_hrtf.max_moving_notch_depth_db > DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
            "the source-referenced detector must reproduce the HRTF false positive"
        );
        assert_eq!(
            point_shaped.maximum_added_moving_notch_depth_db, 0.0,
            "an honest point compared with itself must be point-shaped"
        );
        assert!(
            honest_report.maximum_added_moving_notch_depth_db < 3.0,
            "matched-HRTF honest width must pass: {honest_report:?}"
        );
        assert!(
            corrupt_report.maximum_added_moving_notch_depth_db > DEFAULT_MOVING_NOTCH_THRESHOLD_DB,
            "the walking coherent path must fail behind the HRTF: {corrupt_report:?}"
        );
        assert!(corrupt_report.left.moving_window_pair_count > 0);
        assert!(corrupt_report.right.moving_window_pair_count > 0);
        assert!(corrupt_report.mono_sum.moving_window_pair_count > 0);
    }

    #[test]
    fn pump_detector_finds_modulation_and_approach_reversal() {
        let spec = spec(2);
        let frames = 48_000 * 8;
        let reference = (0..frames)
            .map(|frame| {
                let time = frame as f32 / 48_000.0;
                0.12 * (std::f32::consts::TAU * 311.0 * time).sin()
                    + 0.08 * (std::f32::consts::TAU * 997.0 * time).sin()
            })
            .collect::<Vec<_>>();
        let clean = reference
            .iter()
            .enumerate()
            .flat_map(|(frame, sample)| {
                let gain = 0.2 + 0.6 * frame as f32 / (frames - 1) as f32;
                [*sample * gain, *sample * gain]
            })
            .collect::<Vec<_>>();
        let clean_report = summed_output_pump(
            spec,
            &clean,
            &reference,
            frames,
            2_400,
            48_000,
            -1.0,
            DEFAULT_APPROACH_DROP_TOLERANCE_DB,
        )
        .unwrap();
        assert!(clean_report.modulation_depth < 0.05, "{clean_report:?}");
        assert_eq!(clean_report.approach_violation_count, 0);

        let pumped = reference
            .iter()
            .enumerate()
            .flat_map(|(frame, sample)| {
                let time = frame as f32 / 48_000.0;
                let approach = 0.2 + 0.6 * frame as f32 / (frames - 1) as f32;
                let reversal = if (3 * 48_000..5 * 48_000).contains(&frame) {
                    0.35
                } else {
                    1.0
                };
                let limiter = 0.7 + 0.28 * (std::f32::consts::TAU * 3.0 * time).sin();
                let output = *sample * approach * reversal * limiter;
                [output, output]
            })
            .collect::<Vec<_>>();
        let pumped_report = summed_output_pump(
            spec,
            &pumped,
            &reference,
            frames,
            2_400,
            48_000,
            -1.0,
            DEFAULT_APPROACH_DROP_TOLERANCE_DB,
        )
        .unwrap();
        assert!(
            pumped_report.modulation_depth > DEFAULT_PUMP_MODULATION_THRESHOLD,
            "{pumped_report:?}"
        );
        assert!(pumped_report.approach_violation_count >= 1);
        assert!(pumped_report.max_approach_drop_db > DEFAULT_APPROACH_DROP_TOLERANCE_DB);
    }

    #[test]
    fn metrics_json_is_deterministic() {
        let spec = spec(1);
        let s = signal::sine(spec, 1_000.0, 480, -20.0).unwrap().samples;
        let m = channel_metrics(spec, &s).unwrap();
        assert_eq!(m.to_json(), m.to_json());
        assert!(m.to_json().contains("\"all_finite\":true"));
    }

    #[test]
    fn spectral_comparison_emits_bins_as_finite_json_numbers() {
        let spec = spec(1);
        let off = signal::sine(spec, 1_000.0, 4_800, -20.0).unwrap().samples;
        let comparison = compare_pathing(spec, &off, &off, &[500.0, 1_000.0, 2_000.0]).unwrap();
        let json = comparison.to_json();
        // bins_hz are unquoted JSON numbers in declared order.
        assert!(json.contains(r#""bins_hz":[500,1000,2000]"#));
        // A string-valued array would open with a quote right after the bracket.
        assert!(!json.contains(r##""bins_hz":[""##));
        // The remaining shape is stable and finite.
        assert!(json.contains(r#""on_rms_dbfs":"#));
        assert!(json.contains(r#""off_rms_dbfs":"#));
        assert!(json.contains(r#""level_difference_db":0"#));
        assert!(json.contains(r#""energy":"both_energetic""#));
        assert!(json.contains(r#""differs":false"#));
        assert_eq!(json, comparison.to_json());
    }
}
