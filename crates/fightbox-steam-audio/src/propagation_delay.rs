//! Physical time-of-flight delay, Doppler, and teleport handling for the
//! per-source dry signal.
//!
//! # What this models
//!
//! Sound leaves a source and reaches the listener `distance / 343 m/s` later.
//! Feeding the dry mono stem through a delay line whose length tracks that
//! quantity gives two effects for the price of one:
//!
//! * **Onset latency.** A source 343 m away is heard a full second after it
//!   sounds. This is what makes distant events read as distant even before the
//!   listener has any reverberant cue.
//! * **Doppler.** Nothing computes a pitch ratio here. When the distance
//!   changes continuously the read head glides, and resampling a signal with a
//!   drifting read head *is* a pitch shift. Doppler falls out of the geometry
//!   rather than being applied on top of it.
//!
//! # Which Doppler ratio this produces, exactly
//!
//! The delay applied to the sample leaving at time `n` is derived from the
//! distance *at that same time* — the only distance the simulation has
//! published. The output is therefore `input(n - d(n)/c)`, and differentiating
//! the read index gives a frequency ratio of `1 - v_radial/c`.
//!
//! Textbook Doppler is `1 / (1 + v_radial/c)`, which differs because it
//! evaluates the distance at *emission* time: sound arriving now left when the
//! source was somewhere else. The two agree to first order and diverge by
//! `(v/c)^2`. At 20 m/s, a fast vehicle, that is 0.3% — under six cents, below
//! the threshold of pitch discrimination. At 34.3 m/s it reaches 1%.
//!
//! Closing that gap exactly requires the source's radial velocity (the exact
//! delay is `d/(c + v_radial)`, not `d/c`), which the propagation snapshot does
//! not currently carry. Estimating the velocity instead, by differencing the
//! block-rate target, would be the wrong trade: the target arrives as a 60 Hz
//! staircase sampled at block rate, so the estimate is zero on most blocks and
//! large on the rest, and any error in it lands directly on the delay as
//! audible warble. A stable 1% pitch error beats an unstable exact one.
//!
//! # Why the delay is slewed rather than set
//!
//! The simulated distance arrives as a block-rate staircase. Applying it
//! directly would step the read head once per block and click. The applied
//! delay therefore follows the target through a per-sample one-pole with the
//! same [`PROPAGATION_SLEW_TIME_SECONDS`] constant the acoustic terms use, and
//! is then bounded by [`MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE`].
//!
//! The hard bound matters independently of smoothing: a read head moving
//! faster than the write head reverses the signal, and one moving at an
//! appreciable fraction of the write rate transposes far enough to alias
//! against the interpolator's passband. Bounding the slew at 0.5 samples per
//! sample caps radial velocity at ~171 m/s and the pitch ratio inside
//! `[2/3, 2]`, which is well outside anything a city simulation produces and
//! well inside what four-tap Lagrange interpolation resolves cleanly.
//!
//! # Why teleports crossfade instead of gliding
//!
//! A teleport (the workbench height selector, a source respawn, a listener
//! warp) is a discontinuity, not motion. Slewing across it would be
//! *physically* wrong in an audible way: the glide is a pitch sweep whose
//! depth scales with the jump, so moving a source 200 m sounds like a siren
//! wail rather than like the source now being somewhere else.
//!
//! When one update steps the target by more than
//! [`TELEPORT_DELAY_STEP_SECONDS`], the old delay is handed to a second,
//! frozen read head and the primary head is placed directly at the new delay.
//! The two are crossfaded over [`TELEPORT_CROSSFADE_SECONDS`]. Neither head
//! moves during the fade, so no pitch shift of any kind occurs. The taps are
//! by construction more than the teleport threshold apart in the source
//! history and are therefore effectively uncorrelated, so the fade is
//! equal-power; a linear fade would dip audibly at its midpoint.
//!
//! # Relationship to the pathing and reflection sends
//!
//! `multi_source` feeds the delayed stem to the direct, baked-path, and
//! reflection stages alike, so all three share this one source-distance
//! delay. See the stage-alignment note in `render_source` for the empirical
//! basis and the approximation it accepts.

use crate::motion_smoothing::PROPAGATION_SLEW_TIME_SECONDS;

/// Hard bound on how fast the read head may move, in samples per sample.
///
/// `0.5` corresponds to `|v_radial| <= 171.5 m/s` and a pitch ratio within
/// `[2/3, 2]`. The one-pole normally keeps the slew far below this; the bound
/// exists so that no distance discontinuity smaller than the teleport
/// threshold can still transpose far enough to alias.
pub(crate) const MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE: f32 = 0.5;

/// Target step, in seconds of delay, that is treated as a discontinuity
/// rather than as motion.
///
/// 50 ms is ~17 m of distance change within one update. Sustained motion
/// cannot reach it: at 100 m/s a 128-frame block moves the target by under a
/// millisecond, so only genuine position jumps trip the detector.
pub(crate) const TELEPORT_DELAY_STEP_SECONDS: f32 = 0.050;

/// Length of the equal-power fade between the pre- and post-teleport heads.
pub(crate) const TELEPORT_CROSSFADE_SECONDS: f32 = 0.050;

/// A preallocated fractional delay line with time-of-flight slewing and
/// teleport crossfading.
///
/// Every buffer is sized at construction. `observe_block_target` and
/// `process_sample` allocate nothing, take no locks, and read no clock.
#[derive(Debug)]
pub(crate) struct PropagationDelayLine {
    ring: Vec<f32>,
    write_index: usize,
    maximum_delay_samples: f32,
    /// Delay of the primary read head.
    applied_delay_samples: f32,
    /// Endpoint the primary head slews toward.
    target_delay_samples: f32,
    /// Previous block's raw target, used to separate motion from teleports.
    previous_raw_target_samples: f32,
    /// Frozen delay of the outgoing head while a crossfade runs.
    outgoing_delay_samples: f32,
    crossfade_remaining: u32,
    crossfade_frames: u32,
    /// Per-sample one-pole retention for the delay target.
    slew_retention: f32,
    teleport_threshold_samples: f32,
    initialized: bool,
}

impl PropagationDelayLine {
    /// Builds a line able to hold `maximum_delay_samples` of history.
    pub(crate) fn new(maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        debug_assert!(sample_rate_hz > 0);
        let sample_rate = sample_rate_hz as f32;
        Self {
            // Four guard samples keep all four Lagrange taps distinct, including
            // at the configured maximum delay.
            ring: vec![0.0; maximum_delay_samples.saturating_add(4)],
            write_index: 0,
            maximum_delay_samples: maximum_delay_samples as f32,
            applied_delay_samples: 0.0,
            target_delay_samples: 0.0,
            previous_raw_target_samples: 0.0,
            outgoing_delay_samples: 0.0,
            crossfade_remaining: 0,
            crossfade_frames: (TELEPORT_CROSSFADE_SECONDS * sample_rate).ceil().max(1.0) as u32,
            slew_retention: (-1.0 / (PROPAGATION_SLEW_TIME_SECONDS * sample_rate)).exp(),
            teleport_threshold_samples: TELEPORT_DELAY_STEP_SECONDS * sample_rate,
            initialized: false,
        }
    }

    pub(crate) fn current_delay_samples(&self) -> f32 {
        self.applied_delay_samples
    }

    #[cfg(test)]
    pub(crate) fn is_crossfading(&self) -> bool {
        self.crossfade_remaining > 0
    }

    /// Marks the line as having no trustworthy delay state.
    ///
    /// The next observed target is adopted whole instead of slewed toward,
    /// which is what a deactivated source needs: when it returns it must be
    /// heard at its real distance immediately, not swept in from wherever it
    /// used to be. The ring is deliberately left intact; the caller's
    /// reactivation guard is responsible for suppressing stale history.
    pub(crate) fn invalidate(&mut self) {
        self.initialized = false;
    }

    /// Adopts `delay_samples` instantly and cancels any crossfade.
    pub(crate) fn reset_to(&mut self, delay_samples: f32) {
        let delay = self.clamp_delay(delay_samples);
        self.applied_delay_samples = delay;
        self.target_delay_samples = delay;
        self.previous_raw_target_samples = delay;
        self.outgoing_delay_samples = delay;
        self.crossfade_remaining = 0;
        self.initialized = true;
    }

    /// Supplies this block's raw, unsmoothed time-of-flight target.
    ///
    /// The target must come from the simulated positions rather than from the
    /// acoustic smoother: the smoother would already have turned a teleport
    /// into the very glide this detector exists to prevent.
    pub(crate) fn observe_block_target(&mut self, raw_target_samples: f32) {
        let raw = self.clamp_delay(raw_target_samples);
        if !self.initialized {
            self.reset_to(raw);
            return;
        }
        if (raw - self.previous_raw_target_samples).abs() > self.teleport_threshold_samples {
            // A crossfade already in flight is abandoned rather than layered:
            // its incoming head becomes the new outgoing head, so at most two
            // taps are ever mixed no matter how fast teleports arrive.
            self.outgoing_delay_samples = self.applied_delay_samples;
            self.applied_delay_samples = raw;
            self.crossfade_remaining = self.crossfade_frames;
        }
        self.previous_raw_target_samples = raw;
        self.target_delay_samples = raw;
    }

    /// Processes one sample. Allocation-, lock-, and syscall-free.
    #[must_use]
    pub(crate) fn process_sample(&mut self, input: f32) -> f32 {
        let one_pole = self.target_delay_samples
            + (self.applied_delay_samples - self.target_delay_samples) * self.slew_retention;
        let step = (one_pole - self.applied_delay_samples).clamp(
            -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
        );
        self.applied_delay_samples = self.clamp_delay(self.applied_delay_samples + step);

        self.ring[self.write_index] = input;
        let primary = self.read_at(self.applied_delay_samples);
        let output = if self.crossfade_remaining > 0 {
            let outgoing = self.read_at(self.outgoing_delay_samples);
            let elapsed = self.crossfade_frames - self.crossfade_remaining;
            let progress = elapsed as f32 / self.crossfade_frames as f32;
            let angle = progress * core::f32::consts::FRAC_PI_2;
            self.crossfade_remaining -= 1;
            primary * angle.sin() + outgoing * angle.cos()
        } else {
            primary
        };

        self.write_index += 1;
        if self.write_index == self.ring.len() {
            self.write_index = 0;
        }
        output
    }

    fn clamp_delay(&self, delay_samples: f32) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, self.maximum_delay_samples)
        } else {
            0.0
        }
    }

    /// Reads the ring at a fractional delay behind the current write position.
    ///
    /// Third-order Lagrange over four causal taps: at fractional delays the
    /// newest tap needed is the sample just written, so unlike a Catmull-Rom
    /// kernel centered on the read point it requires no unavailable future
    /// sample for delays between zero and one.
    fn read_at(&self, delay_samples: f32) -> f32 {
        let len = self.ring.len();
        let mut read_position = self.write_index as f32 - delay_samples;
        if read_position < 0.0 {
            read_position += len as f32;
            // A negative offset smaller than half an ulp of the ring length
            // rounds to exactly `len` here; that position is position 0.
            if read_position >= len as f32 {
                read_position = 0.0;
            }
        }
        let center = read_position.floor() as usize;
        let fraction = read_position - center as f32;
        let previous = if center == 0 { len - 1 } else { center - 1 };
        let previous_2 = if previous == 0 { len - 1 } else { previous - 1 };
        let next = if center + 1 == len { 0 } else { center + 1 };

        let x = fraction;
        let x_minus_1 = x - 1.0;
        let x_plus_1 = x + 1.0;
        let x_plus_2 = x + 2.0;
        self.ring[previous_2] * (-(x_plus_1 * x * x_minus_1) / 6.0)
            + self.ring[previous] * ((x_plus_2 * x * x_minus_1) * 0.5)
            + self.ring[center] * (-(x_plus_2 * x_plus_1 * x_minus_1) * 0.5)
            + self.ring[next] * ((x_plus_2 * x_plus_1 * x) / 6.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    const SAMPLE_RATE: i32 = 48_000;

    fn tone(frame: usize, hertz: f32) -> f32 {
        (TAU * hertz * frame as f32 / SAMPLE_RATE as f32).sin()
    }

    /// Counts positive-going zero crossings, which measures frequency without
    /// needing a transform.
    fn zero_crossings(samples: &[f32]) -> usize {
        samples
            .windows(2)
            .filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0)
            .count()
    }

    #[test]
    fn first_target_is_adopted_whole_rather_than_slewed_in() {
        let mut delay = PropagationDelayLine::new(8_192, SAMPLE_RATE);
        delay.observe_block_target(1_234.5);

        assert_eq!(
            delay.current_delay_samples().to_bits(),
            1_234.5_f32.to_bits()
        );
        assert!(!delay.is_crossfading());
    }

    #[test]
    fn continuous_motion_never_trips_the_teleport_detector() {
        // 100 m/s outbound, sampled once per 128-frame block.
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let block_seconds = 128.0 / SAMPLE_RATE as f32;
        for block in 0..2_000 {
            let distance = 10.0 + 100.0 * block as f32 * block_seconds;
            delay.observe_block_target(distance * SAMPLE_RATE as f32 / 343.0);
            for _ in 0..128 {
                let _ = delay.process_sample(0.0);
            }
            assert!(
                !delay.is_crossfading(),
                "sustained motion was misread as a teleport at block {block}"
            );
        }
    }

    #[test]
    fn a_position_jump_crossfades_and_completes_within_its_window() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(100.0);
        for _ in 0..128 {
            let _ = delay.process_sample(0.0);
        }

        // 200 m further out: far beyond the 50 ms threshold.
        let jumped = 200.0 * SAMPLE_RATE as f32 / 343.0;
        delay.observe_block_target(jumped);

        assert!(delay.is_crossfading());
        assert_eq!(
            delay.current_delay_samples().to_bits(),
            jumped.to_bits(),
            "the primary head must be placed at the new delay, not slewed to it"
        );
        let fade_frames = (TELEPORT_CROSSFADE_SECONDS * SAMPLE_RATE as f32).ceil() as usize;
        for _ in 0..fade_frames {
            let _ = delay.process_sample(0.0);
        }
        assert!(
            !delay.is_crossfading(),
            "crossfade outlasted its {fade_frames}-sample window"
        );
    }

    #[test]
    fn teleport_produces_no_pitch_glide_where_a_slew_would() {
        const HERTZ: f32 = 1_000.0;
        const CAPTURE: usize = 24_000;
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(480.0);
        let mut frame = 0_usize;
        // The post-teleport head reads ~8,400 samples back, so the ring needs
        // that much real history before the jump or it would read startup
        // silence and the measurement would be meaningless.
        for _ in 0..16_000 {
            let _ = delay.process_sample(tone(frame, HERTZ));
            frame += 1;
        }

        delay.observe_block_target(60.0 * SAMPLE_RATE as f32 / 343.0);
        let captured: Vec<f32> = (0..CAPTURE)
            .map(|_| {
                let output = delay.process_sample(tone(frame, HERTZ));
                frame += 1;
                output
            })
            .collect();

        // A glide would stretch or compress the tone for as long as it lasted.
        // Both halves of the capture must instead hold the source frequency.
        let expected = HERTZ * CAPTURE as f32 / (2.0 * SAMPLE_RATE as f32);
        let first = zero_crossings(&captured[..CAPTURE / 2]) as f32;
        let second = zero_crossings(&captured[CAPTURE / 2..]) as f32;
        assert!(
            (first - expected).abs() <= 2.0,
            "first half showed {first} crossings against {expected} expected"
        );
        assert!(
            (second - expected).abs() <= 2.0,
            "second half showed {second} crossings against {expected} expected"
        );
    }

    /// Renders a tone against a constant radial velocity and returns the
    /// heard frequency, measured after the slew has reached steady state.
    fn doppler_hz(speed_mps: f32, start_m: f32) -> f32 {
        const HERTZ: f32 = 1_000.0;
        const WARMUP_BLOCKS: usize = 600;
        const CAPTURE_BLOCKS: usize = 1_000;
        let mut delay = PropagationDelayLine::new(600_000, SAMPLE_RATE);
        let block_seconds = 128.0 / SAMPLE_RATE as f32;
        let mut frame = 0_usize;
        let mut captured = Vec::with_capacity(CAPTURE_BLOCKS * 128);

        for block in 0..(WARMUP_BLOCKS + CAPTURE_BLOCKS) {
            let distance = start_m + speed_mps * block as f32 * block_seconds;
            delay.observe_block_target(distance * SAMPLE_RATE as f32 / 343.0);
            for _ in 0..128 {
                let output = delay.process_sample(tone(frame, HERTZ));
                frame += 1;
                if block >= WARMUP_BLOCKS {
                    captured.push(output);
                }
            }
        }
        assert!(
            !delay.is_crossfading(),
            "constant velocity must never be read as a teleport"
        );
        let seconds = captured.len() as f32 / SAMPLE_RATE as f32;
        zero_crossings(&captured) as f32 / seconds
    }

    #[test]
    fn city_speed_motion_matches_the_textbook_doppler_ratio() {
        // 10 m/s is the fast end of what a vehicle or a running listener
        // reaches in the city fixture. At this speed the reception-time model
        // this delay line implements and the emission-time textbook ratio
        // agree to well under a cent.
        const HERTZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 10.0;

        let receding = doppler_hz(SPEED_MPS, 50.0);
        let approaching = doppler_hz(-SPEED_MPS, 400.0);

        let textbook = |speed: f32| HERTZ / (1.0 + speed / 343.0);
        assert!(
            (receding - textbook(SPEED_MPS)).abs() < 1.5,
            "recession measured {receding} Hz against textbook {} Hz",
            textbook(SPEED_MPS)
        );
        assert!(
            (approaching - textbook(-SPEED_MPS)).abs() < 1.5,
            "approach measured {approaching} Hz against textbook {} Hz",
            textbook(-SPEED_MPS)
        );
        assert!(
            receding < HERTZ && approaching > HERTZ,
            "recession {receding} Hz and approach {approaching} Hz did not \
             straddle the source frequency"
        );
    }

    /// Pins the exact model so the documented second-order deviation from
    /// textbook Doppler cannot drift silently.
    #[test]
    fn extreme_speed_follows_the_reception_time_model_within_its_stated_error() {
        const HERTZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 34.3; // exactly c/10

        let measured = doppler_hz(SPEED_MPS, 50.0);

        let model = HERTZ * (1.0 - SPEED_MPS / 343.0);
        let textbook = HERTZ / (1.0 + SPEED_MPS / 343.0);
        assert!(
            (measured - model).abs() < 1.5,
            "measured {measured} Hz against the model's {model} Hz"
        );
        // The whole point of the module note: at c/10 the deviation is ~1%,
        // and it is second order, so it shrinks quadratically below that.
        let deviation = (measured - textbook).abs() / textbook;
        assert!(
            (0.005..0.015).contains(&deviation),
            "deviation from textbook Doppler was {deviation}, outside the \
             documented second-order band"
        );
    }

    #[test]
    fn the_read_head_never_moves_faster_than_the_documented_slew_bound() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(0.0);
        let mut previous = delay.current_delay_samples();

        // Steps just under the teleport threshold are the worst case the
        // slew bound has to absorb: they bypass the crossfade entirely.
        let threshold = TELEPORT_DELAY_STEP_SECONDS * SAMPLE_RATE as f32;
        for step in 1..40 {
            delay.observe_block_target(step as f32 * threshold * 0.99);
            for _ in 0..4_096 {
                let _ = delay.process_sample(0.0);
                let current = delay.current_delay_samples();
                let moved = (current - previous).abs();
                // The bound is on the intended step. Long delays are far from
                // the f32 origin, so representing `applied + step` costs up to
                // half an ulp on top of it.
                let ulp = f32::EPSILON * current.abs().max(1.0);
                assert!(
                    moved <= MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE + ulp,
                    "read head moved {moved} samples in one sample"
                );
                previous = delay.current_delay_samples();
            }
        }
    }

    #[test]
    fn sub_threshold_steps_stay_click_free() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(4_800.0);
        let mut frame = 0_usize;
        let mut previous = 0.0_f32;
        let mut largest_step = 0.0_f32;

        for block in 0..600 {
            // 40 ms of delay added per block: under the 50 ms teleport
            // threshold, so this is handled entirely by slewing.
            if block >= 100 {
                let target = 4_800.0 + (block - 100) as f32 * 0.040 * SAMPLE_RATE as f32;
                delay.observe_block_target(target);
            }
            for _ in 0..128 {
                let output = delay.process_sample(tone(frame, 220.0));
                frame += 1;
                if frame > 1_024 {
                    largest_step = largest_step.max((output - previous).abs());
                }
                previous = output;
            }
        }

        // One sample of a 220 Hz tone advances by ~0.029 at unity rate; the
        // bounded slew may stretch that but must not produce a discontinuity.
        assert!(
            largest_step < 0.05,
            "delay slewing introduced a {largest_step} step"
        );
    }

    #[test]
    fn a_teleport_arriving_mid_crossfade_restarts_cleanly() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(480.0);
        for _ in 0..128 {
            let _ = delay.process_sample(1.0);
        }

        delay.observe_block_target(100.0 * SAMPLE_RATE as f32 / 343.0);
        for _ in 0..256 {
            let _ = delay.process_sample(1.0);
        }
        assert!(delay.is_crossfading());

        delay.observe_block_target(300.0 * SAMPLE_RATE as f32 / 343.0);
        let fade_frames = (TELEPORT_CROSSFADE_SECONDS * SAMPLE_RATE as f32).ceil() as usize;
        let outputs: Vec<f32> = (0..fade_frames + 16)
            .map(|_| delay.process_sample(1.0))
            .collect();

        assert!(outputs.iter().all(|sample| sample.is_finite()));
        assert!(
            !delay.is_crossfading(),
            "the restarted crossfade did not complete within its window"
        );
    }

    #[test]
    fn invalidate_makes_the_next_target_adopt_instantly() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target(480.0);
        for _ in 0..128 {
            let _ = delay.process_sample(0.0);
        }

        delay.invalidate();
        let reactivated = 100.0 * SAMPLE_RATE as f32 / 343.0;
        delay.observe_block_target(reactivated);

        assert_eq!(
            delay.current_delay_samples().to_bits(),
            reactivated.to_bits()
        );
        assert!(
            !delay.is_crossfading(),
            "reactivation must adopt the new delay, not crossfade from the old one"
        );
    }

    #[test]
    fn a_static_source_holds_its_delay_exactly() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let target = 34.3 * SAMPLE_RATE as f32 / 343.0;
        delay.observe_block_target(target);
        for _ in 0..48_000 {
            let _ = delay.process_sample(0.0);
        }

        assert_eq!(delay.current_delay_samples().to_bits(), target.to_bits());
    }

    #[test]
    fn an_impulse_emerges_at_its_time_of_flight() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let distance_m = 343.0;
        delay.observe_block_target(distance_m * SAMPLE_RATE as f32 / 343.0);

        let onset = (0..SAMPLE_RATE as usize + 512)
            .map(|frame| delay.process_sample(if frame == 0 { 1.0 } else { 0.0 }))
            .position(|sample| sample.abs() > 1.0e-7)
            .expect("impulse must emerge within the captured window");

        // 343 m at 343 m/s is exactly one second.
        assert!(
            (SAMPLE_RATE as usize - 2..=SAMPLE_RATE as usize + 1).contains(&onset),
            "onset was {onset} samples"
        );
    }

    #[test]
    fn targets_beyond_the_ring_are_clamped_rather_than_wrapped() {
        let mut delay = PropagationDelayLine::new(1_024, SAMPLE_RATE);
        delay.observe_block_target(f32::INFINITY);
        assert_eq!(delay.current_delay_samples(), 0.0);

        delay.reset_to(50_000.0);
        assert_eq!(delay.current_delay_samples(), 1_024.0);
        assert!(delay.process_sample(1.0).is_finite());
    }
}
