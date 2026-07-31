//! Deterministic block-rate smoothing for backend-private propagation controls.

use crate::SteamVector3;
use crate::backend_snapshot::{MAX_PATH_SH_COEFFS, SteamDirectParams, SteamSourcePropagation};

// Direct occlusion and pathing are simulation-rate controls. An 80 ms
// one-pole time constant removes corner zippering while remaining perceptually
// prompt; block endpoints follow the exponential and the retained Steam Audio
// effects interpolate between the endpoint frames supplied on consecutive
// calls.
pub(crate) const PROPAGATION_SLEW_TIME_SECONDS: f32 = 0.080;
pub(crate) const SPEED_OF_SOUND_METERS_PER_SECOND: f32 = 343.0;
/// Physical-distance cap for backend-owned propagation delay lines.
///
/// 2,048 m covers the probe-volume scale while keeping render state fixed and
/// bounded. Distances beyond the cap retain the maximum causal delay.
pub(crate) const MAX_PROPAGATION_DISTANCE_METERS: f32 = 2_048.0;

pub(crate) fn maximum_propagation_delay_samples(sample_rate_hz: i32) -> usize {
    (MAX_PROPAGATION_DISTANCE_METERS * sample_rate_hz as f32 / SPEED_OF_SOUND_METERS_PER_SECOND)
        .ceil() as usize
}

pub(crate) fn propagation_delay_samples(
    source_position: SteamVector3,
    listener_position: SteamVector3,
    sample_rate_hz: i32,
) -> f32 {
    let x = source_position.x - listener_position.x;
    let y = source_position.y - listener_position.y;
    let z = source_position.z - listener_position.z;
    let distance_meters = (x * x + y * y + z * z)
        .sqrt()
        .min(MAX_PROPAGATION_DISTANCE_METERS);
    distance_meters * sample_rate_hz as f32 / SPEED_OF_SOUND_METERS_PER_SECOND
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SmoothedPropagationTerms {
    pub(crate) source_position: SteamVector3,
    pub(crate) listener_position: SteamVector3,
    pub(crate) direct: SteamDirectParams,
    pub(crate) path_eq: [f32; 3],
    pub(crate) path_sh: [f32; MAX_PATH_SH_COEFFS],
}

impl Default for SmoothedPropagationTerms {
    fn default() -> Self {
        Self {
            source_position: SteamVector3::default(),
            listener_position: SteamVector3::default(),
            direct: SteamDirectParams::default(),
            path_eq: [0.0; 3],
            path_sh: [0.0; MAX_PATH_SH_COEFFS],
        }
    }
}

impl SmoothedPropagationTerms {
    fn from_snapshot(propagation: SteamSourcePropagation, listener_position: SteamVector3) -> Self {
        Self {
            source_position: propagation.source_position,
            listener_position,
            direct: propagation.direct,
            path_eq: propagation.path_eq,
            path_sh: propagation.path_sh,
        }
    }

    fn slew_toward(self, target: Self, retention: f32) -> Self {
        Self {
            source_position: slew_vector(self.source_position, target.source_position, retention),
            listener_position: slew_vector(
                self.listener_position,
                target.listener_position,
                retention,
            ),
            direct: SteamDirectParams {
                distance_attenuation: slew_value(
                    self.direct.distance_attenuation,
                    target.direct.distance_attenuation,
                    retention,
                ),
                air_absorption: slew_array(
                    self.direct.air_absorption,
                    target.direct.air_absorption,
                    retention,
                ),
                directivity: slew_value(
                    self.direct.directivity,
                    target.direct.directivity,
                    retention,
                ),
                occlusion: slew_value(self.direct.occlusion, target.direct.occlusion, retention),
                transmission: slew_array(
                    self.direct.transmission,
                    target.direct.transmission,
                    retention,
                ),
            },
            path_eq: slew_array(self.path_eq, target.path_eq, retention),
            path_sh: slew_array(self.path_sh, target.path_sh, retention),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SourcePropagationSmoother {
    initialized: bool,
    applied: SmoothedPropagationTerms,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PropagationTermBlockRamp {
    start: SmoothedPropagationTerms,
    end: SmoothedPropagationTerms,
}

impl PropagationTermBlockRamp {
    /// Exact endpoint passed to Steam Audio after the backend's linear block
    /// ramp. Its retained effects use the preceding parameter frame as the
    /// interpolation start.
    pub(crate) const fn endpoint(self) -> SmoothedPropagationTerms {
        self.end
    }

    #[cfg(test)]
    fn at_sample(self, frame: usize, block_frames: usize) -> SmoothedPropagationTerms {
        assert!(block_frames > 0 && frame < block_frames);
        let progress = (frame + 1) as f32 / block_frames as f32;
        self.start.slew_toward(self.end, 1.0 - progress)
    }
}

impl SourcePropagationSmoother {
    /// Advances every continuous propagation term to this block's exact
    /// exponential endpoint. The first observed snapshot is adopted verbatim,
    /// avoiding a fade from default values.
    pub(crate) fn advance(
        &mut self,
        propagation: SteamSourcePropagation,
        listener_position: SteamVector3,
        block_retention: f32,
    ) -> PropagationTermBlockRamp {
        debug_assert!(block_retention.is_finite() && (0.0..=1.0).contains(&block_retention));
        let target = SmoothedPropagationTerms::from_snapshot(propagation, listener_position);
        let start;
        if self.initialized {
            start = self.applied;
            self.applied = self.applied.slew_toward(target, block_retention);
        } else {
            self.applied = target;
            self.initialized = true;
            start = target;
        }
        PropagationTermBlockRamp {
            start,
            end: self.applied,
        }
    }

    pub(crate) fn reset(&mut self) {
        self.initialized = false;
    }

    #[cfg(test)]
    pub(crate) const fn applied(&self) -> SmoothedPropagationTerms {
        self.applied
    }
}

#[inline]
fn slew_value(applied: f32, target: f32, retention: f32) -> f32 {
    target + (applied - target) * retention
}

fn slew_array<const N: usize>(applied: [f32; N], target: [f32; N], retention: f32) -> [f32; N] {
    std::array::from_fn(|index| slew_value(applied[index], target[index], retention))
}

fn slew_vector(applied: SteamVector3, target: SteamVector3, retention: f32) -> SteamVector3 {
    SteamVector3::new(
        slew_value(applied.x, target.x, retention),
        slew_value(applied.y, target.y, retention),
        slew_value(applied.z, target.z, retention),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend_snapshot::SteamSourcePropagation;

    fn propagation(value: f32) -> SteamSourcePropagation {
        SteamSourcePropagation {
            source_position: SteamVector3::new(value, value + 1.0, value + 2.0),
            direct: SteamDirectParams {
                distance_attenuation: value,
                air_absorption: [value + 3.0, value + 4.0, value + 5.0],
                directivity: value + 6.0,
                occlusion: value + 7.0,
                transmission: [value + 8.0, value + 9.0, value + 10.0],
            },
            path_eq: [value + 11.0, value + 12.0, value + 13.0],
            path_sh: std::array::from_fn(|index| value + 14.0 + index as f32),
            ..SteamSourcePropagation::default()
        }
    }

    fn assert_bits_eq(left: SmoothedPropagationTerms, right: SmoothedPropagationTerms) {
        let left_values = terms_as_values(left);
        let right_values = terms_as_values(right);
        for (left, right) in left_values.into_iter().zip(right_values) {
            assert_eq!(left.to_bits(), right.to_bits());
        }
    }

    fn terms_as_values(
        terms: SmoothedPropagationTerms,
    ) -> [f32; 2 * 3 + 1 + 3 + 1 + 1 + 3 + 3 + MAX_PATH_SH_COEFFS] {
        let mut values = [0.0; 34];
        let mut index = 0;
        for value in [
            terms.source_position.x,
            terms.source_position.y,
            terms.source_position.z,
            terms.listener_position.x,
            terms.listener_position.y,
            terms.listener_position.z,
            terms.direct.distance_attenuation,
        ]
        .into_iter()
        .chain(terms.direct.air_absorption)
        .chain([terms.direct.directivity, terms.direct.occlusion])
        .chain(terms.direct.transmission)
        .chain(terms.path_eq)
        .chain(terms.path_sh)
        {
            values[index] = value;
            index += 1;
        }
        assert_eq!(index, values.len());
        values
    }

    #[test]
    fn first_snapshot_initializes_every_term_without_a_fade_in() {
        let mut smoother = SourcePropagationSmoother::default();
        let target = propagation(0.125);
        let listener = SteamVector3::new(31.0, 32.0, 33.0);

        let ramp = smoother.advance(target, listener, 0.99);

        assert_bits_eq(
            ramp.endpoint(),
            SmoothedPropagationTerms::from_snapshot(target, listener),
        );
        assert_bits_eq(ramp.at_sample(0, 128), ramp.endpoint());
        assert_bits_eq(ramp.at_sample(127, 128), ramp.endpoint());
    }

    #[test]
    fn every_continuous_term_reaches_the_exact_one_pole_block_endpoint() {
        let mut smoother = SourcePropagationSmoother::default();
        let first = propagation(1.0);
        let target = propagation(11.0);
        let first_listener = SteamVector3::new(2.0, 3.0, 4.0);
        let target_listener = SteamVector3::new(12.0, 13.0, 14.0);
        let retention = 0.75;
        smoother.advance(first, first_listener, retention);

        let ramp = smoother.advance(target, target_listener, retention);
        let expected = SmoothedPropagationTerms::from_snapshot(first, first_listener).slew_toward(
            SmoothedPropagationTerms::from_snapshot(target, target_listener),
            retention,
        );

        assert_bits_eq(ramp.endpoint(), expected);
        assert_bits_eq(smoother.applied(), expected);
    }

    #[test]
    fn consecutive_blocks_are_continuous_and_deterministic() {
        let target = propagation(0.05);
        let listener = SteamVector3::new(-4.0, 2.0, 7.0);
        let block_seconds = 128.0 / 48_000.0;
        let retention = (-block_seconds / PROPAGATION_SLEW_TIME_SECONDS).exp();
        let mut first = SourcePropagationSmoother::default();
        let mut second = SourcePropagationSmoother::default();

        for block in 0..32 {
            let input = if block < 4 { propagation(1.0) } else { target };
            let first_applied = first.advance(input, listener, retention).endpoint();
            let second_applied = second.advance(input, listener, retention).endpoint();
            assert_bits_eq(first_applied, second_applied);
        }

        let expected_occlusion = target.direct.occlusion
            + (propagation(1.0).direct.occlusion - target.direct.occlusion) * retention.powi(28);
        assert_eq!(
            first.applied().direct.occlusion.to_bits(),
            expected_occlusion.to_bits()
        );
    }

    #[test]
    fn binary_corner_target_has_a_bounded_occlusion_and_transmission_step_per_block() {
        let block_seconds = 128.0 / 48_000.0;
        let retention = (-block_seconds / PROPAGATION_SLEW_TIME_SECONDS).exp();
        let maximum_endpoint_step = 1.0 - retention;
        let listener = SteamVector3::default();
        let mut smoother = SourcePropagationSmoother::default();
        let visible = SteamSourcePropagation {
            direct: SteamDirectParams {
                occlusion: 1.0,
                transmission: [1.0; 3],
                ..SteamDirectParams::default()
            },
            ..SteamSourcePropagation::default()
        };
        let occluded = SteamSourcePropagation {
            direct: SteamDirectParams {
                occlusion: 0.0,
                transmission: [0.0; 3],
                ..SteamDirectParams::default()
            },
            ..SteamSourcePropagation::default()
        };
        smoother.advance(visible, listener, retention);

        let mut previous = 1.0;
        for _ in 0..128 {
            let ramp = smoother.advance(occluded, listener, retention);
            let applied = ramp.endpoint().direct;
            let endpoint_step = previous - applied.occlusion;
            assert!(
                endpoint_step >= 0.0 && endpoint_step <= maximum_endpoint_step + f32::EPSILON,
                "occlusion endpoint step {endpoint_step} exceeded {maximum_endpoint_step}"
            );
            for transmission in applied.transmission {
                assert_eq!(transmission.to_bits(), applied.occlusion.to_bits());
            }
            let first_sample_step = previous - ramp.at_sample(0, 128).direct.occlusion;
            assert!(
                first_sample_step <= maximum_endpoint_step / 128.0 + f32::EPSILON,
                "first intra-block occlusion step {first_sample_step} was not bounded"
            );
            previous = applied.occlusion;
        }

        assert!(
            previous < 0.02,
            "80 ms slew did not substantially reach the occluded target: {previous}"
        );
    }

    #[test]
    fn reset_makes_reactivation_initialize_instantly() {
        let mut smoother = SourcePropagationSmoother::default();
        smoother.advance(propagation(1.0), SteamVector3::new(1.0, 2.0, 3.0), 0.9);
        smoother.reset();
        let target = propagation(0.2);
        let listener = SteamVector3::new(4.0, 5.0, 6.0);

        let applied = smoother.advance(target, listener, 0.9).endpoint();

        assert_bits_eq(
            applied,
            SmoothedPropagationTerms::from_snapshot(target, listener),
        );
    }

    #[test]
    fn propagation_delay_converts_distance_to_samples() {
        let delay = propagation_delay_samples(
            SteamVector3::new(34.3, 0.0, 0.0),
            SteamVector3::default(),
            48_000,
        );

        assert!((delay - 4_800.0).abs() < 0.001, "delay was {delay}");
    }

    #[test]
    fn propagation_delay_clamps_at_the_documented_distance_cap() {
        let maximum = maximum_propagation_delay_samples(48_000) as f32;
        let at_cap = propagation_delay_samples(
            SteamVector3::new(MAX_PROPAGATION_DISTANCE_METERS, 0.0, 0.0),
            SteamVector3::default(),
            48_000,
        );
        let beyond_cap = propagation_delay_samples(
            SteamVector3::new(MAX_PROPAGATION_DISTANCE_METERS * 4.0, 0.0, 0.0),
            SteamVector3::default(),
            48_000,
        );

        assert!(at_cap <= maximum);
        assert_eq!(beyond_cap.to_bits(), at_cap.to_bits());
    }

    #[test]
    fn delay_target_uses_the_smoothed_position_endpoint() {
        let mut smoother = SourcePropagationSmoother::default();
        let listener = SteamVector3::default();
        smoother.advance(
            SteamSourcePropagation {
                source_position: SteamVector3::new(1.0, 0.0, 0.0),
                ..SteamSourcePropagation::default()
            },
            listener,
            0.5,
        );
        let smoothed = smoother
            .advance(
                SteamSourcePropagation {
                    source_position: SteamVector3::new(81.0, 0.0, 0.0),
                    ..SteamSourcePropagation::default()
                },
                listener,
                0.5,
            )
            .endpoint();

        assert_eq!(smoothed.source_position.x.to_bits(), 41.0_f32.to_bits());
        let delay =
            propagation_delay_samples(smoothed.source_position, smoothed.listener_position, 48_000);
        let expected = 41.0 * 48_000.0 / SPEED_OF_SOUND_METERS_PER_SECOND;
        assert_eq!(delay.to_bits(), expected.to_bits());
    }

    #[test]
    fn block_ramp_is_linear_continuous_and_lands_exactly_on_the_endpoint() {
        let mut smoother = SourcePropagationSmoother::default();
        let listener = SteamVector3::new(1.0, 2.0, 3.0);
        let first = smoother.advance(propagation(1.0), listener, 0.75);
        let second = smoother.advance(propagation(9.0), listener, 0.75);

        assert_bits_eq(first.endpoint(), second.start);
        let expected_step = (second.end.direct.occlusion - second.start.direct.occlusion) / 128.0;
        assert_eq!(
            second.at_sample(0, 128).direct.occlusion.to_bits(),
            (second.start.direct.occlusion + expected_step).to_bits()
        );
        assert_eq!(
            second.at_sample(63, 128).direct.occlusion.to_bits(),
            (second.start.direct.occlusion + expected_step * 64.0).to_bits()
        );
        assert_bits_eq(second.at_sample(127, 128), second.endpoint());
    }
}
