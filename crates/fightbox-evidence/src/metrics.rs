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
