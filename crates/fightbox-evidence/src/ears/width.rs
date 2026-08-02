//! Windowed binaural-width evidence aligned with source geometry.

use super::dsp::{EPS, db20, median, normalized_correlation};
use super::{AnalysisError, Pcm};

/// Geometry samples aligned one-for-one with the frames in a stereo capture.
///
/// `distances_m` is required and normally contains the source-center distance.
/// `angular_subtenses_rad` is optional because a point control has no physical
/// extent to report. When supplied, it is the full angle between the extent's
/// endpoint directions, in radians. Both slices must contain one value per PCM
/// frame; the profiler reports their per-window medians.
#[derive(Debug, Clone, Copy)]
pub struct WidthTrack<'a> {
    /// Positive source-center distance for every capture frame, in metres.
    pub distances_m: &'a [f32],
    /// Optional full angular subtense for every capture frame, in radians.
    pub angular_subtenses_rad: Option<&'a [f32]>,
}

/// Window and admission settings for [`windowed_width_profile`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthProfileConfig {
    /// Number of PCM frames in each analysis window.
    pub window_frames: usize,
    /// Number of PCM frames between adjacent window starts.
    pub hop_frames: usize,
    /// Maximum absolute lag searched by IACC, in samples.
    pub max_iacc_lag_samples: usize,
    /// Stereo RMS floor below which a window is marked unadmitted.
    pub minimum_rms_dbfs: f64,
}

impl WidthProfileConfig {
    /// Construct the Wave 11 qualification settings for `sample_rate_hz`.
    ///
    /// This is a 500 ms window, 50 percent overlap, a ±1 ms IACC search, and a
    /// −60 dBFS admission floor. Callers may construct the public fields
    /// directly when a diagnostic needs different settings.
    #[must_use]
    pub fn wave11(sample_rate_hz: u32) -> Self {
        let window_frames = (sample_rate_hz as usize / 2).max(2);
        Self {
            window_frames,
            hop_frames: window_frames / 2,
            max_iacc_lag_samples: (sample_rate_hz as usize / 1_000).max(1),
            minimum_rms_dbfs: -60.0,
        }
    }
}

/// Width evidence for one capture window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthWindow {
    /// First PCM frame included in the window.
    pub start_frame: usize,
    /// Time at the center of the window, in seconds from capture start.
    pub center_time_s: f64,
    /// Median source-center distance in this window, in metres.
    pub distance_m: f64,
    /// Median full angular subtense, when supplied by [`WidthTrack`].
    pub angular_subtense_rad: Option<f64>,
    /// Peak absolute interaural correlation over the configured lag range.
    pub iacc: f64,
    /// The established Fightbox width convention, `1 - iacc`.
    pub width: f64,
    /// Side energy divided by mid-plus-side energy, using
    /// `mid = (L + R) / 2` and `side = (L - R) / 2`.
    pub lateral_energy_fraction: f64,
    /// Combined stereo RMS level for checking window admission.
    pub rms_dbfs: f64,
    /// Energy/balance confidence in `[0, 1]`; zero denotes silence or a
    /// completely missing ear, while ordinary energetic binaural windows are
    /// near one.
    pub confidence: f64,
    /// Whether the window clears the configured energy floor in both ears.
    /// Gates must ignore unadmitted windows: silence otherwise has undefined
    /// correlation and could look maximally wide.
    pub admitted: bool,
}

/// A deterministic sequence of geometry-aligned width windows.
#[derive(Debug, Clone, PartialEq)]
pub struct WidthProfile {
    /// Capture sample rate copied from the input PCM.
    pub sample_rate_hz: u32,
    /// Settings used to produce every window.
    pub config: WidthProfileConfig,
    /// Windows in increasing `start_frame` order.
    pub windows: Vec<WidthWindow>,
}

/// Measure IACC width and lateral energy over a time-aligned capture.
///
/// Geometry is summarized with a median so a short invalid trajectory spike
/// cannot relabel a whole window. The function returns every complete window,
/// including low-energy windows with [`WidthWindow::admitted`] set to `false`;
/// this preserves time alignment while making silence explicit. No distance
/// binning or pass/fail threshold is applied here, so a later gate can use the
/// exact bins and tolerances recorded in its evidence contract.
pub fn windowed_width_profile(
    pcm: Pcm<'_>,
    track: WidthTrack<'_>,
    config: WidthProfileConfig,
) -> Result<WidthProfile, AnalysisError> {
    pcm.validate()?;
    let angular_subtenses = track.angular_subtenses_rad;
    if track.distances_m.len() != pcm.left.len()
        || angular_subtenses.is_some_and(|values| values.len() != pcm.left.len())
    {
        return Err(AnalysisError::TrackLengthMismatch {
            frames: pcm.left.len(),
            distances: track.distances_m.len(),
            angular_subtenses: angular_subtenses.map(<[f32]>::len),
        });
    }
    if track
        .distances_m
        .iter()
        .any(|distance| !distance.is_finite() || *distance <= 0.0)
        || angular_subtenses.is_some_and(|values| {
            values
                .iter()
                .any(|angle| !angle.is_finite() || !(0.0..=std::f32::consts::PI).contains(angle))
        })
    {
        return Err(AnalysisError::InvalidTrack);
    }
    if config.window_frames < 2
        || config.window_frames > pcm.left.len()
        || config.hop_frames == 0
        || config.max_iacc_lag_samples >= config.window_frames
        || !config.minimum_rms_dbfs.is_finite()
        || config.minimum_rms_dbfs > 0.0
    {
        return Err(AnalysisError::InvalidConfiguration);
    }

    let minimum_rms = 10.0_f64.powf(config.minimum_rms_dbfs / 20.0);
    let mut windows = Vec::new();
    for start in (0..=pcm.left.len() - config.window_frames).step_by(config.hop_frames) {
        let end = start + config.window_frames;
        let left = &pcm.left[start..end];
        let right = &pcm.right[start..end];
        let (left_energy, right_energy, mid_energy, side_energy) = stereo_energies(left, right);
        let stereo_rms = (0.5 * (left_energy + right_energy)).sqrt();
        let rms_dbfs = db20(stereo_rms);
        let admitted = stereo_rms >= minimum_rms && left_energy > EPS && right_energy > EPS;
        let balance = if left_energy + right_energy > EPS {
            2.0 * (left_energy * right_energy).sqrt() / (left_energy + right_energy)
        } else {
            0.0
        };
        let confidence = (balance * (stereo_rms / minimum_rms.max(EPS)).min(1.0)).clamp(0.0, 1.0);
        let iacc = peak_iacc(left, right, config.max_iacc_lag_samples);
        let lateral_energy_fraction = if mid_energy + side_energy > EPS {
            side_energy / (mid_energy + side_energy)
        } else {
            0.0
        };
        let distances = track.distances_m[start..end]
            .iter()
            .map(|value| f64::from(*value))
            .collect::<Vec<_>>();
        let angular_subtense_rad = angular_subtenses.map(|values| {
            let values = values[start..end]
                .iter()
                .map(|value| f64::from(*value))
                .collect::<Vec<_>>();
            median(&values)
        });
        windows.push(WidthWindow {
            start_frame: start,
            center_time_s: (start as f64 + 0.5 * config.window_frames as f64)
                / pcm.sample_rate_hz as f64,
            distance_m: median(&distances),
            angular_subtense_rad,
            iacc,
            width: 1.0 - iacc,
            lateral_energy_fraction,
            rms_dbfs,
            confidence,
            admitted,
        });
    }

    Ok(WidthProfile {
        sample_rate_hz: pcm.sample_rate_hz,
        config,
        windows,
    })
}

fn stereo_energies(left: &[f32], right: &[f32]) -> (f64, f64, f64, f64) {
    let mut left_energy = 0.0;
    let mut right_energy = 0.0;
    let mut mid_energy = 0.0;
    let mut side_energy = 0.0;
    for (&left, &right) in left.iter().zip(right) {
        let left = f64::from(left);
        let right = f64::from(right);
        let mid = 0.5 * (left + right);
        let side = 0.5 * (left - right);
        left_energy += left * left;
        right_energy += right * right;
        mid_energy += mid * mid;
        side_energy += side * side;
    }
    let scale = 1.0 / left.len().max(1) as f64;
    (
        left_energy * scale,
        right_energy * scale,
        mid_energy * scale,
        side_energy * scale,
    )
}

fn peak_iacc(left: &[f32], right: &[f32], max_lag_samples: usize) -> f64 {
    (-(max_lag_samples as isize)..=max_lag_samples as isize)
        .map(|lag| normalized_correlation(left, right, lag, 4).abs())
        .fold(0.0, f64::max)
        .clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ears::corpus::{Corruption, clean, corrupted};

    fn median_width(profile: &WidthProfile) -> f64 {
        let values = profile
            .windows
            .iter()
            .filter(|window| window.admitted)
            .map(|window| window.width)
            .collect::<Vec<_>>();
        median(&values)
    }

    #[test]
    fn gate0_mono_collapse_fails_wave11_near_width_gate() {
        let wide = clean();
        let collapsed = corrupted(Corruption::MonoCollapsed);
        let distances = vec![2.5; wide.left.len()];
        let track = WidthTrack {
            distances_m: &distances,
            angular_subtenses_rad: None,
        };
        let config = WidthProfileConfig::wave11(48_000);
        let wide_profile = windowed_width_profile(wide.pcm(), track, config).unwrap();
        let collapsed_profile = windowed_width_profile(collapsed.pcm(), track, config).unwrap();
        let wide_median = median_width(&wide_profile);
        let collapsed_median = median_width(&collapsed_profile);
        eprintln!(
            "near-width corruption proof: wide={wide_median:.6}, mono-collapsed={collapsed_median:.6}"
        );

        assert!(wide_profile.windows.iter().all(|window| window.admitted));
        assert_eq!(wide_profile.windows.len(), 11);
        assert_eq!(wide_profile.config.hop_frames, 12_000);
        assert!(
            wide_median >= 0.20,
            "wide control must clear the Wave 11 gate"
        );
        assert!(
            collapsed_median < 0.01,
            "mono collapse must fail the same gate"
        );
    }

    #[test]
    fn constructed_width_collapses_monotonically_with_distance() {
        const SAMPLE_RATE: u32 = 48_000;
        const WINDOW: usize = SAMPLE_RATE as usize / 2;
        const DISTANCES: [f64; 8] = [2.5, 4.0, 6.3, 10.0, 16.0, 25.0, 32.0, 40.0];
        let mut state_center = 0x243f_6a88_85a3_08d3_u64;
        let mut state_side = 0x1319_8a2e_0370_7344_u64;
        let mut left = Vec::with_capacity(WINDOW * DISTANCES.len());
        let mut right = Vec::with_capacity(WINDOW * DISTANCES.len());
        let mut distances = Vec::with_capacity(WINDOW * DISTANCES.len());
        let mut subtenses = Vec::with_capacity(WINDOW * DISTANCES.len());
        let mut expected_widths = Vec::new();

        for distance in DISTANCES {
            let k = 3.0 / (distance * distance + 9.0).sqrt();
            expected_widths.push(2.0 * k * k / (1.0 + k * k));
            let subtense = 2.0 * k.asin();
            for _ in 0..WINDOW {
                let center = 0.12 * bipolar(&mut state_center);
                let side = 0.12 * bipolar(&mut state_side) * k;
                left.push((center + side) as f32);
                right.push((center - side) as f32);
                distances.push(distance as f32);
                subtenses.push(subtense as f32);
            }
        }

        let profile = windowed_width_profile(
            Pcm {
                left: &left,
                right: &right,
                sample_rate_hz: SAMPLE_RATE,
            },
            WidthTrack {
                distances_m: &distances,
                angular_subtenses_rad: Some(&subtenses),
            },
            WidthProfileConfig {
                window_frames: WINDOW,
                hop_frames: WINDOW,
                max_iacc_lag_samples: 48,
                minimum_rms_dbfs: -60.0,
            },
        )
        .unwrap();
        let mut corrupt_left = left.clone();
        let mut corrupt_right = right.clone();
        let corrupt_window = 5;
        let original_k = 3.0 / (DISTANCES[corrupt_window] * DISTANCES[corrupt_window] + 9.0).sqrt();
        let corrupt_k = 0.50;
        for frame in corrupt_window * WINDOW..(corrupt_window + 1) * WINDOW {
            let center = 0.5 * (left[frame] + right[frame]);
            let side = 0.5 * (left[frame] - right[frame]) * (corrupt_k / original_k) as f32;
            corrupt_left[frame] = center + side;
            corrupt_right[frame] = center - side;
        }
        let corrupt_profile = windowed_width_profile(
            Pcm {
                left: &corrupt_left,
                right: &corrupt_right,
                sample_rate_hz: SAMPLE_RATE,
            },
            WidthTrack {
                distances_m: &distances,
                angular_subtenses_rad: Some(&subtenses),
            },
            WidthProfileConfig {
                window_frames: WINDOW,
                hop_frames: WINDOW,
                max_iacc_lag_samples: 48,
                minimum_rms_dbfs: -60.0,
            },
        )
        .unwrap();
        let measured_widths = profile
            .windows
            .iter()
            .map(|window| window.width)
            .collect::<Vec<_>>();
        let corrupt_max_outward_increase = corrupt_profile
            .windows
            .windows(2)
            .map(|pair| (pair[1].width - pair[0].width).max(0.0))
            .fold(0.0_f64, f64::max);
        eprintln!("distance profile expected={expected_widths:?}");
        eprintln!("distance profile measured={measured_widths:?}");
        eprintln!(
            "monotonic corruption proof: max outward increase={corrupt_max_outward_increase:.6}"
        );

        assert_eq!(profile.windows.len(), DISTANCES.len());
        assert!(profile.windows.iter().all(|window| window.admitted));
        assert!(profile.windows.windows(2).all(|pair| {
            pair[0].distance_m < pair[1].distance_m && pair[0].width > pair[1].width
        }));
        for ((window, expected_width), expected_distance) in
            profile.windows.iter().zip(expected_widths).zip(DISTANCES)
        {
            assert!((window.distance_m - expected_distance).abs() < 1.0e-6);
            assert!((window.width - expected_width).abs() < 0.025);
            let expected_lateral = expected_width / 2.0;
            assert!((window.lateral_energy_fraction - expected_lateral).abs() < 0.01);
            let expected_subtense = 2.0 * (3.0 / (expected_distance.powi(2) + 9.0).sqrt()).asin();
            assert!((window.angular_subtense_rad.unwrap() - expected_subtense).abs() < 1.0e-6);
        }
        assert!(measured_widths[0] - measured_widths[7] > 0.70);
        assert!(
            corrupt_max_outward_increase > 0.02,
            "far-field width re-expansion must fail the adjacent-window tolerance"
        );
    }

    fn bipolar(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        let unit = (*state >> 11) as f64 / (1_u64 << 53) as f64;
        unit * 2.0 - 1.0
    }
}
