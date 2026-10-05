//! Offline measurements for decoded, pre-drive program PCM.
//!
//! Program RMS and true peak are deliberately separate measurements:
//!
//! - RMS is the square root of the `f64` sum of squares divided by every
//!   interleaved sample in the full program. Channels are therefore aggregated
//!   by sample energy, never mixed down first.
//! - True peak is the maximum across channels reported by `ebur128` 0.1.10's
//!   custom polyphase FIR interpolator, converted to dBTP. It is never replaced
//!   by sample peak.

use ebur128::{EbuR128, Mode};
use fightbox_api::{AssetAnalysis, AssetMeasurementProvenance, CalibrationError};

use crate::wav::{WavError, WavSpec, validate_spec};

/// Stable identity for every measurement decision made by
/// [`analyze_decoded_asset`].
///
/// The rate-dependent interpolation factors are those documented by
/// `ebur128` 0.1.10: 4x below 96 kHz, 2x below 192 kHz, and no interpolation at
/// 192 kHz or above. Changing any token in this method requires a new analyzer
/// method ID.
pub const ASSET_ANALYSIS_METHOD_ID: &str = concat!(
    "fightbox.asset-analysis.v1",
    "|rms=f64-full-program-all-interleaved-samples",
    "|true_peak=ebur128@0.1.10-custom-polyphase-fir",
    "-max-across-channels-4x-below-96000hz",
    "-2x-below-192000hz-1x-at-or-above-192000hz",
    "|units=dbfs,dbtp"
);

/// Stable identity of channels represented by [`WavSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PcmChannelLayout {
    /// One ordinary full-range channel.
    Mono,
    /// Two interleaved full-range channels ordered left, right.
    StereoLeftRight,
}

impl PcmChannelLayout {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mono => "mono",
            Self::StereoLeftRight => "stereo_left_right",
        }
    }

    fn from_channels(channels: u16) -> Result<Self, AssetAnalysisError> {
        match channels {
            1 => Ok(Self::Mono),
            2 => Ok(Self::StereoLeftRight),
            _ => Err(AssetAnalysisError::InvalidSpec(
                WavError::InvalidChannelCount,
            )),
        }
    }
}

/// Decoded-PCM facts bound to an [`AssetAnalysis`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedPcmProvenance {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub channel_layout: PcmChannelLayout,
    pub frame_count: usize,
}

/// Caller-ready API analysis plus the decoded format it measured.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzedAsset {
    analysis: AssetAnalysis,
    pcm: DecodedPcmProvenance,
}

impl AnalyzedAsset {
    /// Public-API measurement ready for source-drive derivation.
    #[must_use]
    pub const fn analysis(&self) -> &AssetAnalysis {
        &self.analysis
    }

    /// Decoded sample-rate, channel-layout, and frame-count provenance.
    #[must_use]
    pub const fn pcm(&self) -> DecodedPcmProvenance {
        self.pcm
    }

    /// Split the result into its public-API measurement and PCM provenance.
    #[must_use]
    pub fn into_parts(self) -> (AssetAnalysis, DecodedPcmProvenance) {
        (self.analysis, self.pcm)
    }
}

/// Rejections from offline decoded-asset analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetAnalysisError {
    InvalidSpec(WavError),
    FrameChannelMismatch,
    EmptyProgram,
    SilentProgram,
    NonFiniteSample,
    AnalyzerInitialization,
    AnalyzerProcessing,
    AnalyzerResult,
    InvalidApiAnalysis(CalibrationError),
}

impl AssetAnalysisError {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidSpec(_) => "decoded PCM spec is invalid",
            Self::FrameChannelMismatch => {
                "interleaved sample count is not a whole number of frames"
            }
            Self::EmptyProgram => "decoded PCM program is empty",
            Self::SilentProgram => "decoded PCM program is silent",
            Self::NonFiniteSample => "decoded PCM contains NaN or infinity",
            Self::AnalyzerInitialization => "true-peak analyzer initialization failed",
            Self::AnalyzerProcessing => "true-peak analyzer rejected decoded PCM",
            Self::AnalyzerResult => "true-peak analyzer returned an invalid result",
            Self::InvalidApiAnalysis(_) => "measured analysis violates the public API contract",
        }
    }
}

impl From<CalibrationError> for AssetAnalysisError {
    fn from(error: CalibrationError) -> Self {
        Self::InvalidApiAnalysis(error)
    }
}

/// Analyze finite, interleaved, decoded program PCM before source drive.
///
/// The returned [`AssetAnalysis`] is ready for
/// [`fightbox_api::SceneCalibration::derive_source_drive`]. The accompanying
/// PCM provenance prevents a measurement from being detached from its sample
/// rate or channel interpretation.
pub fn analyze_decoded_asset(
    spec: WavSpec,
    interleaved_samples: &[f32],
) -> Result<AnalyzedAsset, AssetAnalysisError> {
    validate_spec(spec).map_err(AssetAnalysisError::InvalidSpec)?;

    let channels = usize::from(spec.channels);
    if interleaved_samples.len() % channels != 0 {
        return Err(AssetAnalysisError::FrameChannelMismatch);
    }
    if interleaved_samples.is_empty() {
        return Err(AssetAnalysisError::EmptyProgram);
    }
    if interleaved_samples.iter().any(|sample| !sample.is_finite()) {
        return Err(AssetAnalysisError::NonFiniteSample);
    }

    let sum_squares = interleaved_samples.iter().fold(0.0_f64, |sum, &sample| {
        sum + f64::from(sample) * f64::from(sample)
    });
    if sum_squares == 0.0 {
        return Err(AssetAnalysisError::SilentProgram);
    }
    let rms = (sum_squares / interleaved_samples.len() as f64).sqrt();
    let program_rms_dbfs = 20.0 * rms.log10();

    let mut meter = EbuR128::new(
        u32::from(spec.channels),
        spec.sample_rate_hz,
        Mode::TRUE_PEAK,
    )
    .map_err(|_| AssetAnalysisError::AnalyzerInitialization)?;
    meter
        .add_frames_f32(interleaved_samples)
        .map_err(|_| AssetAnalysisError::AnalyzerProcessing)?;

    let mut true_peak_amplitude = 0.0_f64;
    for channel in 0..u32::from(spec.channels) {
        let channel_peak = meter
            .true_peak(channel)
            .map_err(|_| AssetAnalysisError::AnalyzerResult)?;
        if !channel_peak.is_finite() || channel_peak < 0.0 {
            return Err(AssetAnalysisError::AnalyzerResult);
        }
        true_peak_amplitude = true_peak_amplitude.max(channel_peak);
    }
    if true_peak_amplitude == 0.0 {
        return Err(AssetAnalysisError::AnalyzerResult);
    }
    let true_peak_dbtp = 20.0 * true_peak_amplitude.log10();
    if !program_rms_dbfs.is_finite() || !true_peak_dbtp.is_finite() {
        return Err(AssetAnalysisError::AnalyzerResult);
    }

    let measurement_provenance = AssetMeasurementProvenance::new(ASSET_ANALYSIS_METHOD_ID)
        .map_err(AssetAnalysisError::from)?;
    let analysis = AssetAnalysis::new(
        program_rms_dbfs as f32,
        true_peak_dbtp as f32,
        measurement_provenance,
    )
    .map_err(AssetAnalysisError::from)?;

    Ok(AnalyzedAsset {
        analysis,
        pcm: DecodedPcmProvenance {
            sample_rate_hz: spec.sample_rate_hz,
            channels: spec.channels,
            channel_layout: PcmChannelLayout::from_channels(spec.channels)?,
            frame_count: interleaved_samples.len() / channels,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{pink_like, sine};

    fn mono_spec() -> WavSpec {
        WavSpec {
            sample_rate_hz: 48_000,
            channels: 1,
        }
    }

    fn sample_peak_dbfs(samples: &[f32]) -> f32 {
        let peak = samples
            .iter()
            .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
        20.0 * peak.log10()
    }

    #[test]
    fn sine_and_pink_measurements_are_deterministic() {
        let spec = mono_spec();
        let sine_a = sine(spec, 1_000.0, 48_000, -20.0).unwrap();
        let sine_b = sine(spec, 1_000.0, 48_000, -20.0).unwrap();
        let sine_analysis_a = analyze_decoded_asset(spec, &sine_a.samples).unwrap();
        let sine_analysis_b = analyze_decoded_asset(spec, &sine_b.samples).unwrap();
        assert_eq!(sine_analysis_a, sine_analysis_b);
        assert!((sine_analysis_a.analysis().program_rms_dbfs - -20.0).abs() < 0.001);
        assert!(sine_analysis_a.analysis().true_peak_dbtp.is_finite());

        let pink_a = pink_like(spec, 0xC0FFEE, 48_000, -20.0).unwrap();
        let pink_b = pink_like(spec, 0xC0FFEE, 48_000, -20.0).unwrap();
        let pink_analysis_a = analyze_decoded_asset(spec, &pink_a.samples).unwrap();
        let pink_analysis_b = analyze_decoded_asset(spec, &pink_b.samples).unwrap();
        assert_eq!(pink_analysis_a, pink_analysis_b);
        assert!((pink_analysis_a.analysis().program_rms_dbfs - -20.0).abs() < 0.001);
        assert!(pink_analysis_a.analysis().true_peak_dbtp.is_finite());
    }

    #[test]
    fn true_peak_is_finite_and_never_below_sample_peak() {
        let signal = pink_like(mono_spec(), 7, 48_000, -20.0).unwrap();
        let analyzed = analyze_decoded_asset(signal.spec, &signal.samples).unwrap();
        let sample_peak = sample_peak_dbfs(&signal.samples);
        assert!(analyzed.analysis().true_peak_dbtp.is_finite());
        assert!(analyzed.analysis().true_peak_dbtp + 1e-5 >= sample_peak);
    }

    #[test]
    fn intersample_stimulus_true_peak_exceeds_sample_peak() {
        // A quarter-sample-rate sine at pi/4 phase has samples at ±sqrt(1/2)
        // while its reconstructed waveform peaks between sample instants.
        let amplitude = 0.8_f32 * std::f32::consts::FRAC_1_SQRT_2;
        let pattern = [amplitude, amplitude, -amplitude, -amplitude];
        let samples: Vec<f32> = pattern.into_iter().cycle().take(48_000).collect();
        let analyzed = analyze_decoded_asset(mono_spec(), &samples).unwrap();
        let sample_peak = sample_peak_dbfs(&samples);
        assert!(
            analyzed.analysis().true_peak_dbtp > sample_peak + 0.5,
            "true peak {} dBTP did not exceed sample peak {sample_peak} dBFS",
            analyzed.analysis().true_peak_dbtp
        );
    }

    #[test]
    fn rejects_invalid_spec_mismatch_empty_silence_and_nonfinite() {
        let invalid_rate = WavSpec {
            sample_rate_hz: 0,
            channels: 1,
        };
        assert_eq!(
            analyze_decoded_asset(invalid_rate, &[0.5]).unwrap_err(),
            AssetAnalysisError::InvalidSpec(WavError::InvalidSampleRate)
        );

        let invalid_channels = WavSpec {
            sample_rate_hz: 48_000,
            channels: 3,
        };
        assert_eq!(
            analyze_decoded_asset(invalid_channels, &[0.5, 0.5, 0.5]).unwrap_err(),
            AssetAnalysisError::InvalidSpec(WavError::InvalidChannelCount)
        );

        let stereo = WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        };
        assert_eq!(
            analyze_decoded_asset(stereo, &[0.5]).unwrap_err(),
            AssetAnalysisError::FrameChannelMismatch
        );
        assert_eq!(
            analyze_decoded_asset(mono_spec(), &[]).unwrap_err(),
            AssetAnalysisError::EmptyProgram
        );
        assert_eq!(
            analyze_decoded_asset(mono_spec(), &[0.0; 64]).unwrap_err(),
            AssetAnalysisError::SilentProgram
        );
        for sample in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(
                analyze_decoded_asset(mono_spec(), &[sample]).unwrap_err(),
                AssetAnalysisError::NonFiniteSample
            );
        }
    }

    #[test]
    fn records_sample_rate_channel_layout_and_exact_method() {
        let spec = WavSpec {
            sample_rate_hz: 44_100,
            channels: 2,
        };
        let samples = [0.25_f32, -0.5].repeat(128);
        let analyzed = analyze_decoded_asset(spec, &samples).unwrap();
        assert_eq!(analyzed.pcm().sample_rate_hz, 44_100);
        assert_eq!(analyzed.pcm().channels, 2);
        assert_eq!(
            analyzed.pcm().channel_layout,
            PcmChannelLayout::StereoLeftRight
        );
        assert_eq!(analyzed.pcm().frame_count, 128);
        assert_eq!(
            analyzed.analysis().measurement_provenance.method_id,
            ASSET_ANALYSIS_METHOD_ID
        );
        assert_eq!(
            ASSET_ANALYSIS_METHOD_ID,
            concat!(
                "fightbox.asset-analysis.v1",
                "|rms=f64-full-program-all-interleaved-samples",
                "|true_peak=ebur128@0.1.10-custom-polyphase-fir",
                "-max-across-channels-4x-below-96000hz",
                "-2x-below-192000hz-1x-at-or-above-192000hz",
                "|units=dbfs,dbtp"
            )
        );
    }
}
