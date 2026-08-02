//! Wave 12 distance-keyed residual shaping for opt-in impulsive sources.
//!
//! The signed family is two cascaded causal one-pole low-passes,
//! `H(z) = ((1 - p) / (1 - p z^-1))^2`. Distance and cutoff are interpolated
//! in log/log space between the signed knots. The transfer-derived makeup
//! values are positive fixed constants at those knots and form a piecewise
//! log-linear function of the interpolated cutoff. No program, peak, envelope,
//! or preceding block can alter a distance key.

use fightbox_api::ImpulseClass;
use std::f64::consts::TAU;

const STAGE_COUNT: usize = 2;

#[derive(Clone, Copy, Debug)]
struct SignedKnot {
    distance_m: f64,
    cutoff_hz: f64,
    makeup: f64,
}

const ARTILLERY_THUNDER_KNOTS: [SignedKnot; 4] = [
    SignedKnot {
        distance_m: 5.0,
        cutoff_hz: 18_000.0,
        makeup: 1.000_370_416,
    },
    SignedKnot {
        distance_m: 50.0,
        cutoff_hz: 7_500.0,
        makeup: 1.001_510_085,
    },
    SignedKnot {
        distance_m: 200.0,
        cutoff_hz: 2_800.0,
        makeup: 1.004_272_464,
    },
    SignedKnot {
        distance_m: 500.0,
        cutoff_hz: 1_100.0,
        makeup: 1.010_668_403,
    },
];

#[derive(Clone, Copy, Debug)]
struct CurveKnot {
    signed: SignedKnot,
    pole: f64,
    stage_feed: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ImpulseParameters {
    #[cfg(test)]
    pub(crate) cutoff_hz: f64,
    pub(crate) pole: f64,
    pub(crate) stage_feed: f64,
    pub(crate) makeup: f64,
}

impl ImpulseParameters {
    #[cfg(test)]
    pub(crate) fn direct_feed(self) -> f64 {
        self.stage_feed.powi(STAGE_COUNT as i32)
    }

    #[cfg(test)]
    pub(crate) fn residual_gain_at(self, frequency_hz: f64, sample_rate_hz: f64) -> f64 {
        let omega = TAU * frequency_hz / sample_rate_hz;
        let denominator = 1.0 + self.pole * self.pole - 2.0 * self.pole * omega.cos();
        self.makeup * self.stage_feed.powi(2) / denominator
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ImpulseShaper {
    sample_rate_hz: f64,
    knots: [CurveKnot; ARTILLERY_THUNDER_KNOTS.len()],
    states: [f64; STAGE_COUNT],
}

impl ImpulseShaper {
    pub(crate) fn new(class: ImpulseClass, sample_rate_hz: i32) -> Option<Self> {
        match class {
            ImpulseClass::None => return None,
            ImpulseClass::ArtilleryThunder => {}
        }
        debug_assert!(sample_rate_hz > 0);
        let sample_rate_hz = f64::from(sample_rate_hz);
        let knots = ARTILLERY_THUNDER_KNOTS.map(|signed| {
            let pole = pole_for_cutoff(signed.cutoff_hz, sample_rate_hz);
            CurveKnot {
                signed,
                pole,
                stage_feed: 1.0 - pole,
            }
        });
        Some(Self {
            sample_rate_hz,
            knots,
            states: [0.0; STAGE_COUNT],
        })
    }

    pub(crate) fn reset(&mut self) {
        self.states = [0.0; STAGE_COUNT];
    }

    /// Evaluates the immutable curve at one smoothed-distance key.
    ///
    /// Exact knots use their construction-time parameters. Between knots,
    /// cutoff uses log-distance interpolation; pole/feed and the fixed-makeup
    /// interpolation key are then derived from that interpolated cutoff.
    pub(crate) fn parameters_at_distance(&self, distance_m: f32) -> ImpulseParameters {
        let distance_m = f64::from(distance_m.max(0.0));
        if distance_m <= self.knots[0].signed.distance_m {
            return parameters_from_knot(self.knots[0]);
        }
        let last = self.knots[self.knots.len() - 1];
        if distance_m >= last.signed.distance_m {
            return parameters_from_knot(last);
        }

        for pair in self.knots.windows(2) {
            if distance_m == pair[1].signed.distance_m {
                return parameters_from_knot(pair[1]);
            }
            if distance_m < pair[1].signed.distance_m {
                let t = (distance_m.ln() - pair[0].signed.distance_m.ln())
                    / (pair[1].signed.distance_m.ln() - pair[0].signed.distance_m.ln());
                let cutoff_hz =
                    geometric_lerp(pair[0].signed.cutoff_hz, pair[1].signed.cutoff_hz, t);
                let cutoff_t = (cutoff_hz.ln() - pair[0].signed.cutoff_hz.ln())
                    / (pair[1].signed.cutoff_hz.ln() - pair[0].signed.cutoff_hz.ln());
                let makeup = geometric_lerp(pair[0].signed.makeup, pair[1].signed.makeup, cutoff_t);
                let pole = pole_for_cutoff(cutoff_hz, self.sample_rate_hz);
                return ImpulseParameters {
                    #[cfg(test)]
                    cutoff_hz,
                    pole,
                    stage_feed: 1.0 - pole,
                    makeup,
                };
            }
        }
        unreachable!("distance is clamped to the signed knot domain")
    }

    pub(crate) fn process_sample(&mut self, sample: f32, parameters: ImpulseParameters) -> f32 {
        let mut value = f64::from(sample);
        for state in &mut self.states {
            value = parameters.stage_feed * value + parameters.pole * *state;
            *state = value;
        }
        (value * parameters.makeup) as f32
    }
}

fn parameters_from_knot(knot: CurveKnot) -> ImpulseParameters {
    ImpulseParameters {
        #[cfg(test)]
        cutoff_hz: knot.signed.cutoff_hz,
        pole: knot.pole,
        stage_feed: knot.stage_feed,
        makeup: knot.signed.makeup,
    }
}

fn pole_for_cutoff(cutoff_hz: f64, sample_rate_hz: f64) -> f64 {
    (-TAU * cutoff_hz / sample_rate_hz).exp()
}

fn geometric_lerp(left: f64, right: f64, t: f64) -> f64 {
    (left.ln() + t * (right.ln() - left.ln())).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE_HZ: i32 = 48_000;

    fn shaper() -> ImpulseShaper {
        ImpulseShaper::new(ImpulseClass::ArtilleryThunder, SAMPLE_RATE_HZ).unwrap()
    }

    #[test]
    fn none_is_a_structural_bypass_and_signed_knots_are_exact() {
        assert!(ImpulseShaper::new(ImpulseClass::None, SAMPLE_RATE_HZ).is_none());

        let shaper = shaper();
        let signed = [
            (
                5.0_f32,
                18_000.0,
                0.094_780_225,
                0.819_422_841,
                1.000_370_416,
            ),
            (50.0, 7_500.0, 0.374_655_739, 0.391_055_445, 1.001_510_085),
            (200.0, 2_800.0, 0.693_142_868, 0.094_161_300, 1.004_272_464),
            (500.0, 1_100.0, 0.865_896_699, 0.017_983_695, 1.010_668_403),
        ];
        for (distance, cutoff, pole, direct_feed, makeup) in signed {
            let parameters = shaper.parameters_at_distance(distance);
            assert_eq!(parameters.cutoff_hz, cutoff);
            assert!((parameters.pole - pole).abs() < 5.0e-10);
            assert!((parameters.direct_feed() - direct_feed).abs() < 5.0e-10);
            assert_eq!(parameters.makeup, makeup);
        }
    }

    #[test]
    fn curve_is_log_distance_log_cutoff_between_knots() {
        let shaper = shaper();
        let distance = (5.0_f32 * 50.0).sqrt();
        let parameters = shaper.parameters_at_distance(distance);
        let t = (f64::from(distance).ln() - 5.0_f64.ln()) / (50.0_f64.ln() - 5.0_f64.ln());
        let expected_cutoff = geometric_lerp(18_000.0, 7_500.0, t);
        let cutoff_t =
            (expected_cutoff.ln() - 18_000.0_f64.ln()) / (7_500.0_f64.ln() - 18_000.0_f64.ln());
        let expected_makeup = geometric_lerp(1.000_370_416, 1.001_510_085, cutoff_t);

        assert!((parameters.cutoff_hz - expected_cutoff).abs() < 1.0e-8);
        assert!((parameters.makeup - expected_makeup).abs() < 1.0e-12);
    }

    #[test]
    fn fixed_key_is_lti_across_different_program_histories() {
        fn differential_ir(history: &[f32]) -> Vec<f32> {
            let mut with_probe = shaper();
            let mut baseline = shaper();
            let parameters = with_probe.parameters_at_distance(200.0);
            for sample in history.iter().copied() {
                with_probe.process_sample(sample, parameters);
                baseline.process_sample(sample, parameters);
            }
            (0..96)
                .map(|frame| {
                    let probe = if frame == 0 { 0.25 } else { 0.0 };
                    with_probe.process_sample(probe, parameters)
                        - baseline.process_sample(0.0, parameters)
                })
                .collect()
        }

        let quiet_history = vec![0.0; 257];
        let busy_history = (0..257)
            .map(|frame| ((frame as f32 * 0.173).sin() * 0.125) + 0.03125)
            .collect::<Vec<_>>();
        let quiet_ir = differential_ir(&quiet_history);
        let busy_ir = differential_ir(&busy_history);
        for (frame, (quiet, busy)) in quiet_ir.into_iter().zip(busy_ir).enumerate() {
            assert!(
                (quiet - busy).abs() <= 2.0e-8,
                "history changed the fixed-key impulse response at frame {frame}: {quiet} vs {busy}"
            );
        }
    }

    #[test]
    fn isolated_probe_keeps_the_exact_onset_frame() {
        let mut shaper = shaper();
        let parameters = shaper.parameters_at_distance(500.0);
        let input = (0..64)
            .map(|frame| if frame == 17 { 1.0_f32 } else { 0.0 })
            .collect::<Vec<_>>();
        let output = input
            .iter()
            .copied()
            .map(|sample| shaper.process_sample(sample, parameters))
            .collect::<Vec<_>>();

        let input_onset = input.iter().position(|sample| *sample != 0.0).unwrap();
        let output_onset = output.iter().position(|sample| *sample != 0.0).unwrap();
        assert_eq!(output_onset, input_onset);
        assert!(output[input_onset].is_finite() && output[input_onset] > 0.0);
    }
}
