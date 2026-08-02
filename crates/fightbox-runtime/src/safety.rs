//! Control-rate output-safety publication and callback-safe DSP.

use crate::{MAX_ACTIVE_SOURCES, SnapshotPublication, SnapshotReader, SnapshotWriter};
use fightbox_api::{
    EnuVector3, OutputSafetyConfig, OutputSafetyConfigError, ReferenceLevel, SourceProfile,
};

/// Final limiter ceiling measured by the four-times oversampled detector.
pub const TRUE_PEAK_LIMITER_CEILING_DBTP: f32 = -1.0;
/// Fixed added latency: 32 samples (0.667 ms at 48 kHz).
pub const TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES: usize = 32;
/// One-pole gain-release time constant.
pub const TRUE_PEAK_LIMITER_RELEASE_SECONDS: f32 = 0.100;
const TRUE_PEAK_OVERSAMPLE_FACTOR: usize = 4;
const TRUE_PEAK_FILTER_TAPS: usize = 12;
const MIN_GEOMETRY_DISTANCE_M: f32 = 1.0e-6;

/// One complete set of callback targets. Values are already linearized on the
/// control thread, so reading and applying a publication requires no logarithm.
#[derive(Clone, Copy, Debug)]
struct OutputSafetySnapshot {
    source_gains: [f32; MAX_ACTIVE_SOURCES],
    monitor_gain: f32,
}

impl Default for OutputSafetySnapshot {
    fn default() -> Self {
        Self {
            source_gains: [1.0; MAX_ACTIVE_SOURCES],
            monitor_gain: 1.0,
        }
    }
}

/// Callback-side endpoint of the output-safety publication channel.
pub struct OutputSafetyReader {
    reader: SnapshotReader<OutputSafetySnapshot>,
}

impl OutputSafetyReader {
    pub(crate) fn read(&mut self) -> ([f32; MAX_ACTIVE_SOURCES], f32) {
        let snapshot = self.reader.read();
        (snapshot.source_gains, snapshot.monitor_gain)
    }
}

#[derive(Clone, Copy, Debug)]
struct ControlledSource {
    position: EnuVector3,
    declared_spl_at_one_meter_db: Option<f32>,
    radius_m: f32,
}

impl Default for ControlledSource {
    fn default() -> Self {
        Self {
            position: EnuVector3::default(),
            declared_spl_at_one_meter_db: None,
            radius_m: 1.0,
        }
    }
}

/// Single control-thread owner for geometry and monitor-gain publication.
///
/// Callers update this object from their simulation/control cadence and give
/// the paired [`OutputSafetyReader`] to [`crate::RuntimeGraph`]. None of this
/// geometry or logarithmic work runs in the audio callback.
pub struct OutputSafetyController {
    writer: SnapshotWriter<OutputSafetySnapshot>,
    config: OutputSafetyConfig,
    listener_position: EnuVector3,
    sources: [Option<ControlledSource>; MAX_ACTIVE_SOURCES],
}

/// Factory for the lock-free control-to-audio safety channel.
pub struct OutputSafetyPublication;

impl OutputSafetyPublication {
    pub fn new(
        config: OutputSafetyConfig,
    ) -> Result<(OutputSafetyController, OutputSafetyReader), OutputSafetyConfigError> {
        config.validate()?;
        let initial = OutputSafetySnapshot {
            monitor_gain: db_to_linear(config.monitor_gain_db),
            ..OutputSafetySnapshot::default()
        };
        let (writer, reader) = SnapshotPublication::new(initial);
        Ok((
            OutputSafetyController {
                writer,
                config,
                listener_position: EnuVector3::default(),
                sources: [None; MAX_ACTIVE_SOURCES],
            },
            OutputSafetyReader { reader },
        ))
    }
}

impl OutputSafetyController {
    pub fn set_config(
        &mut self,
        config: OutputSafetyConfig,
    ) -> Result<(), OutputSafetyConfigError> {
        config.validate()?;
        self.config = config;
        for source in self.sources.iter_mut().flatten() {
            if source.radius_m <= 0.0 || !source.radius_m.is_finite() {
                source.radius_m = config.default_source_radius_m;
            }
        }
        self.publish()
    }

    pub fn set_monitor_gain_db(
        &mut self,
        monitor_gain_db: f32,
    ) -> Result<(), OutputSafetyConfigError> {
        let config = OutputSafetyConfig {
            monitor_gain_db,
            ..self.config
        };
        self.set_config(config)
    }

    pub fn set_listener_position(
        &mut self,
        position: EnuVector3,
    ) -> Result<(), OutputSafetyConfigError> {
        if !position.is_finite() {
            return Err(OutputSafetyConfigError::InvalidPosition);
        }
        self.listener_position = position;
        self.publish()
    }

    pub fn set_source(
        &mut self,
        source_index: usize,
        profile: &SourceProfile,
        source_radius_m: Option<f32>,
    ) -> Result<(), OutputSafetyConfigError> {
        if source_index >= MAX_ACTIVE_SOURCES {
            return Err(OutputSafetyConfigError::InvalidSourceIndex);
        }
        profile
            .validate()
            .map_err(|_| OutputSafetyConfigError::InvalidSourceProfile)?;
        let radius_m = source_radius_m.unwrap_or(self.config.default_source_radius_m);
        if !radius_m.is_finite() || radius_m <= 0.0 {
            return Err(OutputSafetyConfigError::InvalidSourceRadius);
        }
        let declared_spl_at_one_meter_db = match profile.reference_level {
            ReferenceLevel::SplAtOneMeter { db_spl } => Some(db_spl),
            ReferenceLevel::CreativeDb { .. } => None,
        };
        self.sources[source_index] = Some(ControlledSource {
            position: profile.pose.position,
            declared_spl_at_one_meter_db,
            radius_m,
        });
        self.publish()
    }

    pub fn set_source_position(
        &mut self,
        source_index: usize,
        position: EnuVector3,
    ) -> Result<(), OutputSafetyConfigError> {
        if !position.is_finite() {
            return Err(OutputSafetyConfigError::InvalidPosition);
        }
        let Some(source) = self.sources.get_mut(source_index).and_then(Option::as_mut) else {
            return Err(if source_index >= MAX_ACTIVE_SOURCES {
                OutputSafetyConfigError::InvalidSourceIndex
            } else {
                OutputSafetyConfigError::SourceNotConfigured
            });
        };
        source.position = position;
        self.publish()
    }

    pub fn clear_source(&mut self, source_index: usize) {
        if let Some(source) = self.sources.get_mut(source_index) {
            *source = None;
            let _ = self.publish();
        }
    }

    fn publish(&mut self) -> Result<(), OutputSafetyConfigError> {
        self.config.validate()?;
        let mut snapshot = OutputSafetySnapshot {
            source_gains: [1.0; MAX_ACTIVE_SOURCES],
            monitor_gain: db_to_linear(self.config.monitor_gain_db),
        };
        for (index, source) in self.sources.iter().enumerate() {
            let Some(source) = source else {
                continue;
            };
            let Some(declared_spl_db) = source.declared_spl_at_one_meter_db else {
                continue;
            };
            let distance_m = distance(self.listener_position, source.position);
            if !distance_m.is_finite() {
                return Err(OutputSafetyConfigError::InvalidPosition);
            }
            let gain_db = proximity_ceiling_gain_db(
                declared_spl_db,
                distance_m,
                source.radius_m,
                self.config.scene_spl_ceiling_db,
                self.config.scene_spl_knee_width_db,
            );
            snapshot.source_gains[index] = db_to_linear(gain_db);
        }
        self.writer.publish(snapshot);
        Ok(())
    }
}

fn distance(a: EnuVector3, b: EnuVector3) -> f32 {
    let east = a.east_m - b.east_m;
    let north = a.north_m - b.north_m;
    let up = a.up_m - b.up_m;
    (east * east + north * north + up * up).sqrt()
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Monotone, C1 soft ceiling in the scene-SPL domain.
///
/// The knee begins at `ceiling_db - knee_width_db`. Above it, the output
/// approaches `ceiling_db` asymptotically. Its derivative at the join is one,
/// so both value and slope match the identity branch.
#[must_use]
pub fn soft_knee_ceiling_output_db(input_db: f32, ceiling_db: f32, knee_width_db: f32) -> f32 {
    let knee_start_db = ceiling_db - knee_width_db;
    if input_db <= knee_start_db {
        input_db
    } else {
        ceiling_db - knee_width_db * (-(input_db - knee_start_db) / knee_width_db).exp()
    }
}

/// Geometry-keyed source gain for the proximity ceiling.
///
/// The radius bounds the near field: distances inside the finite radiator use
/// its surface distance. The returned pre-propagation gain also cancels any
/// inverse-distance growth inside that radius, leaving a finite, constant
/// surface level instead of a 1/r singularity.
#[must_use]
pub fn proximity_ceiling_gain_db(
    declared_spl_at_one_meter_db: f32,
    distance_m: f32,
    source_radius_m: f32,
    ceiling_db: f32,
    knee_width_db: f32,
) -> f32 {
    let bounded_distance_m = distance_m.max(source_radius_m);
    let predicted_scene_spl_db = declared_spl_at_one_meter_db - 20.0 * bounded_distance_m.log10();
    let raw_free_field_spl_db =
        declared_spl_at_one_meter_db - 20.0 * distance_m.max(MIN_GEOMETRY_DISTANCE_M).log10();
    soft_knee_ceiling_output_db(predicted_scene_spl_db, ceiling_db, knee_width_db)
        - raw_free_field_spl_db
}

/// Additive output-safety telemetry. Peaks are run maxima in linear full scale.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SafetyTelemetry {
    pub proximity_ceiling_engagements: u64,
    pub limiter_engagements: u64,
    pub pre_limiter_peak: f32,
    pub post_limiter_peak: f32,
}

/// Stereo-linked lookahead limiter with a four-times oversampled,
/// 12-tap Blackman-windowed sinc true-peak detector.
///
/// The delay is always present, so non-engaging signals are bit-identical
/// after compensating for the documented 32-sample latency.
pub(crate) struct TruePeakLimiter {
    delay_left: [f32; TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES],
    delay_right: [f32; TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES],
    delay_index: usize,
    delay_filled: usize,
    history_left: [f32; TRUE_PEAK_FILTER_TAPS],
    history_right: [f32; TRUE_PEAK_FILTER_TAPS],
    interpolation: [[f32; TRUE_PEAK_FILTER_TAPS]; TRUE_PEAK_OVERSAMPLE_FACTOR - 1],
    gain: f32,
    release_retention: f32,
    ceiling_linear: f32,
}

impl TruePeakLimiter {
    pub(crate) fn new(sample_rate_hz: u32) -> Self {
        Self {
            delay_left: [0.0; TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES],
            delay_right: [0.0; TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES],
            delay_index: 0,
            delay_filled: 0,
            history_left: [0.0; TRUE_PEAK_FILTER_TAPS],
            history_right: [0.0; TRUE_PEAK_FILTER_TAPS],
            interpolation: interpolation_coefficients(),
            gain: 1.0,
            release_retention: (-1.0 / (sample_rate_hz as f32 * TRUE_PEAK_LIMITER_RELEASE_SECONDS))
                .exp(),
            ceiling_linear: db_to_linear(TRUE_PEAK_LIMITER_CEILING_DBTP),
        }
    }

    pub(crate) fn process_stereo(&mut self, left: f32, right: f32) -> (f32, f32, bool) {
        let delayed_left = self.delay_left[self.delay_index];
        let delayed_right = self.delay_right[self.delay_index];
        self.delay_left[self.delay_index] = left;
        self.delay_right[self.delay_index] = right;
        self.delay_index = (self.delay_index + 1) % TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES;
        if self.delay_filled < TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES {
            self.delay_filled += 1;
        }

        self.history_left.rotate_right(1);
        self.history_right.rotate_right(1);
        self.history_left[0] = left;
        self.history_right[0] = right;
        let mut detected_peak = left.abs().max(right.abs());
        for phase in &self.interpolation {
            let mut interpolated_left = 0.0;
            let mut interpolated_right = 0.0;
            for tap in 0..TRUE_PEAK_FILTER_TAPS {
                interpolated_left += self.history_left[tap] * phase[tap];
                interpolated_right += self.history_right[tap] * phase[tap];
            }
            detected_peak = detected_peak
                .max(interpolated_left.abs())
                .max(interpolated_right.abs());
        }

        let required_gain = if detected_peak > self.ceiling_linear {
            self.ceiling_linear / detected_peak
        } else {
            1.0
        };
        let engaged = required_gain < 1.0;
        if required_gain < self.gain {
            self.gain = required_gain;
        } else {
            let released_gain = 1.0 - (1.0 - self.gain) * self.release_retention;
            self.gain = released_gain.min(required_gain);
        }

        if self.delay_filled < TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES {
            (0.0, 0.0, engaged)
        } else {
            (delayed_left * self.gain, delayed_right * self.gain, engaged)
        }
    }
}

fn interpolation_coefficients() -> [[f32; TRUE_PEAK_FILTER_TAPS]; TRUE_PEAK_OVERSAMPLE_FACTOR - 1] {
    let mut coefficients = [[0.0; TRUE_PEAK_FILTER_TAPS]; TRUE_PEAK_OVERSAMPLE_FACTOR - 1];
    let center = (TRUE_PEAK_FILTER_TAPS as f32 - 1.0) * 0.5;
    for phase_index in 0..TRUE_PEAK_OVERSAMPLE_FACTOR - 1 {
        let fraction = (phase_index + 1) as f32 / TRUE_PEAK_OVERSAMPLE_FACTOR as f32;
        let mut sum = 0.0;
        for tap in 0..TRUE_PEAK_FILTER_TAPS {
            let x = tap as f32 - center + fraction;
            let sinc = if x.abs() < 1.0e-6 {
                1.0
            } else {
                (std::f32::consts::PI * x).sin() / (std::f32::consts::PI * x)
            };
            let window = 0.42
                - 0.5
                    * (2.0 * std::f32::consts::PI * tap as f32
                        / (TRUE_PEAK_FILTER_TAPS - 1) as f32)
                        .cos()
                + 0.08
                    * (4.0 * std::f32::consts::PI * tap as f32
                        / (TRUE_PEAK_FILTER_TAPS - 1) as f32)
                        .cos();
            coefficients[phase_index][tap] = sinc * window;
            sum += coefficients[phase_index][tap];
        }
        for coefficient in &mut coefficients[phase_index] {
            *coefficient /= sum;
        }
    }
    coefficients
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_close(actual: f32, expected: f32, tolerance: f32) {
        assert!(
            (actual - expected).abs() <= tolerance,
            "{actual} was not within {tolerance} of {expected}"
        );
    }

    #[test]
    fn soft_knee_has_stated_location_and_is_c1() {
        let ceiling = 100.0;
        let width = 12.0;
        let knee = ceiling - width;
        assert_eq!(
            soft_knee_ceiling_output_db(knee - 1.0, ceiling, width),
            knee - 1.0
        );
        assert_eq!(soft_knee_ceiling_output_db(knee, ceiling, width), knee);

        let epsilon = 1.0e-3;
        let left_slope = (soft_knee_ceiling_output_db(knee, ceiling, width)
            - soft_knee_ceiling_output_db(knee - epsilon, ceiling, width))
            / epsilon;
        let right_slope = (soft_knee_ceiling_output_db(knee + epsilon, ceiling, width)
            - soft_knee_ceiling_output_db(knee, ceiling, width))
            / epsilon;
        assert_close(left_slope, 1.0, 0.01);
        assert_close(right_slope, 1.0, 0.01);
    }

    #[test]
    fn soft_knee_transfer_is_monotone_everywhere_sampled() {
        let mut previous = soft_knee_ceiling_output_db(-200.0, 100.0, 12.0);
        for step in -1999..=3000 {
            let input = step as f32 * 0.1;
            let output = soft_knee_ceiling_output_db(input, 100.0, 12.0);
            assert!(
                output >= previous,
                "{input} dB mapped below its predecessor"
            );
            previous = output;
        }
    }

    #[test]
    fn approach_never_gets_quieter_and_radius_bounds_the_near_field() {
        let declared = 120.0;
        let radius = 1.0;
        let mut previous_output = f32::NEG_INFINITY;
        for step in (1..=10_000).rev() {
            let distance_m = step as f32 / 1_000.0;
            let raw_free_field = declared - 20.0 * distance_m.log10();
            let gain = proximity_ceiling_gain_db(declared, distance_m, radius, 100.0, 12.0);
            let output = raw_free_field + gain;
            assert!(output >= previous_output);
            previous_output = output;
        }
        let at_surface =
            declared + proximity_ceiling_gain_db(declared, radius, radius, 100.0, 12.0);
        let center_raw = declared - 20.0 * MIN_GEOMETRY_DISTANCE_M.log10();
        let at_center = center_raw + proximity_ceiling_gain_db(declared, 0.0, radius, 100.0, 12.0);
        assert_close(at_center, at_surface, 1.0e-5);
    }

    #[test]
    fn limiter_is_bit_transparent_below_threshold_after_documented_latency() {
        let mut limiter = TruePeakLimiter::new(48_000);
        let input: Vec<f32> = (0..256)
            .map(|index| ((index as f32 * 0.071).sin()) * 0.1)
            .collect();
        let mut output = Vec::with_capacity(input.len());
        for sample in &input {
            output.push(limiter.process_stereo(*sample, -*sample).0);
        }
        for index in TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES..input.len() {
            assert_eq!(
                output[index].to_bits(),
                input[index - TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES].to_bits()
            );
        }
    }

    #[test]
    fn control_publication_delivers_geometry_and_monitor_targets() {
        let (mut control, mut reader) =
            OutputSafetyPublication::new(OutputSafetyConfig::default()).unwrap();
        let profile = SourceProfile {
            id: fightbox_api::SourceId::new("safety-test"),
            pose: fightbox_api::Pose {
                position: EnuVector3::new(0.0, 0.0, 0.0),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            reference_level: ReferenceLevel::SplAtOneMeter { db_spl: 120.0 },
            asset_analysis: fightbox_api::AssetAnalysis::new(
                -24.0,
                -12.0,
                fightbox_api::AssetMeasurementProvenance::new("safety-test/v1").unwrap(),
            )
            .unwrap(),
            extent: fightbox_api::ExtentDescriptor::Point,
            directivity: fightbox_api::Directivity::default(),
            max_speed_mps: 0.0,
        };
        control.set_source(0, &profile, None).unwrap();
        let creative_profile = SourceProfile {
            id: fightbox_api::SourceId::new("creative-safety-test"),
            reference_level: ReferenceLevel::CreativeDb { db: 0.0 },
            ..profile.clone()
        };
        control.set_source(1, &creative_profile, None).unwrap();
        control
            .set_listener_position(EnuVector3::new(1.0, 0.0, 0.0))
            .unwrap();
        control.set_monitor_gain_db(-6.0).unwrap();
        let (source_gains, monitor_gain) = reader.read();
        assert!(source_gains[0] < 1.0);
        assert_eq!(source_gains[1], 1.0);
        assert_close(monitor_gain, db_to_linear(-6.0), 1.0e-6);
    }

    #[test]
    fn limiter_contains_hot_signal_and_reports_engagement() {
        let mut limiter = TruePeakLimiter::new(48_000);
        let ceiling = db_to_linear(TRUE_PEAK_LIMITER_CEILING_DBTP);
        let mut engaged = false;
        let mut post_peak = 0.0_f32;
        for _ in 0..512 {
            let (left, right, sample_engaged) = limiter.process_stereo(2.0, -2.0);
            engaged |= sample_engaged;
            post_peak = post_peak.max(left.abs()).max(right.abs());
        }
        assert!(engaged);
        assert!(post_peak <= ceiling + 1.0e-6);
    }

    #[test]
    fn oversampled_detector_catches_an_intersample_peak() {
        let mut limiter = TruePeakLimiter::new(48_000);
        let ceiling = db_to_linear(TRUE_PEAK_LIMITER_CEILING_DBTP);
        let sample_peak = 0.88_f32;
        assert!(sample_peak < ceiling);
        let mut engaged = false;
        for index in 0..128 {
            let sample = if index % 2 == 0 {
                sample_peak
            } else {
                -sample_peak
            };
            engaged |= limiter.process_stereo(sample, sample).2;
        }
        assert!(engaged);
    }
}
