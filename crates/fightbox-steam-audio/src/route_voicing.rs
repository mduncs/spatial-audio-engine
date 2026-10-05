//! Air absorption and time of flight for routed (baked-pathing) sound.
//!
//! Steam's pathing output carries neither air absorption nor a route length.
//! For air, the length is estimated from the path's omni gain, which Steam's
//! inverse-distance model sets to `1 / L`; that estimate voices the EQ where
//! the pathing pass is published and never drives timing. Timing follows only
//! a host-validated route length, through a second read head on the source's
//! propagation delay line.

#[cfg(test)]
use fightbox_runtime::FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M;

use crate::motion_smoothing::{
    MAX_PROPAGATION_DISTANCE_METERS, PROPAGATION_SLEW_TIME_SECONDS,
    SPEED_OF_SOUND_METERS_PER_SECOND,
};
use crate::propagation_delay::{MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE, PropagationDelayLine};

/// Steam Audio's orthonormal zeroth-order SH basis value: a single unit path
/// projects to this W coefficient.
pub(crate) const PATH_SH_Y00: f32 = 0.282_094_8;

/// Lowest band gain relative to the loudest, as Steam's direct-path air EQ
/// enforces (`EQEffect::normalizeGains`). A deeper cut makes its 8 kHz shelf
/// leak down through the mids, and routed and line-of-sight sound keep the
/// same timbre range.
const MIN_RELATIVE_BAND_GAIN: f64 = 0.0625;

/// Lowest loudest-band gain handed to Steam's un-normalized EQ. Its RBJ bands
/// divide by `sqrt(gain)`: a zero mid band makes NaN coefficients that poison
/// the pathing stage for good.
const MIN_PEAK_BAND_GAIN: f64 = 1.0e-6;

/// Fade between two pathing read heads on a route change.
const ROUTE_CROSSFADE_SECONDS: f32 = 0.090;

/// A published length step this large is a route change even when the host
/// keeps the route's vertices.
const ROUTE_JUMP_METERS: f32 = 3.0;

/// Two heads closer than this carry nearly the same signal, so they fade at
/// equal gain; an equal-power fade would swell them by up to 3 dB.
const CORRELATED_HEADS_METERS: f32 = 0.25;

/// Pressure gain per band over `length_m` of air.
pub(crate) fn air_gain(length_m: f64, exponents: [f32; 3]) -> [f64; 3] {
    exponents.map(|exponent| (-f64::from(exponent) * length_m.max(0.0)).exp())
}

/// Band gains Steam's un-normalized EQ realizes with finite filters: the
/// loudest at least [`MIN_PEAK_BAND_GAIN`], none below
/// [`MIN_RELATIVE_BAND_GAIN`] of it, and a non-finite band at the floor.
pub(crate) fn realizable_eq(gains: [f64; 3]) -> [f32; 3] {
    let peak = gains
        .into_iter()
        .filter(|gain| gain.is_finite())
        .fold(MIN_PEAK_BAND_GAIN, f64::max);
    gains.map(|gain| gain.max(MIN_RELATIVE_BAND_GAIN * peak).min(peak) as f32)
}

/// One source's estimated route length, held through publications whose omni
/// gain cannot be inverted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RouteAirVoicing {
    held_length_m: Option<f32>,
}

impl RouteAirVoicing {
    /// Steam's path EQ times air absorption over the estimated route length,
    /// which stays between the straight line and the propagation horizon.
    pub(crate) fn voice(&mut self, steam_eq: [f32; 3], path_sh0: f32, straight_m: f32, exponents: [f32; 3]) -> [f32; 3] {
        let horizon = MAX_PROPAGATION_DISTANCE_METERS;
        let straight_m = if straight_m.is_finite() {
            straight_m.clamp(0.0, horizon)
        } else {
            0.0
        };
        // A gain below one horizon's worth is a coverage gap, not distance.
        if path_sh0.is_finite() && path_sh0 * horizon >= PATH_SH_Y00 {
            self.held_length_m = Some(PATH_SH_Y00 / path_sh0);
        }
        let length_m = self
            .held_length_m
            .unwrap_or(straight_m)
            .clamp(straight_m, horizon);
        let air = air_gain(f64::from(length_m), exponents);
        realizable_eq(std::array::from_fn(|band| {
            f64::from(steam_eq[band]) * air[band]
        }))
    }

    pub(crate) fn reset(&mut self) {
        self.held_length_m = None;
    }
}

/// A host-validated primary route, as a delay-line target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RouteDelayTarget {
    pub(crate) delay_samples: f32,
    pub(crate) topology_id: u64,
}

impl RouteDelayTarget {
    pub(crate) fn from_route(route: crate::PrimaryRoute, sample_rate_hz: i32) -> Self {
        Self {
            delay_samples: route.length_m * sample_rate_hz as f32
                / SPEED_OF_SOUND_METERS_PER_SECOND,
            topology_id: route.topology_id,
        }
    }
}

/// The pathing stage's own read head on a source's propagation delay line.
///
/// Direct and reflections stay on the line's straight-line head. Behind a
/// host route the pathing stage reads `L / c` instead: within one route the
/// head follows the published length through the line's 80 ms one-pole, so
/// walking keeps its physical Doppler, while taking up or leaving a route, a
/// change of route topology, or a length jump fades two heads instead of
/// gliding across the gap. A change arriving mid-fade waits for that fade to
/// finish; the route is read afresh each block, so the latest one wins.
/// Without a route the stage hears the line's own head. Allocation- and
/// lock-free.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteReadHead {
    engaged: bool,
    /// Whether the line has carried sound since the history became untrusted.
    /// Until it has, a route is adopted outright: fading from the silent
    /// straight-line head would only expose a false early arrival.
    line_heard: bool,
    topology_id: u64,
    target_samples: f32,
    applied_samples: f32,
    /// Frozen delay of the outgoing head while a fade runs; `None` is the
    /// line's own head.
    outgoing_samples: Option<f32>,
    fade_remaining: u32,
    fade_frames: u32,
    fade_equal_gain: bool,
    slew_retention: f32,
    jump_samples: f32,
    correlated_samples: f32,
    /// Leaves one block of read-back below the line's horizon.
    maximum_samples: f32,
    /// Samples written since the line's history last became untrusted.
    history_samples: usize,
}

impl RouteReadHead {
    pub(crate) fn new(
        maximum_delay_samples: usize,
        frame_size: usize,
        sample_rate_hz: i32,
    ) -> Self {
        let sample_rate = sample_rate_hz as f32;
        let samples_per_meter = sample_rate / SPEED_OF_SOUND_METERS_PER_SECOND;
        Self {
            engaged: false,
            line_heard: false,
            topology_id: 0,
            target_samples: 0.0,
            applied_samples: 0.0,
            outgoing_samples: None,
            fade_remaining: 0,
            fade_frames: (ROUTE_CROSSFADE_SECONDS * sample_rate).ceil().max(1.0) as u32,
            fade_equal_gain: false,
            slew_retention: (-1.0 / (PROPAGATION_SLEW_TIME_SECONDS * sample_rate)).exp(),
            jump_samples: ROUTE_JUMP_METERS * samples_per_meter,
            correlated_samples: CORRELATED_HEADS_METERS * samples_per_meter,
            maximum_samples: maximum_delay_samples.saturating_sub(frame_size) as f32,
            history_samples: 0,
        }
    }

    /// The pathing stage must read [`Self::process`] rather than the line.
    pub(crate) fn is_heard(&self) -> bool {
        self.engaged || self.fade_remaining > 0
    }

    /// Mirrors [`PropagationDelayLine::invalidate`]: the next route is taken
    /// up afresh and stale history stays silent.
    pub(crate) fn invalidate(&mut self) {
        self.engaged = false;
        self.line_heard = false;
        self.fade_remaining = 0;
        self.history_samples = 0;
    }

    /// Takes this block's route once the line has produced `line_output`;
    /// `line_delay_samples` is where the line's own head now reads.
    pub(crate) fn observe(
        &mut self,
        route: Option<RouteDelayTarget>,
        line_delay_samples: f32,
        line_output: &[f32],
    ) {
        self.history_samples = self.history_samples.saturating_add(line_output.len());
        let line_was_heard = self.line_heard;
        self.line_heard = line_was_heard || line_output.iter().any(|sample| *sample != 0.0);
        let fading = self.fade_remaining > 0;
        let line_delay = self.clamp(line_delay_samples);
        let Some(route) = route.filter(|route| route.delay_samples.is_finite()) else {
            if self.engaged && !fading {
                self.engaged = false;
                self.begin_fade(
                    Some(self.applied_samples),
                    self.applied_samples - line_delay,
                );
            }
            return;
        };
        let target = self.clamp(route.delay_samples);
        if !self.engaged {
            if fading {
                return;
            }
            self.engaged = true;
            if line_was_heard {
                self.begin_fade(None, target - line_delay);
            }
            self.applied_samples = target;
        } else if route.topology_id != self.topology_id
            || (target - self.target_samples).abs() > self.jump_samples
        {
            if fading {
                return;
            }
            self.begin_fade(Some(self.applied_samples), target - self.applied_samples);
            self.applied_samples = target;
        }
        self.topology_id = route.topology_id;
        self.target_samples = target;
    }

    /// One pathing sample, `behind_samples` before the line's newest write;
    /// `line_output` is the line's own output for that sample.
    pub(crate) fn process(
        &mut self,
        line: &PropagationDelayLine,
        line_output: f32,
        behind_samples: usize,
    ) -> f32 {
        let incoming = if self.engaged {
            let requested = self.target_samples
                + (self.applied_samples - self.target_samples) * self.slew_retention;
            self.applied_samples += (requested - self.applied_samples).clamp(
                -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            );
            self.read(line, self.applied_samples, behind_samples)
        } else {
            line_output
        };
        if self.fade_remaining == 0 {
            return incoming;
        }
        let outgoing = match self.outgoing_samples {
            Some(delay) => self.read(line, delay, behind_samples),
            None => line_output,
        };
        let progress = (self.fade_frames - self.fade_remaining) as f32 / self.fade_frames as f32;
        self.fade_remaining -= 1;
        if self.fade_equal_gain {
            incoming * progress + outgoing * (1.0 - progress)
        } else {
            let angle = progress * core::f32::consts::FRAC_PI_2;
            incoming * angle.sin() + outgoing * angle.cos()
        }
    }

    /// Only ever called with no fade in flight, so the outgoing head is the
    /// whole audible signal.
    fn begin_fade(&mut self, outgoing_samples: Option<f32>, separation_samples: f32) {
        self.outgoing_samples = outgoing_samples;
        self.fade_remaining = self.fade_frames;
        self.fade_equal_gain = separation_samples.abs() <= self.correlated_samples;
    }

    fn clamp(&self, delay_samples: f32) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, self.maximum_samples)
        } else {
            0.0
        }
    }

    fn read(&self, line: &PropagationDelayLine, delay_samples: f32, behind_samples: usize) -> f32 {
        line.read_behind_newest(delay_samples + behind_samples as f32, self.history_samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEAM_EQ: [f32; 3] = [0.93, 0.79, 0.66];

    fn close(actual: [f32; 3], expected: [f32; 3]) -> bool {
        actual
            .into_iter()
            .zip(expected)
            .all(|(actual, expected)| (actual - expected).abs() <= 1.0e-6)
    }

    #[test]
    fn voiced_eq_applies_iso_air_over_the_route_within_steams_eq_range() {
        let mut voicing = RouteAirVoicing::default();
        let voiced = voicing.voice(STEAM_EQ, PATH_SH_Y00 / 559.5, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        let air = air_gain(559.5, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        assert!((air[0] - 0.8657).abs() < 1.0e-3 && (air[1] - 0.4087).abs() < 1.0e-3);
        assert!(air[2] < 1.0e-7);
        let low = STEAM_EQ[0] * air[0] as f32;
        assert!(close(
            voiced,
            [
                low,
                STEAM_EQ[1] * air[1] as f32,
                MIN_RELATIVE_BAND_GAIN as f32 * low
            ]
        ));
        assert_eq!(air_gain(0.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M), [1.0; 3]);
    }

    #[test]
    fn every_eq_handed_to_steam_is_realizable() {
        let floor = (MIN_RELATIVE_BAND_GAIN * MIN_PEAK_BAND_GAIN) as f32;
        assert_eq!(realizable_eq([0.0; 3]), [floor; 3]);
        assert_eq!(
            realizable_eq([f64::NAN, 0.5, f64::INFINITY]),
            [0.5 * MIN_RELATIVE_BAND_GAIN as f32, 0.5, 0.5]
        );
        // Silent Steam output over the full horizon still voices finitely.
        let voiced = RouteAirVoicing::default().voice([0.0; 3], PATH_SH_Y00 / 2_048.0, 1.0e9, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        assert!(
            voiced
                .into_iter()
                .all(|gain| gain.is_finite() && gain > 0.0)
        );
    }

    #[test]
    fn route_length_stays_between_straight_line_and_horizon_and_holds_through_gaps() {
        let mut voicing = RouteAirVoicing::default();
        let straight = voicing.voice(STEAM_EQ, PATH_SH_Y00 / 300.0, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        let routed = voicing.voice(STEAM_EQ, PATH_SH_Y00 / 559.5, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        assert!(close(
            straight,
            RouteAirVoicing::default().voice(STEAM_EQ, 0.0, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M)
        ));
        for gap in [0.0, -1.0e-3, f32::NAN, PATH_SH_Y00 / 4_096.0] {
            assert_eq!(voicing.voice(STEAM_EQ, gap, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M), routed);
        }
        // The held route never undercuts a longer straight line.
        assert_eq!(
            voicing.voice(STEAM_EQ, 0.0, 600.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M),
            RouteAirVoicing::default().voice(STEAM_EQ, 0.0, 600.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M)
        );
        let beyond = voicing.voice(STEAM_EQ, 0.0, f32::INFINITY, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M);
        assert!(beyond.into_iter().all(f32::is_finite));
        voicing.reset();
        assert_eq!(voicing.voice(STEAM_EQ, 0.0, 505.0, FALLBACK_THREE_BAND_AIR_PRESSURE_EXPONENT_PER_M), straight);
    }

    const RATE: i32 = 48_000;
    const FRAME: usize = 256;
    /// 4,800 samples of flight.
    const ROUTE_M: f32 = 34.3;

    fn line_and_head() -> (PropagationDelayLine, RouteReadHead) {
        let mut line = PropagationDelayLine::new(RATE as usize, RATE);
        line.reset_to(100.0);
        (line, RouteReadHead::new(RATE as usize, FRAME, RATE))
    }

    fn route(length_m: f32, topology_id: u64) -> Option<RouteDelayTarget> {
        let route = crate::PrimaryRoute {
            length_m,
            topology_id,
        };
        Some(RouteDelayTarget::from_route(route, RATE))
    }

    /// One block as the graph runs it: the line's own output, then the
    /// pathing stage's input.
    fn block(
        line: &mut PropagationDelayLine,
        head: &mut RouteReadHead,
        route: Option<RouteDelayTarget>,
        input: &[f32],
    ) -> (Vec<f32>, Vec<f32>) {
        let line_out = input
            .iter()
            .map(|sample| line.process_sample(*sample))
            .collect::<Vec<_>>();
        head.observe(route, line.current_delay_samples(), &line_out);
        let path_in = if head.is_heard() {
            line_out
                .iter()
                .enumerate()
                .map(|(frame, output)| head.process(line, *output, input.len() - 1 - frame))
                .collect()
        } else {
            line_out.clone()
        };
        (line_out, path_in)
    }

    fn peak(samples: &[f32]) -> usize {
        (0..samples.len())
            .max_by(|a, b| samples[*a].abs().total_cmp(&samples[*b].abs()))
            .unwrap()
    }

    #[test]
    fn the_pathing_head_plays_the_route_late_and_glides_within_it() {
        let (mut line, mut head) = line_and_head();
        let (line_out, path_in) = block(&mut line, &mut head, None, &[0.25; FRAME]);
        assert!(!head.is_heard());
        assert_eq!(path_in, line_out);
        for _ in 0..20 {
            block(&mut line, &mut head, route(ROUTE_M, 7), &[0.0; FRAME]);
        }
        let mut impulse = [0.0; FRAME];
        impulse[0] = 1.0;
        let (mut line_out, mut path_in) = block(&mut line, &mut head, route(ROUTE_M, 7), &impulse);
        for _ in 0..20 {
            let (line_block, path_block) =
                block(&mut line, &mut head, route(ROUTE_M, 7), &[0.0; FRAME]);
            line_out.extend(line_block);
            path_in.extend(path_block);
        }
        assert_eq!(peak(&line_out), 100);
        assert_eq!(peak(&path_in), 4_800);
        assert!((path_in[4_800] - 1.0).abs() < 1.0e-3);

        // One more metre along the same route slews: walking keeps its Doppler.
        block(&mut line, &mut head, route(ROUTE_M + 1.0, 7), &[0.0; FRAME]);
        assert_eq!(head.fade_remaining, 0);
        let moved = head.applied_samples - 4_800.0;
        assert!(moved > 0.0 && moved <= MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE * FRAME as f32);
    }

    #[test]
    fn a_fresh_route_is_adopted_whole_and_keeps_its_first_sample() {
        let (mut line, mut head) = line_and_head();
        let mut impulse = [0.0; FRAME];
        impulse[0] = 1.0;
        let (mut line_out, mut path_in) = block(&mut line, &mut head, route(ROUTE_M, 7), &impulse);
        assert_eq!(head.fade_remaining, 0);
        for _ in 0..20 {
            let (line_block, path_block) =
                block(&mut line, &mut head, route(ROUTE_M, 7), &[0.0; FRAME]);
            line_out.extend(line_block);
            path_in.extend(path_block);
        }
        // No straight-line copy, and the arrival at the route's integer delay
        // is whole although it is the line's very first sample.
        assert!((line_out[100] - 1.0).abs() < 1.0e-3);
        assert!(path_in[..4_800].iter().all(|sample| *sample == 0.0));
        assert_eq!(path_in[4_800], 1.0);
    }

    #[test]
    fn route_changes_fade_two_heads_without_swelling_correlated_ones() {
        let (mut line, mut head) = line_and_head();
        let ones = [1.0; FRAME];
        for _ in 0..40 {
            block(&mut line, &mut head, route(ROUTE_M, 7), &ones);
        }
        // A new topology at nearly the same length fades at equal gain.
        let (_, path_in) = block(&mut line, &mut head, route(ROUTE_M + 0.1, 8), &ones);
        assert!(head.fade_remaining > 0 && head.fade_equal_gain);
        assert!(path_in.iter().all(|sample| (sample - 1.0).abs() < 1.0e-4));
        // A jump waits for that fade, then fades two distinct heads at equal
        // power.
        let remaining = head.fade_remaining;
        block(&mut line, &mut head, route(ROUTE_M + 5.0, 8), &ones);
        assert!(head.fade_equal_gain && head.fade_remaining == remaining - FRAME as u32);
        while head.fade_remaining > 0 {
            block(&mut line, &mut head, route(ROUTE_M + 5.0, 8), &ones);
        }
        block(&mut line, &mut head, route(ROUTE_M + 5.0, 8), &ones);
        assert!(head.fade_remaining > 0 && !head.fade_equal_gain);
        assert!((head.applied_samples - head.target_samples).abs() < 1.0e-3);
        while head.fade_remaining > 0 {
            block(&mut line, &mut head, route(ROUTE_M + 5.0, 8), &ones);
        }
        // Leaving the route fades back and then hands over the line's head.
        let mut blocks = 0;
        while blocks == 0 || head.is_heard() {
            block(&mut line, &mut head, None, &ones);
            blocks += 1;
        }
        assert_eq!(blocks, 17);
        let (line_out, path_in) = block(&mut line, &mut head, None, &[0.5; FRAME]);
        assert_eq!(path_in, line_out);

        // Stale history after an invalidation stays silent.
        head.invalidate();
        head.observe(
            route(ROUTE_M, 9),
            line.current_delay_samples(),
            &[0.0; FRAME],
        );
        assert!(line.read_behind_newest(4_800.0, usize::MAX) != 0.0);
        assert_eq!(head.read(&line, 4_800.0, 0), 0.0);
    }
}
