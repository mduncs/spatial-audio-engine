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
//! * **Doppler.** Published source and listener velocities reconstruct the
//!   block-rate geometry between position observations. The read head follows
//!   that geometry at its emission time, so resampling produces the exact
//!   reception-time pitch ratio without applying a second pitch effect on top
//!   of the physical delay.
//!
//! # Which Doppler ratio this produces, exactly
//!
//! Let `G(t) = distance(t) / c` be the same-time geometry delay reconstructed
//! sample by sample from position and velocity publications. Distance, radial
//! velocity, and full relative speed determine the constant-relative-velocity
//! range exactly: `G(t)^2 = G(0)^2 + 2 G(0) u t + w^2 t^2`, where `u = v_r/c`
//! and `w = |v|/c`. The causal delay is the retarded-time solution
//! `D(t) = G(t - D(t))`: the sound heard now reads the geometry at the instant
//! it was emitted. For constant radial velocity `v`, differentiating that
//! equation gives `D' = u / (1 + u)`; the read rate and requested pitch ratio
//! are therefore exactly `r = 1 - D' = 1 / (1 + v/c)`.
//!
//! Non-teleport position residuals are distributed over the following observed
//! interval as a bounded radial-rate correction. They never step geometry
//! history or replace the current retarded target. Between observations, a
//! squared-range recurrence preserves off-axis curvature instead of freezing
//! one scalar radial derivative. Its analytic range remains unclamped beyond
//! the finite delay-line horizon; only recorded history and exposed read-head
//! delay saturate, so an out-of-range source can return inward at the correct
//! time. A velocity corner is consequently encountered only when its
//! emission-time sample becomes due, and changes the delay derivative without
//! stepping the delay itself. Before the ring holds a full causal lookback, a
//! separately retained seed trajectory supplies only the older prehistory;
//! later velocity publications cannot rewrite that already-emitted interval.
//!
//! Velocity is never inferred by differencing block-rate positions. Published
//! finite source and listener velocities are projected onto the source-listener
//! axis. A finite zero radial component remains valid guidance when the full
//! relative speed is nonzero, as at a tangency. Finite zero relative speed
//! takes the exact legacy/static path, as do the explicit position-only entry
//! point and non-finite guidance.
//!
//! # Why the delay still has a slew bound
//!
//! Fast velocity-guided motion normally lands directly on the continuously
//! evolved retarded target. The hard slew cap is only a safety for stale or
//! inconsistent publications. Slow motion enters the fast algorithm at 8 m/s
//! and does not leave it until speed falls to 7 m/s, preventing block-to-block
//! flutter. Position-only callers retain the original per-sample one-pole based
//! on [`PROPAGATION_SLEW_TIME_SECONDS`].
//!
//! The hard bound matters independently of smoothing: a receding read head
//! must never reverse through source history. Recession therefore retains the
//! 0.5-sample ceiling. Approaching motion can advance by up to one delay sample
//! per output sample, for a read rate of two, because a rate-aware 32-tap
//! low-pass interpolator now protects that faster read from folding source
//! energy into the audible band. The separate geometry-rate clamp admits
//! radial velocities through ±171.5 m/s and target ratios in `[2/3, 2]`; full
//! relative speed is capped at `c/2` as well so the off-axis geometry remains a
//! contraction. A line that has never moved stays on the immutable four-tap
//! legacy arithmetic. Once moving audio becomes audible, the filter remains
//! latched through the causal stop corner so that stopping cannot switch
//! interpolators under a live waveform.
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
//! The two are crossfaded over [`TELEPORT_CROSSFADE_SECONDS`]. The outgoing
//! head stays frozen; a velocity-guided incoming head continues building its
//! causal geometry history and following physical motion during the fade, so
//! it cannot begin a delayed catch-up after becoming fully audible. A static
//! incoming head remains frozen exactly as before. The taps are by construction
//! more than the teleport threshold apart in the source history and are
//! therefore effectively uncorrelated, so the fade is equal-power; a linear
//! fade would dip audibly at its midpoint.
//!
//! # Relationship to the pathing and reflection sends
//!
//! `multi_source` feeds the delayed stem to the direct, baked-path, and
//! reflection stages alike, so all three share this one source-distance
//! delay. See the stage-alignment note in `render_source` for the empirical
//! basis and the approximation it accepts.

use crate::motion_smoothing::{
    PROPAGATION_SLEW_TIME_SECONDS, SPEED_OF_SOUND_METERS_PER_SECOND, fast_mover_mode,
};
use std::sync::OnceLock;

/// Requested Doppler ratios outside this interval are saturated before they
/// drive the delay target.
const MIN_DOPPLER_PITCH_RATIO: f32 = 2.0 / 3.0;
const MAX_DOPPLER_PITCH_RATIO: f32 = 2.0;

/// Hard bound on how fast the read head may move, in samples per sample.
///
/// The velocity drive separately clamps its requested ratio to `[2/3, 2]`.
/// This independent bound also covers positional drift correction and any
/// distance discontinuity just below the teleport threshold. Fast guided
/// approaches use the separate bandlimited bound below.
pub(crate) const MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE: f32 = 0.5;

/// A bandlimited approaching read head may advance by up to two source samples
/// per output sample. In delay coordinates that is a minimum step of `-1.0`.
/// Recession retains the existing `+0.5` safety ceiling, so the read head can
/// never reverse through source history.
pub(crate) const MAX_BANDLIMITED_APPROACH_SLEW_SAMPLES_PER_SAMPLE: f32 = 1.0;

const BANDLIMITED_TAPS: usize = 32;
const BANDLIMITED_PHASE_INTERVALS: usize = 256;
const BANDLIMITED_PHASE_LEVELS: usize = BANDLIMITED_PHASE_INTERVALS + 1;
const BANDLIMITED_RATE_INTERVALS: usize = 32;
const BANDLIMITED_RATE_LEVELS: usize = BANDLIMITED_RATE_INTERVALS + 1;
const BANDLIMITED_NEWEST_OFFSET: isize = BANDLIMITED_TAPS as isize / 2;
const BANDLIMITED_FIRST_OFFSET: isize = BANDLIMITED_NEWEST_OFFSET + 1 - BANDLIMITED_TAPS as isize;
/// One 128-frame callback block (2.67 ms at 48 kHz) removes the timbral step
/// between the frozen four-tap reference and the moving polyphase readout.
const BANDLIMITED_TRANSITION_FRAMES: u8 = 128;

static BANDLIMITED_KERNEL_TABLE: OnceLock<Box<[f32]>> = OnceLock::new();

/// Activates the anti-alias readout only when the emission head consumes more
/// than one source sample per output sample. Receding and stationary reads do
/// not fold source bandwidth and must not inherit the symmetric kernel's
/// positive-offset precursor from an earlier approach.
fn update_bandlimited_readout_state(
    fast_motion_guided: bool,
    applied_step: f32,
    active: &mut bool,
    transition_remaining: &mut u8,
) {
    let required = fast_motion_guided && applied_step < 0.0;
    if required && !*active {
        *active = true;
        *transition_remaining = BANDLIMITED_TRANSITION_FRAMES;
    } else if !required {
        *active = false;
        *transition_remaining = 0;
    }
}

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
    history_dirty: bool,
    /// Same-time `distance / c` history, indexed with the audio ring.
    geometry_history: Vec<f32>,
    /// Shared immutable polyphase coefficients initialized on the control
    /// thread by `new`; callback reads never enter `OnceLock`.
    bandlimited_kernel: &'static [f32],
    write_index: usize,
    maximum_delay_samples: f32,
    /// Delay of the primary read head.
    applied_delay_samples: f32,
    /// Endpoint the primary head slews toward.
    target_delay_samples: f32,
    /// Present-time same-time geometry, advanced between publications.
    ///
    /// Fast-motion analytic state may exceed the exposed ring horizon; only
    /// history samples and read-head delays are clamped to that horizon.
    geometry_delay_samples: f64,
    /// `v_radial / c`, in geometry-delay samples per output sample.
    geometry_rate_samples_per_sample: f64,
    /// Unclamped squared geometry for the constant-relative-velocity recurrence.
    geometry_delay_squared_samples: f64,
    /// First forward difference of squared geometry per output sample.
    geometry_squared_step_samples: f64,
    /// Constant second forward difference of squared geometry.
    geometry_squared_second_difference_samples: f64,
    /// Constant-velocity geometry that extends backward before retained history.
    ///
    /// This shadow is seeded when guided history begins and is never
    /// reconfigured by later publications. It therefore cannot retroactively
    /// apply a new velocity to emission times older than the history ring.
    prehistory_delay_squared_samples: f64,
    prehistory_squared_step_samples: f64,
    prehistory_squared_second_difference_samples: f64,
    geometry_history_samples: usize,
    geometry_samples_since_publication: usize,
    retarded_time_guided: bool,
    fast_motion_guided: bool,
    /// Once a real moving read begins, retain the bandlimited presentation
    /// through a later stop so changing interpolators cannot click at the
    /// retarded stop corner. Reset/invalidation clears the latch under silence.
    bandlimited_readout_active: bool,
    bandlimited_transition_remaining: u8,
    outgoing_bandlimited_blend: f32,
    /// A finite-zero observation is filling history with post-stop geometry
    /// while older moving emissions remain causally audible.
    zero_motion_retiring: bool,
    zero_motion_samples: usize,
    /// Pre-change velocity state retained exactly for sub-threshold motion.
    legacy_position_anchor_samples: f32,
    legacy_target_rate_samples_per_sample: f32,
    /// Previous block's uncensored target, used to separate motion from teleports.
    previous_raw_target_samples: f64,
    /// Frozen delay of the outgoing head while a crossfade runs.
    outgoing_delay_samples: f32,
    crossfade_remaining: u32,
    crossfade_frames: u32,
    /// Per-sample one-pole retention for the delay target.
    slew_retention: f32,
    teleport_threshold_samples: f32,
    initialized: bool,
}

/// Persistent storage owned by one legacy-compatible mono propagation line.
///
/// These byte counts are read from the live vector capacities. They exclude
/// allocator bookkeeping, the controller's inline state, and the one shared
/// process-wide polyphase table reported by session telemetry.
#[cfg(feature = "linked-sdk")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct PropagationDelayMemory {
    pub(crate) audio_history_payload_bytes: usize,
    pub(crate) geometry_history_payload_bytes: usize,
    pub(crate) total_heap_payload_bytes: usize,
}

impl PropagationDelayLine {
    /// Builds a line able to hold `maximum_delay_samples` of history.
    pub(crate) fn new(maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        debug_assert!(sample_rate_hz > 0);
        let sample_rate = sample_rate_hz as f32;
        let history_len = delay_history_len(maximum_delay_samples);
        Self {
            // The extra guard keeps every 32-tap moving read distinct at the
            // configured physical horizon. Static reads retain their exact
            // four-tap arithmetic within the same, slightly larger ring.
            ring: vec![0.0; history_len],
            history_dirty: false,
            geometry_history: vec![0.0; history_len],
            bandlimited_kernel: bandlimited_kernel_table(),
            write_index: 0,
            maximum_delay_samples: maximum_delay_samples as f32,
            applied_delay_samples: 0.0,
            target_delay_samples: 0.0,
            geometry_delay_samples: 0.0,
            geometry_rate_samples_per_sample: 0.0,
            geometry_delay_squared_samples: 0.0,
            geometry_squared_step_samples: 0.0,
            geometry_squared_second_difference_samples: 0.0,
            prehistory_delay_squared_samples: 0.0,
            prehistory_squared_step_samples: 0.0,
            prehistory_squared_second_difference_samples: 0.0,
            geometry_history_samples: 0,
            geometry_samples_since_publication: 0,
            retarded_time_guided: false,
            fast_motion_guided: false,
            bandlimited_readout_active: false,
            bandlimited_transition_remaining: 0,
            outgoing_bandlimited_blend: 0.0,
            zero_motion_retiring: false,
            zero_motion_samples: 0,
            legacy_position_anchor_samples: 0.0,
            legacy_target_rate_samples_per_sample: 0.0,
            previous_raw_target_samples: 0.0,
            outgoing_delay_samples: 0.0,
            crossfade_remaining: 0,
            crossfade_frames: (TELEPORT_CROSSFADE_SECONDS * sample_rate).ceil().max(1.0) as u32,
            slew_retention: (-1.0 / (PROPAGATION_SLEW_TIME_SECONDS * sample_rate)).exp(),
            teleport_threshold_samples: TELEPORT_DELAY_STEP_SECONDS * sample_rate,
            initialized: false,
        }
    }

    /// Reports the exact live payload capacities of both retained histories.
    #[cfg(feature = "linked-sdk")]
    pub(crate) fn memory(&self) -> PropagationDelayMemory {
        let audio_history_payload_bytes = self.ring.capacity() * core::mem::size_of::<f32>();
        let geometry_history_payload_bytes =
            self.geometry_history.capacity() * core::mem::size_of::<f32>();
        PropagationDelayMemory {
            audio_history_payload_bytes,
            geometry_history_payload_bytes,
            total_heap_payload_bytes: audio_history_payload_bytes
                .saturating_add(geometry_history_payload_bytes),
        }
    }

    pub(crate) fn current_delay_samples(&self) -> f32 {
        self.applied_delay_samples
    }

    /// Retained input required before a reactivated mono source may expose its
    /// current read plan. The bandlimited kernel reaches farther into the past
    /// than the immutable four-tap static interpolator.
    pub(crate) fn required_reactivation_history_samples(&self) -> usize {
        let guard = if self.bandlimited_readout_active {
            BANDLIMITED_TAPS / 2
        } else {
            2
        };
        self.applied_delay_samples.ceil() as usize + guard
    }

    #[cfg(test)]
    pub(crate) fn current_geometry_delay_samples(&self) -> f32 {
        self.geometry_delay_samples as f32
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
        self.geometry_history_samples = 0;
        self.geometry_samples_since_publication = 0;
        self.bandlimited_readout_active = false;
        self.bandlimited_transition_remaining = 0;
        self.outgoing_bandlimited_blend = 0.0;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
    }

    /// Explicit scene discontinuity; unlike deactivation, no old program
    /// history may survive when the next scene starts.
    pub(crate) fn reset_history(&mut self) {
        if self.history_dirty {
            self.ring.fill(0.0);
            self.history_dirty = false;
        }
        self.write_index = 0;
        self.invalidate();
    }

    /// Adopts `delay_samples` instantly and cancels any crossfade.
    pub(crate) fn reset_to(&mut self, delay_samples: f32) {
        let uncensored_delay = uncensored_geometry_observation_samples(delay_samples);
        let delay = self.clamp_delay(delay_samples);
        self.applied_delay_samples = delay;
        self.target_delay_samples = delay;
        self.reset_geometry_history(delay);
        self.retarded_time_guided = false;
        self.fast_motion_guided = false;
        self.bandlimited_readout_active = false;
        self.bandlimited_transition_remaining = 0;
        self.outgoing_bandlimited_blend = 0.0;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
        self.legacy_position_anchor_samples = delay;
        self.legacy_target_rate_samples_per_sample = 0.0;
        self.previous_raw_target_samples = uncensored_delay;
        self.outgoing_delay_samples = delay;
        self.crossfade_remaining = 0;
        self.initialized = true;
    }

    /// Supplies this block's raw, unsmoothed, uncensored time-of-flight target.
    ///
    /// The target must come from the simulated positions rather than from the
    /// acoustic smoother: the smoother would already have turned a teleport
    /// into the very glide this detector exists to prevent. It must also remain
    /// unclamped beyond the finite ring horizon so two out-of-range positions
    /// retain their distinct teleport identity; exposure is clamped internally.
    pub(crate) fn observe_block_target(&mut self, raw_target_samples: f32) {
        self.observe_block_target_inner(raw_target_samples, None);
    }

    /// Supplies radial velocity plus full relative speed. The latter keeps a
    /// fast tangential source on direct causal tracking even when its radial
    /// projection momentarily crosses zero.
    pub(crate) fn observe_block_target_with_motion(
        &mut self,
        raw_target_samples: f32,
        radial_velocity_mps: f32,
        relative_speed_mps: f32,
    ) {
        let guidance = self.retarded_guidance(radial_velocity_mps, relative_speed_mps);
        self.observe_block_target_inner(raw_target_samples, guidance);
    }

    /// Supplies an explicitly finite-zero relative velocity.
    ///
    /// A line that has never been guided takes the exact position-only path.
    /// After motion, the line instead retains moving emission history until
    /// the stop corner reaches the read head, while publishing static future
    /// geometry without stepping a small observation residual into history.
    pub(crate) fn observe_block_target_with_zero_motion(&mut self, raw_target_samples: f32) {
        let uncensored_raw = uncensored_geometry_observation_samples(raw_target_samples);
        let teleported = self.initialized
            && (uncensored_raw - self.previous_raw_target_samples).abs()
                > f64::from(self.teleport_threshold_samples);
        if !self.initialized
            || !(self.fast_motion_guided || self.zero_motion_retiring)
            || teleported
        {
            self.observe_block_target_inner(raw_target_samples, None);
            return;
        }

        let continuously_corrected = self.geometry_samples_since_publication > 0;
        let evolution_delay = if continuously_corrected {
            self.geometry_delay_samples
        } else {
            uncensored_raw
        };
        let publication_correction_rate = if continuously_corrected {
            (uncensored_raw - evolution_delay) / self.geometry_samples_since_publication as f64
        } else {
            0.0
        };
        let already_retiring = self.zero_motion_retiring;
        self.previous_raw_target_samples = uncensored_raw;
        self.configure_constant_velocity_geometry(
            evolution_delay,
            RetardedGuidance::ZERO,
            publication_correction_rate,
            false,
        );
        self.retarded_time_guided = true;
        self.fast_motion_guided = true;
        self.zero_motion_retiring = true;
        if !already_retiring {
            self.zero_motion_samples = 0;
        }
        let raw = self.clamp_delay(raw_target_samples);
        self.legacy_position_anchor_samples = raw;
        self.legacy_target_rate_samples_per_sample = 0.0;
    }

    fn observe_block_target_inner(
        &mut self,
        raw_target_samples: f32,
        guidance: Option<RetardedGuidance>,
    ) {
        let uncensored_raw = uncensored_geometry_observation_samples(raw_target_samples);
        let raw = self.clamp_delay(raw_target_samples);
        let teleported = self.initialized
            && (uncensored_raw - self.previous_raw_target_samples).abs()
                > f64::from(self.teleport_threshold_samples);
        // An initialized unguided line already describes a static past. Motion
        // begins at this observation; it must not be extrapolated backward
        // across audio that was emitted before the corner.
        let seed_prehistory = !self.initialized || teleported;
        let analytic_raw = uncensored_raw;
        let continuously_corrected = self.initialized
            && self.retarded_time_guided
            && !teleported
            && self.geometry_samples_since_publication > 0;
        let evolution_delay = if continuously_corrected {
            self.geometry_delay_samples
        } else {
            analytic_raw
        };
        let publication_correction_rate = if continuously_corrected {
            (analytic_raw - evolution_delay) / self.geometry_samples_since_publication as f64
        } else {
            0.0
        };
        if !self.initialized {
            self.reset_to(raw);
        } else if teleported {
            // A crossfade already in flight is abandoned rather than layered:
            // its incoming head becomes the new outgoing head, so at most two
            // taps are ever mixed no matter how fast teleports arrive.
            self.outgoing_delay_samples = self.applied_delay_samples;
            self.outgoing_bandlimited_blend = self.bandlimited_blend();
            self.applied_delay_samples = raw;
            self.target_delay_samples = raw;
            self.reset_geometry_history(raw);
            self.crossfade_remaining = self.crossfade_frames;
        }
        self.previous_raw_target_samples = uncensored_raw;
        if let Some(guidance) = guidance {
            self.zero_motion_retiring = false;
            self.zero_motion_samples = 0;
            if !self.retarded_time_guided {
                self.reset_geometry_history(raw);
            }
            self.configure_constant_velocity_geometry(
                evolution_delay,
                guidance,
                publication_correction_rate,
                seed_prehistory,
            );
            self.retarded_time_guided = true;
            self.fast_motion_guided =
                fast_mover_mode(self.fast_motion_guided, guidance.relative_speed_mps);
            self.legacy_position_anchor_samples = self.clamp_delay(raw * guidance.pitch_ratio);
            self.legacy_target_rate_samples_per_sample = (1.0 - guidance.pitch_ratio).clamp(
                -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            );
            if teleported && self.fast_motion_guided {
                let incoming_delay = self.prehistory_retarded_delay_samples();
                self.applied_delay_samples = incoming_delay;
                self.target_delay_samples = incoming_delay;
            }
        } else {
            self.zero_motion_retiring = false;
            self.zero_motion_samples = 0;
            self.target_delay_samples = raw;
            self.reset_geometry_history(raw);
            self.retarded_time_guided = false;
            self.fast_motion_guided = false;
            self.legacy_position_anchor_samples = raw;
            self.legacy_target_rate_samples_per_sample = 0.0;
        }
    }

    /// Processes one sample. Allocation-, lock-, and syscall-free.
    #[must_use]
    pub(crate) fn process_sample(&mut self, input: f32) -> f32 {
        let advance_retarded_state = self.retarded_time_guided;
        let mut retire_zero_motion_after_sample = false;
        let previous_applied_delay = self.applied_delay_samples;
        if self.retarded_time_guided {
            self.record_geometry_sample();
            if self.fast_motion_guided {
                self.target_delay_samples = self.retarded_delay_samples();
                retire_zero_motion_after_sample = self.zero_motion_retiring
                    && self.target_delay_samples.ceil() as usize <= self.zero_motion_samples;
            } else {
                self.legacy_position_anchor_samples = self.clamp_delay(
                    self.legacy_position_anchor_samples
                        + self.legacy_target_rate_samples_per_sample,
                );
                let integrated_target = self.clamp_delay(
                    self.target_delay_samples + self.legacy_target_rate_samples_per_sample,
                );
                self.target_delay_samples = self.clamp_delay(
                    self.legacy_position_anchor_samples
                        + (integrated_target - self.legacy_position_anchor_samples)
                            * self.slew_retention,
                );
            }
        }
        if self.crossfade_remaining == 0 || self.retarded_time_guided {
            let requested = if self.retarded_time_guided && self.fast_motion_guided {
                self.target_delay_samples
            } else {
                self.target_delay_samples
                    + (self.applied_delay_samples - self.target_delay_samples) * self.slew_retention
            };
            let minimum_step = if self.fast_motion_guided {
                -MAX_BANDLIMITED_APPROACH_SLEW_SAMPLES_PER_SAMPLE
            } else {
                -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE
            };
            let step = (requested - self.applied_delay_samples)
                .clamp(minimum_step, MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE);
            self.applied_delay_samples = self.clamp_delay(self.applied_delay_samples + step);
        }
        let applied_step = self.applied_delay_samples - previous_applied_delay;
        update_bandlimited_readout_state(
            self.fast_motion_guided,
            applied_step,
            &mut self.bandlimited_readout_active,
            &mut self.bandlimited_transition_remaining,
        );
        let primary_bandlimited_blend = self.bandlimited_blend();
        let primary_read_rate =
            (1.0 - applied_step).clamp(MIN_DOPPLER_PITCH_RATIO, MAX_DOPPLER_PITCH_RATIO);

        self.ring[self.write_index] = input;
        self.history_dirty |= input.to_bits() != 0;
        let primary = self.read_at_variable_rate(
            self.applied_delay_samples,
            primary_read_rate,
            primary_bandlimited_blend,
        );
        let output = if self.crossfade_remaining > 0 {
            let outgoing = self.read_at_variable_rate(
                self.outgoing_delay_samples,
                1.0,
                self.outgoing_bandlimited_blend,
            );
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
        if advance_retarded_state {
            if self.fast_motion_guided {
                self.advance_constant_velocity_geometry();
            } else {
                self.geometry_delay_samples = self.clamp_geometry_delay(
                    self.geometry_delay_samples + self.geometry_rate_samples_per_sample,
                );
            }
            self.advance_prehistory_geometry();
            self.geometry_samples_since_publication =
                self.geometry_samples_since_publication.saturating_add(1);
            if self.zero_motion_retiring {
                self.zero_motion_samples = self.zero_motion_samples.saturating_add(1);
            }
        }
        if retire_zero_motion_after_sample {
            self.finish_zero_motion_retirement();
        }
        self.advance_bandlimited_transition();
        output
    }

    /// Reads `delay_samples` behind the newest written sample, for a second
    /// head that follows the same history after `process_sample` returns.
    /// Only the newest `history_samples` writes exist for that head: older
    /// interpolation taps read as silence.
    #[cfg(any(feature = "linked-sdk", test))]
    pub(crate) fn read_behind_newest(&self, delay_samples: f32, history_samples: usize) -> f32 {
        let len = self.ring.len();
        let newest = self.write_index.checked_sub(1).unwrap_or(len - 1);
        let (taps, weights) = self.lagrange_taps(self.clamp_delay(delay_samples) + 1.0);
        taps.into_iter()
            .zip(weights)
            .map(|(tap, weight)| {
                if (newest + len - tap) % len < history_samples {
                    self.ring[tap] * weight
                } else {
                    0.0
                }
            })
            .sum()
    }

    fn clamp_delay(&self, delay_samples: f32) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, self.maximum_delay_samples)
        } else {
            0.0
        }
    }

    fn clamp_geometry_delay(&self, delay_samples: f64) -> f64 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, f64::from(self.maximum_delay_samples))
        } else {
            0.0
        }
    }

    fn expose_geometry_delay(&self, delay_samples: f64) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, f64::from(self.maximum_delay_samples)) as f32
        } else {
            0.0
        }
    }

    fn retarded_guidance(
        &self,
        radial_velocity_mps: f32,
        relative_speed_mps: f32,
    ) -> Option<RetardedGuidance> {
        if !radial_velocity_mps.is_finite()
            || !relative_speed_mps.is_finite()
            || relative_speed_mps <= 0.0
        {
            return None;
        }
        let denominator = 1.0 + radial_velocity_mps / SPEED_OF_SOUND_METERS_PER_SECOND;
        let unbounded_pitch_ratio = if denominator > 0.0 {
            denominator.recip()
        } else {
            MAX_DOPPLER_PITCH_RATIO
        };
        let pitch_ratio =
            unbounded_pitch_ratio.clamp(MIN_DOPPLER_PITCH_RATIO, MAX_DOPPLER_PITCH_RATIO);
        Some(RetardedGuidance {
            geometry_rate_samples_per_sample: (f64::from(radial_velocity_mps)
                / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND))
            .clamp(-0.5, 0.5),
            geometry_speed_samples_per_sample: (f64::from(relative_speed_mps)
                / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND))
            .clamp(0.0, 0.5),
            relative_speed_mps,
            pitch_ratio,
        })
    }

    fn reset_geometry_history(&mut self, delay_samples: f32) {
        let delay = self.clamp_delay(delay_samples);
        self.geometry_delay_samples = f64::from(delay);
        self.geometry_rate_samples_per_sample = 0.0;
        self.geometry_delay_squared_samples = self.geometry_delay_samples.powi(2);
        self.geometry_squared_step_samples = 0.0;
        self.geometry_squared_second_difference_samples = 0.0;
        self.prehistory_delay_squared_samples = self.geometry_delay_squared_samples;
        self.prehistory_squared_step_samples = 0.0;
        self.prehistory_squared_second_difference_samples = 0.0;
        self.geometry_history_samples = 0;
        self.geometry_samples_since_publication = 0;
    }

    fn configure_constant_velocity_geometry(
        &mut self,
        delay_samples: f64,
        guidance: RetardedGuidance,
        publication_correction_rate: f64,
        seed_prehistory: bool,
    ) {
        let delay = delay_samples;
        let radial_rate = (guidance.geometry_rate_samples_per_sample + publication_correction_rate)
            .clamp(-0.5, 0.5);
        let speed_rate = guidance
            .geometry_speed_samples_per_sample
            .max(radial_rate.abs());
        let speed_squared = speed_rate * speed_rate;
        self.geometry_delay_samples = delay;
        self.geometry_delay_squared_samples = delay * delay;
        self.geometry_squared_step_samples = 2.0 * delay * radial_rate + speed_squared;
        self.geometry_squared_second_difference_samples = 2.0 * speed_squared;
        self.geometry_rate_samples_per_sample = radial_rate;
        self.geometry_samples_since_publication = 0;
        if seed_prehistory {
            self.prehistory_delay_squared_samples = self.geometry_delay_squared_samples;
            self.prehistory_squared_step_samples = self.geometry_squared_step_samples;
            self.prehistory_squared_second_difference_samples =
                self.geometry_squared_second_difference_samples;
        }
    }

    fn advance_constant_velocity_geometry(&mut self) {
        self.geometry_delay_squared_samples =
            (self.geometry_delay_squared_samples + self.geometry_squared_step_samples).max(0.0);
        self.geometry_squared_step_samples += self.geometry_squared_second_difference_samples;
        self.geometry_delay_samples = self.geometry_delay_squared_samples.sqrt();
    }

    fn advance_prehistory_geometry(&mut self) {
        self.prehistory_delay_squared_samples =
            (self.prehistory_delay_squared_samples + self.prehistory_squared_step_samples).max(0.0);
        self.prehistory_squared_step_samples += self.prehistory_squared_second_difference_samples;
    }

    fn finish_zero_motion_retirement(&mut self) {
        let stationary_delay = self.expose_geometry_delay(self.geometry_delay_samples);
        self.target_delay_samples = stationary_delay;
        self.legacy_position_anchor_samples = stationary_delay;
        self.legacy_target_rate_samples_per_sample = 0.0;
        self.reset_geometry_history(stationary_delay);
        self.retarded_time_guided = false;
        self.fast_motion_guided = false;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
    }

    fn record_geometry_sample(&mut self) {
        self.geometry_history[self.write_index] =
            self.expose_geometry_delay(self.geometry_delay_samples);
        self.geometry_history_samples = self
            .geometry_history_samples
            .saturating_add(1)
            .min(self.geometry_history.len());
    }

    /// Solves `D(t) = G(t - D(t))` by fixed-point iteration. The published
    /// speed clamp keeps `|G'| <= 0.5`, so this is a contraction. Starting at
    /// the preceding sample's solution makes eight iterations ample even at
    /// the bound, without any data-dependent allocation or synchronization.
    fn retarded_delay_samples(&self) -> f32 {
        let available_lookback = self.geometry_history_samples.saturating_sub(1);
        let mut delay = self.target_delay_samples;
        if delay.ceil() as usize > available_lookback {
            let constant_velocity_delay = self.prehistory_retarded_delay_samples();
            if constant_velocity_delay.ceil() as usize > available_lookback {
                return constant_velocity_delay;
            }
            delay = constant_velocity_delay;
        }

        for _ in 0..8 {
            delay = self.clamp_delay(self.geometry_at_delay(delay));
        }
        delay
    }

    fn prehistory_retarded_delay_samples(&self) -> f32 {
        self.expose_geometry_delay(constant_velocity_retarded_delay_samples(
            self.prehistory_delay_squared_samples,
            self.prehistory_squared_step_samples,
            self.prehistory_squared_second_difference_samples,
        ))
    }

    fn geometry_at_delay(&self, delay_samples: f32) -> f32 {
        let available_lookback = self.geometry_history_samples.saturating_sub(1);
        let delay = self.clamp_delay(delay_samples);
        if delay.ceil() as usize > available_lookback {
            if self.fast_motion_guided {
                let lookback = f64::from(delay);
                let speed_squared = self.prehistory_squared_second_difference_samples * 0.5;
                let squared_derivative = self.prehistory_squared_step_samples - speed_squared;
                let squared_delay = self.prehistory_delay_squared_samples
                    - squared_derivative * lookback
                    + speed_squared * lookback * lookback;
                return self.expose_geometry_delay(squared_delay.max(0.0).sqrt());
            }
            return self.clamp_delay(
                (self.geometry_delay_samples
                    - self.geometry_rate_samples_per_sample * f64::from(delay))
                    as f32,
            );
        }

        let whole = delay.floor() as usize;
        let fraction = delay - whole as f32;
        let newer =
            (self.write_index + self.geometry_history.len() - whole) % self.geometry_history.len();
        let older = if newer == 0 {
            self.geometry_history.len() - 1
        } else {
            newer - 1
        };
        self.geometry_history[newer]
            + (self.geometry_history[older] - self.geometry_history[newer]) * fraction
    }

    fn bandlimited_blend(&self) -> f32 {
        if !self.bandlimited_readout_active {
            return 0.0;
        }
        if self.bandlimited_transition_remaining == 0 {
            return 1.0;
        }
        1.0 - f32::from(self.bandlimited_transition_remaining)
            / f32::from(BANDLIMITED_TRANSITION_FRAMES)
    }

    fn advance_bandlimited_transition(&mut self) {
        self.bandlimited_transition_remaining =
            self.bandlimited_transition_remaining.saturating_sub(1);
    }

    fn read_at_variable_rate(
        &self,
        delay_samples: f32,
        read_rate: f32,
        bandlimited_blend: f32,
    ) -> f32 {
        if bandlimited_blend <= 0.0 || delay_samples < BANDLIMITED_NEWEST_OFFSET as f32 {
            return self.read_at(delay_samples);
        }
        let bandlimited = BandlimitedReadPlan::new(
            self.write_index,
            self.ring.len(),
            delay_samples,
            read_rate,
            self.bandlimited_kernel,
        )
        .apply(&self.ring);
        if bandlimited_blend >= 1.0 {
            bandlimited
        } else {
            let legacy = self.read_at(delay_samples);
            legacy + (bandlimited - legacy) * bandlimited_blend
        }
    }

    /// Reads the ring at a fractional delay behind the current write position.
    ///
    /// Third-order Lagrange over four causal taps: at fractional delays the
    /// newest tap needed is the sample just written, so unlike a Catmull-Rom
    /// kernel centered on the read point it requires no unavailable future
    /// sample for delays between zero and one.
    fn read_at(&self, delay_samples: f32) -> f32 {
        let ([previous_2, previous, center, next], [w_previous_2, w_previous, w_center, w_next]) =
            self.lagrange_taps(delay_samples);
        self.ring[previous_2] * w_previous_2
            + self.ring[previous] * w_previous
            + self.ring[center] * w_center
            + self.ring[next] * w_next
    }

    /// The four ring indices [`Self::read_at`] reads, oldest first, and their
    /// weights.
    fn lagrange_taps(&self, delay_samples: f32) -> ([usize; 4], [f32; 4]) {
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
        (
            [previous_2, previous, center, next],
            [
                -(x_plus_1 * x * x_minus_1) / 6.0,
                (x_plus_2 * x * x_minus_1) * 0.5,
                -(x_plus_2 * x_plus_1 * x_minus_1) * 0.5,
                (x_plus_2 * x_plus_1 * x) / 6.0,
            ],
        )
    }
}

pub(crate) fn delay_history_len(maximum_delay_samples: usize) -> usize {
    maximum_delay_samples
        .saturating_add(BANDLIMITED_TAPS)
        .saturating_add(4)
}

pub(crate) const fn bandlimited_kernel_payload_bytes() -> u64 {
    (BANDLIMITED_RATE_LEVELS
        * BANDLIMITED_PHASE_LEVELS
        * BANDLIMITED_TAPS
        * core::mem::size_of::<f32>()) as u64
}

fn bandlimited_kernel_table() -> &'static [f32] {
    BANDLIMITED_KERNEL_TABLE
        .get_or_init(build_bandlimited_kernel_table)
        .as_ref()
}

/// Builds a control-thread-only 32-tap polyphase bank. Rate and phase are both
/// linearly interpolated on the callback, avoiding cadence-locked coefficient
/// changes while keeping every callback buffer fixed-size.
fn build_bandlimited_kernel_table() -> Box<[f32]> {
    let coefficient_count = BANDLIMITED_RATE_LEVELS * BANDLIMITED_PHASE_LEVELS * BANDLIMITED_TAPS;
    let mut coefficients = Vec::with_capacity(coefficient_count);
    for rate_level in 0..BANDLIMITED_RATE_LEVELS {
        let read_rate = 1.0 + rate_level as f64 / BANDLIMITED_RATE_INTERVALS as f64;
        // A small transition allowance keeps the finite 32-tap kernel away
        // from the fold frequency. At the 167 m/s ratio this retains useful
        // source content through roughly 11 kHz, heard near 21.5 kHz, while
        // placing the high-band alias probe beyond the Blackman-Harris lobe.
        let cutoff_cycles_per_input_sample = 0.45 / read_rate;
        for phase_level in 0..BANDLIMITED_PHASE_LEVELS {
            let fraction = phase_level as f64 / BANDLIMITED_PHASE_INTERVALS as f64;
            let row_start = coefficients.len();
            let mut sum = 0.0_f64;
            for tap in 0..BANDLIMITED_TAPS {
                let offset = BANDLIMITED_FIRST_OFFSET + tap as isize;
                let distance = fraction - offset as f64;
                let lowpass = if distance.abs() < f64::EPSILON {
                    2.0 * cutoff_cycles_per_input_sample
                } else {
                    (2.0 * core::f64::consts::PI * cutoff_cycles_per_input_sample * distance).sin()
                        / (core::f64::consts::PI * distance)
                };
                let window_phase =
                    2.0 * core::f64::consts::PI * tap as f64 / (BANDLIMITED_TAPS - 1) as f64;
                let window = 0.358_75 - 0.488_29 * window_phase.cos()
                    + 0.141_28 * (2.0 * window_phase).cos()
                    - 0.011_68 * (3.0 * window_phase).cos();
                let coefficient = lowpass * window;
                coefficients.push(coefficient as f32);
                sum += coefficient;
            }
            debug_assert!(sum.is_finite() && sum.abs() > f64::EPSILON);
            for coefficient in &mut coefficients[row_start..] {
                *coefficient = (f64::from(*coefficient) / sum) as f32;
            }
        }
    }
    debug_assert_eq!(coefficients.len(), coefficient_count);
    coefficients.into_boxed_slice()
}

#[derive(Clone, Copy, Debug)]
struct BandlimitedReadPlan {
    tap_indices: [usize; BANDLIMITED_TAPS],
    tap_weights: [f32; BANDLIMITED_TAPS],
    fraction: f32,
    required_history_samples: usize,
}

impl BandlimitedReadPlan {
    fn new(
        write_index: usize,
        history_len: usize,
        delay_samples: f32,
        read_rate: f32,
        kernel: &[f32],
    ) -> Self {
        debug_assert_eq!(
            kernel.len(),
            BANDLIMITED_RATE_LEVELS * BANDLIMITED_PHASE_LEVELS * BANDLIMITED_TAPS
        );
        let (center, fraction) = fractional_read_position(write_index, history_len, delay_samples);
        let phase_position = fraction * BANDLIMITED_PHASE_INTERVALS as f32;
        let phase_low = (phase_position.floor() as usize).min(BANDLIMITED_PHASE_INTERVALS - 1);
        let phase_fraction = phase_position - phase_low as f32;

        let finite_rate = if read_rate.is_finite() {
            read_rate
        } else {
            1.0
        };
        let rate_position = (finite_rate.clamp(1.0, MAX_DOPPLER_PITCH_RATIO) - 1.0)
            * BANDLIMITED_RATE_INTERVALS as f32;
        let rate_low = (rate_position.floor() as usize).min(BANDLIMITED_RATE_INTERVALS - 1);
        let rate_fraction = (rate_position - rate_low as f32).clamp(0.0, 1.0);

        let mut tap_indices = [0_usize; BANDLIMITED_TAPS];
        let mut tap_weights = [0.0_f32; BANDLIMITED_TAPS];
        for tap in 0..BANDLIMITED_TAPS {
            let offset = BANDLIMITED_FIRST_OFFSET + tap as isize;
            tap_indices[tap] = wrapped_offset(center, history_len, offset);
            let low_rate_low_phase =
                kernel[bandlimited_coefficient_index(rate_low, phase_low, tap)];
            let low_rate_high_phase =
                kernel[bandlimited_coefficient_index(rate_low, phase_low + 1, tap)];
            let high_rate_low_phase =
                kernel[bandlimited_coefficient_index(rate_low + 1, phase_low, tap)];
            let high_rate_high_phase =
                kernel[bandlimited_coefficient_index(rate_low + 1, phase_low + 1, tap)];
            let low_rate =
                low_rate_low_phase + (low_rate_high_phase - low_rate_low_phase) * phase_fraction;
            let high_rate =
                high_rate_low_phase + (high_rate_high_phase - high_rate_low_phase) * phase_fraction;
            tap_weights[tap] = low_rate + (high_rate - low_rate) * rate_fraction;
        }
        Self {
            tap_indices,
            tap_weights,
            fraction,
            required_history_samples: delay_samples.ceil() as usize + BANDLIMITED_TAPS / 2,
        }
    }

    fn apply(self, history: &[f32]) -> f32 {
        let mut output = 0.0_f32;
        for tap in 0..BANDLIMITED_TAPS {
            output += history[self.tap_indices[tap]] * self.tap_weights[tap];
        }
        output
    }
}

fn bandlimited_coefficient_index(rate_level: usize, phase_level: usize, tap: usize) -> usize {
    (rate_level * BANDLIMITED_PHASE_LEVELS + phase_level) * BANDLIMITED_TAPS + tap
}

fn wrapped_offset(center: usize, history_len: usize, offset: isize) -> usize {
    let position = center as isize + offset;
    if position < 0 {
        (position + history_len as isize) as usize
    } else if position >= history_len as isize {
        (position - history_len as isize) as usize
    } else {
        position as usize
    }
}

fn fractional_read_position(
    write_index: usize,
    history_len: usize,
    delay_samples: f32,
) -> (usize, f32) {
    let mut read_position = write_index as f32 - delay_samples;
    if read_position < 0.0 {
        read_position += history_len as f32;
        if read_position >= history_len as f32 {
            read_position = 0.0;
        }
    }
    let center = read_position.floor() as usize;
    (center, read_position - center as f32)
}

#[derive(Clone, Copy, Debug)]
struct RetardedGuidance {
    geometry_rate_samples_per_sample: f64,
    geometry_speed_samples_per_sample: f64,
    relative_speed_mps: f32,
    pitch_ratio: f32,
}

impl RetardedGuidance {
    const ZERO: Self = Self {
        geometry_rate_samples_per_sample: 0.0,
        geometry_speed_samples_per_sample: 0.0,
        relative_speed_mps: 0.0,
        pitch_ratio: 1.0,
    };
}

fn uncensored_geometry_observation_samples(raw_target_samples: f32) -> f64 {
    if raw_target_samples.is_finite() {
        f64::from(raw_target_samples.max(0.0))
    } else {
        0.0
    }
}

/// Solves the cold-history constant-relative-velocity retarded delay directly.
///
/// With current squared range `Q`, derivative `Qdot`, and `W2 = |v/c|^2`,
/// squaring `D = sqrt(Q - Qdot D + W2 D^2)` gives
/// `(1 - W2) D^2 + Qdot D - Q = 0`. Guidance clamps `W2` to at most 0.25, so
/// the positive-root denominator remains well conditioned. The receding case
/// uses the algebraically equivalent form that avoids subtracting nearby
/// positive values.
fn constant_velocity_retarded_delay_samples(
    squared_delay_samples: f64,
    squared_step_samples: f64,
    squared_second_difference_samples: f64,
) -> f64 {
    if squared_delay_samples <= 0.0 || !squared_delay_samples.is_finite() {
        return 0.0;
    }
    let speed_squared = squared_second_difference_samples * 0.5;
    let squared_derivative = squared_step_samples - speed_squared;
    let quadratic = 1.0 - speed_squared;
    let discriminant = (squared_derivative * squared_derivative
        + 4.0 * quadratic * squared_delay_samples)
        .max(0.0)
        .sqrt();
    if squared_derivative >= 0.0 {
        2.0 * squared_delay_samples / (discriminant + squared_derivative)
    } else {
        (discriminant - squared_derivative) / (2.0 * quadratic)
    }
}

/// The only channel counts accepted by [`StereoProgramPropagationDelay`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StereoProgramDelayError {
    UnsupportedChannelCount(usize),
}

/// Callback-side proof that a logical stereo source advances one shared
/// trajectory and constructs one shared read plan per output frame.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StereoProgramDelayInstrumentation {
    pub(crate) frames_processed: u64,
    pub(crate) trajectory_advances: u64,
    pub(crate) read_plan_advances: u64,
    pub(crate) channel_count_changes: u64,
}

/// Persistent storage owned by one shared-plan stereo propagation controller.
///
/// The byte counts cover live `Vec` payload capacities. Allocator bookkeeping
/// is deliberately excluded because it is allocator-specific; inline state is
/// reported separately by [`StereoProgramPropagationDelay::inline_state_bytes`]
/// and the process-wide polyphase table is reported once by session telemetry.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StereoProgramDelayMemory {
    pub(crate) audio_history_payload_bytes: usize,
    pub(crate) geometry_history_payload_bytes: usize,
    pub(crate) total_heap_payload_bytes: usize,
    pub(crate) additional_channel_payload_bytes: usize,
}

/// One logical mono-or-stereo source program with a single physical
/// propagation trajectory.
///
/// This is intentionally parallel to, rather than a refactor of,
/// [`PropagationDelayLine`]. The legacy mono controller and its call sites
/// remain the frozen reference. This controller owns exactly one geometry
/// history, retarded-time solve, fast-motion hysteresis state, teleport state,
/// write clock, and fractional read plan. Its one or two audio histories only
/// apply that shared plan, so left and right cannot acquire independent
/// Doppler, onset, or teleport timing.
///
/// Every buffer is sized at construction. Observation, reset, invalidation,
/// channel-count changes, and [`Self::process_frame`] allocate nothing, take no
/// locks, and perform no syscalls.
#[derive(Debug)]
pub(crate) struct StereoProgramPropagationDelay {
    audio_history: [Vec<f32>; 2],
    trajectory: StereoProgramTrajectory,
    active_input_planes: usize,
    /// Samples written since a channel's retained history became untrusted.
    valid_history_samples: [usize; 2],
    history_guarded: [bool; 2],
    instrumentation: StereoProgramDelayInstrumentation,
}

impl StereoProgramPropagationDelay {
    pub(crate) fn new(maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        let history_len = delay_history_len(maximum_delay_samples);
        Self {
            audio_history: [vec![0.0; history_len], vec![0.0; history_len]],
            trajectory: StereoProgramTrajectory::new(maximum_delay_samples, sample_rate_hz),
            active_input_planes: 0,
            valid_history_samples: [history_len; 2],
            history_guarded: [false; 2],
            instrumentation: StereoProgramDelayInstrumentation::default(),
        }
    }

    pub(crate) fn current_delay_samples(&self) -> f32 {
        self.trajectory.applied_delay_samples
    }

    pub(crate) fn observe_block_target(&mut self, raw_target_samples: f32) {
        self.trajectory
            .observe_block_target_inner(raw_target_samples, None);
    }

    pub(crate) fn observe_block_target_with_motion(
        &mut self,
        raw_target_samples: f32,
        radial_velocity_mps: f32,
        relative_speed_mps: f32,
    ) {
        let guidance = self
            .trajectory
            .retarded_guidance(radial_velocity_mps, relative_speed_mps);
        self.trajectory
            .observe_block_target_inner(raw_target_samples, guidance);
    }

    pub(crate) fn observe_block_target_with_zero_motion(&mut self, raw_target_samples: f32) {
        self.trajectory
            .observe_block_target_with_zero_motion(raw_target_samples);
    }

    /// Marks geometry and retained program audio as untrustworthy.
    ///
    /// The next observed target is adopted instantly. Until enough new input
    /// has covered every tap in the shared plan, guarded channels emit silence
    /// instead of audio retained from the preceding source activation.
    pub(crate) fn invalidate(&mut self) {
        self.trajectory.invalidate();
        self.guard_all_histories();
    }

    /// Explicit scene discontinuity: neither plane may replay the previous scene.
    pub(crate) fn reset_history(&mut self) {
        for history in &mut self.audio_history {
            history.fill(0.0);
        }
        self.invalidate();
    }

    /// Adopts `delay_samples` instantly and cancels any teleport crossfade.
    pub(crate) fn reset_to(&mut self, delay_samples: f32) {
        self.trajectory.reset_to(delay_samples);
        if self.instrumentation.frames_processed > 0 {
            self.guard_all_histories();
        }
    }

    /// Processes one sample frame from one or two planar program inputs.
    ///
    /// `input[0]` is left/mono and `input[1]` is right. A mono frame always
    /// writes zero to the inactive right history, preserving deterministic
    /// state if that logical source later switches to authored stereo. Invalid
    /// channel counts return without advancing the trajectory or either ring.
    pub(crate) fn process_frame(
        &mut self,
        input: [f32; 2],
        active_input_planes: usize,
    ) -> Result<[f32; 2], StereoProgramDelayError> {
        if !(1..=2).contains(&active_input_planes) {
            return Err(StereoProgramDelayError::UnsupportedChannelCount(
                active_input_planes,
            ));
        }

        self.observe_channel_count(active_input_planes);
        let plan = self.trajectory.advance_read_plan();
        self.instrumentation.frames_processed =
            self.instrumentation.frames_processed.wrapping_add(1);
        self.instrumentation.trajectory_advances =
            self.instrumentation.trajectory_advances.wrapping_add(1);
        self.instrumentation.read_plan_advances =
            self.instrumentation.read_plan_advances.wrapping_add(1);

        self.audio_history[0][plan.write_index] = input[0];
        self.audio_history[1][plan.write_index] = if active_input_planes == 2 {
            input[1]
        } else {
            0.0
        };
        self.note_history_writes();

        let mut output = [0.0; 2];
        if self.channel_history_is_ready(0, plan.required_history_samples) {
            output[0] = plan.apply(&self.audio_history[0]);
        }
        if active_input_planes == 2
            && self.channel_history_is_ready(1, plan.required_history_samples)
        {
            output[1] = plan.apply(&self.audio_history[1]);
        }
        Ok(output)
    }

    /// Shared, guarded fractional read for the selected rooftop arrival.
    /// The original program head remains the reflection send.
    pub(crate) fn read_behind_newest(&self, delay_samples: f32, history_samples: usize) -> [f32; 2] {
        let len = self.trajectory.history_len;
        let write = self.trajectory.write_index;
        let delay = delay_samples.clamp(0.0, self.trajectory.maximum_delay_samples);
        let (center, x) = fractional_read_position(write, len, delay + 1.0);
        let taps = [(center + len - 2) % len, (center + len - 1) % len, center, (center + 1) % len];
        let weights = [-(x+1.0)*x*(x-1.0)/6.0, (x+2.0)*x*(x-1.0)*0.5,
            -(x+2.0)*(x+1.0)*(x-1.0)*0.5, (x+2.0)*(x+1.0)*x/6.0];
        let newest = (write + len - 1) % len;
        std::array::from_fn(|channel| {
            let valid = history_samples.min(self.valid_history_samples[channel]);
            taps.into_iter().zip(weights).map(|(tap, weight)| {
                if (newest + len - tap) % len < valid { self.audio_history[channel][tap] * weight } else { 0.0 }
            }).sum()
        })
    }

    pub(crate) fn instrumentation(&self) -> StereoProgramDelayInstrumentation {
        self.instrumentation
    }

    pub(crate) fn memory(&self) -> StereoProgramDelayMemory {
        let left_bytes = self.audio_history[0].capacity() * core::mem::size_of::<f32>();
        let right_bytes = self.audio_history[1].capacity() * core::mem::size_of::<f32>();
        let geometry_bytes =
            self.trajectory.geometry_history.capacity() * core::mem::size_of::<f32>();
        StereoProgramDelayMemory {
            audio_history_payload_bytes: left_bytes + right_bytes,
            geometry_history_payload_bytes: geometry_bytes,
            total_heap_payload_bytes: left_bytes + right_bytes + geometry_bytes,
            additional_channel_payload_bytes: right_bytes,
        }
    }

    pub(crate) const fn inline_state_bytes() -> usize {
        core::mem::size_of::<Self>()
    }

    #[cfg(test)]
    pub(crate) fn is_crossfading(&self) -> bool {
        self.trajectory.crossfade_remaining > 0
    }

    #[cfg(test)]
    pub(crate) fn fast_motion_guided(&self) -> bool {
        self.trajectory.fast_motion_guided
    }

    #[cfg(test)]
    pub(crate) fn read_phase(&self) -> f32 {
        self.trajectory.last_primary_read_phase
    }

    #[cfg(test)]
    pub(crate) fn current_geometry_delay_samples(&self) -> f32 {
        self.trajectory.geometry_delay_samples as f32
    }

    #[cfg(test)]
    pub(crate) fn target_delay_samples(&self) -> f32 {
        self.trajectory.target_delay_samples
    }

    #[cfg(test)]
    pub(crate) fn previous_raw_target_samples(&self) -> f64 {
        self.trajectory.previous_raw_target_samples
    }

    fn observe_channel_count(&mut self, active_input_planes: usize) {
        if self.active_input_planes == active_input_planes {
            return;
        }
        if self.active_input_planes != 0 {
            self.instrumentation.channel_count_changes =
                self.instrumentation.channel_count_changes.wrapping_add(1);
        }
        if self.active_input_planes == 2 && active_input_planes == 1 {
            // Stale authored-right samples must not reappear if this source is
            // subsequently rebound as stereo before the ring wraps.
            self.history_guarded[1] = true;
            self.valid_history_samples[1] = 0;
        }
        self.active_input_planes = active_input_planes;
    }

    fn guard_all_histories(&mut self) {
        self.history_guarded = [true; 2];
        self.valid_history_samples = [0; 2];
    }

    fn note_history_writes(&mut self) {
        let history_len = self.audio_history[0].len();
        for channel in 0..2 {
            if self.history_guarded[channel] {
                self.valid_history_samples[channel] = self.valid_history_samples[channel]
                    .saturating_add(1)
                    .min(history_len);
                if self.valid_history_samples[channel] == history_len {
                    self.history_guarded[channel] = false;
                }
            }
        }
    }

    fn channel_history_is_ready(&self, channel: usize, required_samples: usize) -> bool {
        !self.history_guarded[channel] || self.valid_history_samples[channel] >= required_samples
    }
}

/// Geometry and read-clock state shared by both program channels.
#[derive(Debug)]
struct StereoProgramTrajectory {
    geometry_history: Vec<f32>,
    bandlimited_kernel: &'static [f32],
    write_index: usize,
    history_len: usize,
    maximum_delay_samples: f32,
    applied_delay_samples: f32,
    target_delay_samples: f32,
    geometry_delay_samples: f64,
    geometry_rate_samples_per_sample: f64,
    geometry_delay_squared_samples: f64,
    geometry_squared_step_samples: f64,
    geometry_squared_second_difference_samples: f64,
    prehistory_delay_squared_samples: f64,
    prehistory_squared_step_samples: f64,
    prehistory_squared_second_difference_samples: f64,
    geometry_history_samples: usize,
    geometry_samples_since_publication: usize,
    retarded_time_guided: bool,
    fast_motion_guided: bool,
    bandlimited_readout_active: bool,
    bandlimited_transition_remaining: u8,
    outgoing_bandlimited_blend: f32,
    zero_motion_retiring: bool,
    zero_motion_samples: usize,
    legacy_position_anchor_samples: f32,
    legacy_target_rate_samples_per_sample: f32,
    previous_raw_target_samples: f64,
    outgoing_delay_samples: f32,
    crossfade_remaining: u32,
    crossfade_frames: u32,
    slew_retention: f32,
    teleport_threshold_samples: f32,
    initialized: bool,
    last_primary_read_phase: f32,
}

impl StereoProgramTrajectory {
    fn new(maximum_delay_samples: usize, sample_rate_hz: i32) -> Self {
        debug_assert!(sample_rate_hz > 0);
        let sample_rate = sample_rate_hz as f32;
        let history_len = delay_history_len(maximum_delay_samples);
        Self {
            geometry_history: vec![0.0; history_len],
            bandlimited_kernel: bandlimited_kernel_table(),
            write_index: 0,
            history_len,
            maximum_delay_samples: maximum_delay_samples as f32,
            applied_delay_samples: 0.0,
            target_delay_samples: 0.0,
            geometry_delay_samples: 0.0,
            geometry_rate_samples_per_sample: 0.0,
            geometry_delay_squared_samples: 0.0,
            geometry_squared_step_samples: 0.0,
            geometry_squared_second_difference_samples: 0.0,
            prehistory_delay_squared_samples: 0.0,
            prehistory_squared_step_samples: 0.0,
            prehistory_squared_second_difference_samples: 0.0,
            geometry_history_samples: 0,
            geometry_samples_since_publication: 0,
            retarded_time_guided: false,
            fast_motion_guided: false,
            bandlimited_readout_active: false,
            bandlimited_transition_remaining: 0,
            outgoing_bandlimited_blend: 0.0,
            zero_motion_retiring: false,
            zero_motion_samples: 0,
            legacy_position_anchor_samples: 0.0,
            legacy_target_rate_samples_per_sample: 0.0,
            previous_raw_target_samples: 0.0,
            outgoing_delay_samples: 0.0,
            crossfade_remaining: 0,
            crossfade_frames: (TELEPORT_CROSSFADE_SECONDS * sample_rate).ceil().max(1.0) as u32,
            slew_retention: (-1.0 / (PROPAGATION_SLEW_TIME_SECONDS * sample_rate)).exp(),
            teleport_threshold_samples: TELEPORT_DELAY_STEP_SECONDS * sample_rate,
            initialized: false,
            last_primary_read_phase: 0.0,
        }
    }

    fn invalidate(&mut self) {
        self.initialized = false;
        self.geometry_history_samples = 0;
        self.geometry_samples_since_publication = 0;
        self.bandlimited_readout_active = false;
        self.bandlimited_transition_remaining = 0;
        self.outgoing_bandlimited_blend = 0.0;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
    }

    fn reset_to(&mut self, delay_samples: f32) {
        let uncensored_delay = uncensored_geometry_observation_samples(delay_samples);
        let delay = self.clamp_delay(delay_samples);
        self.applied_delay_samples = delay;
        self.target_delay_samples = delay;
        self.reset_geometry_history(delay);
        self.retarded_time_guided = false;
        self.fast_motion_guided = false;
        self.bandlimited_readout_active = false;
        self.bandlimited_transition_remaining = 0;
        self.outgoing_bandlimited_blend = 0.0;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
        self.legacy_position_anchor_samples = delay;
        self.legacy_target_rate_samples_per_sample = 0.0;
        self.previous_raw_target_samples = uncensored_delay;
        self.outgoing_delay_samples = delay;
        self.crossfade_remaining = 0;
        self.initialized = true;
    }

    fn observe_block_target_with_zero_motion(&mut self, raw_target_samples: f32) {
        let uncensored_raw = uncensored_geometry_observation_samples(raw_target_samples);
        let teleported = self.initialized
            && (uncensored_raw - self.previous_raw_target_samples).abs()
                > f64::from(self.teleport_threshold_samples);
        if !self.initialized
            || !(self.fast_motion_guided || self.zero_motion_retiring)
            || teleported
        {
            self.observe_block_target_inner(raw_target_samples, None);
            return;
        }

        let continuously_corrected = self.geometry_samples_since_publication > 0;
        let evolution_delay = if continuously_corrected {
            self.geometry_delay_samples
        } else {
            uncensored_raw
        };
        let publication_correction_rate = if continuously_corrected {
            (uncensored_raw - evolution_delay) / self.geometry_samples_since_publication as f64
        } else {
            0.0
        };
        let already_retiring = self.zero_motion_retiring;
        self.previous_raw_target_samples = uncensored_raw;
        self.configure_constant_velocity_geometry(
            evolution_delay,
            RetardedGuidance::ZERO,
            publication_correction_rate,
            false,
        );
        self.retarded_time_guided = true;
        self.fast_motion_guided = true;
        self.zero_motion_retiring = true;
        if !already_retiring {
            self.zero_motion_samples = 0;
        }
        let raw = self.clamp_delay(raw_target_samples);
        self.legacy_position_anchor_samples = raw;
        self.legacy_target_rate_samples_per_sample = 0.0;
    }

    fn observe_block_target_inner(
        &mut self,
        raw_target_samples: f32,
        guidance: Option<RetardedGuidance>,
    ) {
        let uncensored_raw = uncensored_geometry_observation_samples(raw_target_samples);
        let raw = self.clamp_delay(raw_target_samples);
        let teleported = self.initialized
            && (uncensored_raw - self.previous_raw_target_samples).abs()
                > f64::from(self.teleport_threshold_samples);
        // An initialized unguided line already describes a static past. Motion
        // begins at this observation; it must not be extrapolated backward
        // across audio that was emitted before the corner.
        let seed_prehistory = !self.initialized || teleported;
        let analytic_raw = uncensored_raw;
        let continuously_corrected = self.initialized
            && self.retarded_time_guided
            && !teleported
            && self.geometry_samples_since_publication > 0;
        let evolution_delay = if continuously_corrected {
            self.geometry_delay_samples
        } else {
            analytic_raw
        };
        let publication_correction_rate = if continuously_corrected {
            (analytic_raw - evolution_delay) / self.geometry_samples_since_publication as f64
        } else {
            0.0
        };
        if !self.initialized {
            self.reset_to(raw);
        } else if teleported {
            self.outgoing_delay_samples = self.applied_delay_samples;
            self.outgoing_bandlimited_blend = self.bandlimited_blend();
            self.applied_delay_samples = raw;
            self.target_delay_samples = raw;
            self.reset_geometry_history(raw);
            self.crossfade_remaining = self.crossfade_frames;
        }
        self.previous_raw_target_samples = uncensored_raw;
        if let Some(guidance) = guidance {
            self.zero_motion_retiring = false;
            self.zero_motion_samples = 0;
            if !self.retarded_time_guided {
                self.reset_geometry_history(raw);
            }
            self.configure_constant_velocity_geometry(
                evolution_delay,
                guidance,
                publication_correction_rate,
                seed_prehistory,
            );
            self.retarded_time_guided = true;
            self.fast_motion_guided =
                fast_mover_mode(self.fast_motion_guided, guidance.relative_speed_mps);
            self.legacy_position_anchor_samples = self.clamp_delay(raw * guidance.pitch_ratio);
            self.legacy_target_rate_samples_per_sample = (1.0 - guidance.pitch_ratio).clamp(
                -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            );
            if teleported && self.fast_motion_guided {
                let incoming_delay = self.prehistory_retarded_delay_samples();
                self.applied_delay_samples = incoming_delay;
                self.target_delay_samples = incoming_delay;
            }
        } else {
            self.zero_motion_retiring = false;
            self.zero_motion_samples = 0;
            self.target_delay_samples = raw;
            self.reset_geometry_history(raw);
            self.retarded_time_guided = false;
            self.fast_motion_guided = false;
            self.legacy_position_anchor_samples = raw;
            self.legacy_target_rate_samples_per_sample = 0.0;
        }
    }

    /// Advances all physical state once and returns one immutable plan for all
    /// active program planes to apply.
    fn advance_read_plan(&mut self) -> StereoProgramReadPlan {
        let advance_retarded_state = self.retarded_time_guided;
        let mut retire_zero_motion_after_sample = false;
        let previous_applied_delay = self.applied_delay_samples;
        if self.retarded_time_guided {
            self.record_geometry_sample();
            if self.fast_motion_guided {
                self.target_delay_samples = self.retarded_delay_samples();
                retire_zero_motion_after_sample = self.zero_motion_retiring
                    && self.target_delay_samples.ceil() as usize <= self.zero_motion_samples;
            } else {
                self.legacy_position_anchor_samples = self.clamp_delay(
                    self.legacy_position_anchor_samples
                        + self.legacy_target_rate_samples_per_sample,
                );
                let integrated_target = self.clamp_delay(
                    self.target_delay_samples + self.legacy_target_rate_samples_per_sample,
                );
                self.target_delay_samples = self.clamp_delay(
                    self.legacy_position_anchor_samples
                        + (integrated_target - self.legacy_position_anchor_samples)
                            * self.slew_retention,
                );
            }
        }
        if self.crossfade_remaining == 0 || self.retarded_time_guided {
            let requested = if self.retarded_time_guided && self.fast_motion_guided {
                self.target_delay_samples
            } else {
                self.target_delay_samples
                    + (self.applied_delay_samples - self.target_delay_samples) * self.slew_retention
            };
            let minimum_step = if self.fast_motion_guided {
                -MAX_BANDLIMITED_APPROACH_SLEW_SAMPLES_PER_SAMPLE
            } else {
                -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE
            };
            let step = (requested - self.applied_delay_samples)
                .clamp(minimum_step, MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE);
            self.applied_delay_samples = self.clamp_delay(self.applied_delay_samples + step);
        }
        let applied_step = self.applied_delay_samples - previous_applied_delay;
        update_bandlimited_readout_state(
            self.fast_motion_guided,
            applied_step,
            &mut self.bandlimited_readout_active,
            &mut self.bandlimited_transition_remaining,
        );
        let primary_bandlimited_blend = self.bandlimited_blend();
        let primary_read_rate =
            (1.0 - applied_step).clamp(MIN_DOPPLER_PITCH_RATIO, MAX_DOPPLER_PITCH_RATIO);

        let primary = VariableRateReadPlan::new(
            self.write_index,
            self.history_len,
            self.applied_delay_samples,
            primary_read_rate,
            primary_bandlimited_blend,
            self.bandlimited_kernel,
        );
        self.last_primary_read_phase = primary.fraction;
        let (outgoing, primary_gain, outgoing_gain, required_history_samples) =
            if self.crossfade_remaining > 0 {
                let outgoing = VariableRateReadPlan::new(
                    self.write_index,
                    self.history_len,
                    self.outgoing_delay_samples,
                    1.0,
                    self.outgoing_bandlimited_blend,
                    self.bandlimited_kernel,
                );
                let elapsed = self.crossfade_frames - self.crossfade_remaining;
                let progress = elapsed as f32 / self.crossfade_frames as f32;
                let angle = progress * core::f32::consts::FRAC_PI_2;
                self.crossfade_remaining -= 1;
                (
                    Some(outgoing),
                    angle.sin(),
                    angle.cos(),
                    primary
                        .required_history_samples
                        .max(outgoing.required_history_samples),
                )
            } else {
                (None, 1.0, 0.0, primary.required_history_samples)
            };
        let plan = StereoProgramReadPlan {
            write_index: self.write_index,
            primary,
            outgoing,
            primary_gain,
            outgoing_gain,
            required_history_samples,
        };

        self.write_index += 1;
        if self.write_index == self.history_len {
            self.write_index = 0;
        }
        if advance_retarded_state {
            if self.fast_motion_guided {
                self.advance_constant_velocity_geometry();
            } else {
                self.geometry_delay_samples = self.clamp_geometry_delay(
                    self.geometry_delay_samples + self.geometry_rate_samples_per_sample,
                );
            }
            self.advance_prehistory_geometry();
            self.geometry_samples_since_publication =
                self.geometry_samples_since_publication.saturating_add(1);
            if self.zero_motion_retiring {
                self.zero_motion_samples = self.zero_motion_samples.saturating_add(1);
            }
        }
        if retire_zero_motion_after_sample {
            self.finish_zero_motion_retirement();
        }
        self.advance_bandlimited_transition();
        plan
    }

    fn bandlimited_blend(&self) -> f32 {
        if !self.bandlimited_readout_active {
            return 0.0;
        }
        1.0 - self.bandlimited_transition_remaining as f32 / BANDLIMITED_TRANSITION_FRAMES as f32
    }

    fn advance_bandlimited_transition(&mut self) {
        self.bandlimited_transition_remaining =
            self.bandlimited_transition_remaining.saturating_sub(1);
    }

    fn clamp_delay(&self, delay_samples: f32) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, self.maximum_delay_samples)
        } else {
            0.0
        }
    }

    fn clamp_geometry_delay(&self, delay_samples: f64) -> f64 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, f64::from(self.maximum_delay_samples))
        } else {
            0.0
        }
    }

    fn expose_geometry_delay(&self, delay_samples: f64) -> f32 {
        if delay_samples.is_finite() {
            delay_samples.clamp(0.0, f64::from(self.maximum_delay_samples)) as f32
        } else {
            0.0
        }
    }

    fn retarded_guidance(
        &self,
        radial_velocity_mps: f32,
        relative_speed_mps: f32,
    ) -> Option<RetardedGuidance> {
        if !radial_velocity_mps.is_finite()
            || !relative_speed_mps.is_finite()
            || relative_speed_mps <= 0.0
        {
            return None;
        }
        let denominator = 1.0 + radial_velocity_mps / SPEED_OF_SOUND_METERS_PER_SECOND;
        let unbounded_pitch_ratio = if denominator > 0.0 {
            denominator.recip()
        } else {
            MAX_DOPPLER_PITCH_RATIO
        };
        let pitch_ratio =
            unbounded_pitch_ratio.clamp(MIN_DOPPLER_PITCH_RATIO, MAX_DOPPLER_PITCH_RATIO);
        Some(RetardedGuidance {
            geometry_rate_samples_per_sample: (f64::from(radial_velocity_mps)
                / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND))
            .clamp(-0.5, 0.5),
            geometry_speed_samples_per_sample: (f64::from(relative_speed_mps)
                / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND))
            .clamp(0.0, 0.5),
            relative_speed_mps,
            pitch_ratio,
        })
    }

    fn reset_geometry_history(&mut self, delay_samples: f32) {
        let delay = self.clamp_delay(delay_samples);
        self.geometry_delay_samples = f64::from(delay);
        self.geometry_rate_samples_per_sample = 0.0;
        self.geometry_delay_squared_samples = self.geometry_delay_samples.powi(2);
        self.geometry_squared_step_samples = 0.0;
        self.geometry_squared_second_difference_samples = 0.0;
        self.prehistory_delay_squared_samples = self.geometry_delay_squared_samples;
        self.prehistory_squared_step_samples = 0.0;
        self.prehistory_squared_second_difference_samples = 0.0;
        self.geometry_history_samples = 0;
        self.geometry_samples_since_publication = 0;
    }

    fn configure_constant_velocity_geometry(
        &mut self,
        delay_samples: f64,
        guidance: RetardedGuidance,
        publication_correction_rate: f64,
        seed_prehistory: bool,
    ) {
        let delay = delay_samples;
        let radial_rate = (guidance.geometry_rate_samples_per_sample + publication_correction_rate)
            .clamp(-0.5, 0.5);
        let speed_rate = guidance
            .geometry_speed_samples_per_sample
            .max(radial_rate.abs());
        let speed_squared = speed_rate * speed_rate;
        self.geometry_delay_samples = delay;
        self.geometry_delay_squared_samples = delay * delay;
        self.geometry_squared_step_samples = 2.0 * delay * radial_rate + speed_squared;
        self.geometry_squared_second_difference_samples = 2.0 * speed_squared;
        self.geometry_rate_samples_per_sample = radial_rate;
        self.geometry_samples_since_publication = 0;
        if seed_prehistory {
            self.prehistory_delay_squared_samples = self.geometry_delay_squared_samples;
            self.prehistory_squared_step_samples = self.geometry_squared_step_samples;
            self.prehistory_squared_second_difference_samples =
                self.geometry_squared_second_difference_samples;
        }
    }

    fn advance_constant_velocity_geometry(&mut self) {
        self.geometry_delay_squared_samples =
            (self.geometry_delay_squared_samples + self.geometry_squared_step_samples).max(0.0);
        self.geometry_squared_step_samples += self.geometry_squared_second_difference_samples;
        self.geometry_delay_samples = self.geometry_delay_squared_samples.sqrt();
    }

    fn advance_prehistory_geometry(&mut self) {
        self.prehistory_delay_squared_samples =
            (self.prehistory_delay_squared_samples + self.prehistory_squared_step_samples).max(0.0);
        self.prehistory_squared_step_samples += self.prehistory_squared_second_difference_samples;
    }

    fn finish_zero_motion_retirement(&mut self) {
        let stationary_delay = self.expose_geometry_delay(self.geometry_delay_samples);
        self.target_delay_samples = stationary_delay;
        self.legacy_position_anchor_samples = stationary_delay;
        self.legacy_target_rate_samples_per_sample = 0.0;
        self.reset_geometry_history(stationary_delay);
        self.retarded_time_guided = false;
        self.fast_motion_guided = false;
        self.zero_motion_retiring = false;
        self.zero_motion_samples = 0;
    }

    fn record_geometry_sample(&mut self) {
        self.geometry_history[self.write_index] =
            self.expose_geometry_delay(self.geometry_delay_samples);
        self.geometry_history_samples = self
            .geometry_history_samples
            .saturating_add(1)
            .min(self.geometry_history.len());
    }

    fn retarded_delay_samples(&self) -> f32 {
        let available_lookback = self.geometry_history_samples.saturating_sub(1);
        let mut delay = self.target_delay_samples;
        if delay.ceil() as usize > available_lookback {
            let constant_velocity_delay = self.prehistory_retarded_delay_samples();
            if constant_velocity_delay.ceil() as usize > available_lookback {
                return constant_velocity_delay;
            }
            delay = constant_velocity_delay;
        }

        for _ in 0..8 {
            delay = self.clamp_delay(self.geometry_at_delay(delay));
        }
        delay
    }

    fn prehistory_retarded_delay_samples(&self) -> f32 {
        self.expose_geometry_delay(constant_velocity_retarded_delay_samples(
            self.prehistory_delay_squared_samples,
            self.prehistory_squared_step_samples,
            self.prehistory_squared_second_difference_samples,
        ))
    }

    fn geometry_at_delay(&self, delay_samples: f32) -> f32 {
        let available_lookback = self.geometry_history_samples.saturating_sub(1);
        let delay = self.clamp_delay(delay_samples);
        if delay.ceil() as usize > available_lookback {
            if self.fast_motion_guided {
                let lookback = f64::from(delay);
                let speed_squared = self.prehistory_squared_second_difference_samples * 0.5;
                let squared_derivative = self.prehistory_squared_step_samples - speed_squared;
                let squared_delay = self.prehistory_delay_squared_samples
                    - squared_derivative * lookback
                    + speed_squared * lookback * lookback;
                return self.expose_geometry_delay(squared_delay.max(0.0).sqrt());
            }
            return self.clamp_delay(
                (self.geometry_delay_samples
                    - self.geometry_rate_samples_per_sample * f64::from(delay))
                    as f32,
            );
        }

        let whole = delay.floor() as usize;
        let fraction = delay - whole as f32;
        let newer =
            (self.write_index + self.geometry_history.len() - whole) % self.geometry_history.len();
        let older = if newer == 0 {
            self.geometry_history.len() - 1
        } else {
            newer - 1
        };
        self.geometry_history[newer]
            + (self.geometry_history[older] - self.geometry_history[newer]) * fraction
    }
}

#[derive(Clone, Copy, Debug)]
struct StereoProgramReadPlan {
    write_index: usize,
    primary: VariableRateReadPlan,
    outgoing: Option<VariableRateReadPlan>,
    primary_gain: f32,
    outgoing_gain: f32,
    required_history_samples: usize,
}

impl StereoProgramReadPlan {
    fn apply(self, history: &[f32]) -> f32 {
        let primary = self.primary.apply(history);
        if let Some(outgoing) = self.outgoing {
            primary * self.primary_gain + outgoing.apply(history) * self.outgoing_gain
        } else {
            primary
        }
    }
}

/// One immutable fractional read shared by every plane of a logical program.
/// Static operation retains the original four-tap arithmetic exactly. A real
/// moving read adds the rate-aware polyphase plan and crosses to it smoothly.
#[derive(Clone, Copy, Debug)]
struct VariableRateReadPlan {
    legacy: FractionalReadPlan,
    bandlimited: Option<BandlimitedReadPlan>,
    bandlimited_blend: f32,
    fraction: f32,
    required_history_samples: usize,
}

impl VariableRateReadPlan {
    fn new(
        write_index: usize,
        history_len: usize,
        delay_samples: f32,
        read_rate: f32,
        bandlimited_blend: f32,
        kernel: &[f32],
    ) -> Self {
        let legacy = FractionalReadPlan::new(write_index, history_len, delay_samples);
        if bandlimited_blend <= 0.0 || delay_samples < BANDLIMITED_NEWEST_OFFSET as f32 {
            return Self {
                legacy,
                bandlimited: None,
                bandlimited_blend: 0.0,
                fraction: legacy.fraction,
                required_history_samples: legacy.required_history_samples,
            };
        }

        let bandlimited =
            BandlimitedReadPlan::new(write_index, history_len, delay_samples, read_rate, kernel);
        Self {
            legacy,
            bandlimited: Some(bandlimited),
            bandlimited_blend: bandlimited_blend.min(1.0),
            fraction: bandlimited.fraction,
            required_history_samples: legacy
                .required_history_samples
                .max(bandlimited.required_history_samples),
        }
    }

    fn apply(self, history: &[f32]) -> f32 {
        let Some(bandlimited) = self.bandlimited else {
            return self.legacy.apply(history);
        };
        let filtered = bandlimited.apply(history);
        if self.bandlimited_blend >= 1.0 {
            filtered
        } else {
            let legacy = self.legacy.apply(history);
            legacy + (filtered - legacy) * self.bandlimited_blend
        }
    }
}

/// Four causal Lagrange taps and coefficients calculated once per program
/// frame, then applied unchanged to every active input plane.
#[derive(Clone, Copy, Debug)]
struct FractionalReadPlan {
    tap_indices: [usize; 4],
    tap_weights: [f32; 4],
    fraction: f32,
    required_history_samples: usize,
}

impl FractionalReadPlan {
    fn new(write_index: usize, history_len: usize, delay_samples: f32) -> Self {
        let mut read_position = write_index as f32 - delay_samples;
        if read_position < 0.0 {
            read_position += history_len as f32;
            if read_position >= history_len as f32 {
                read_position = 0.0;
            }
        }
        let center = read_position.floor() as usize;
        let fraction = read_position - center as f32;
        let previous = if center == 0 {
            history_len - 1
        } else {
            center - 1
        };
        let previous_2 = if previous == 0 {
            history_len - 1
        } else {
            previous - 1
        };
        let next = if center + 1 == history_len {
            0
        } else {
            center + 1
        };

        let x = fraction;
        let x_minus_1 = x - 1.0;
        let x_plus_1 = x + 1.0;
        let x_plus_2 = x + 2.0;
        Self {
            tap_indices: [previous_2, previous, center, next],
            tap_weights: [
                -(x_plus_1 * x * x_minus_1) / 6.0,
                (x_plus_2 * x * x_minus_1) * 0.5,
                -(x_plus_2 * x_plus_1 * x_minus_1) * 0.5,
                (x_plus_2 * x_plus_1 * x) / 6.0,
            ],
            fraction,
            // A fractional four-tap read can reach three samples further
            // into retained history than the ceiling of its nominal delay.
            // The guard is conservative for integer delays, where all but the
            // center coefficient collapse to zero.
            required_history_samples: delay_samples.ceil() as usize + 3,
        }
    }

    fn apply(self, history: &[f32]) -> f32 {
        history[self.tap_indices[0]] * self.tap_weights[0]
            + history[self.tap_indices[1]] * self.tap_weights[1]
            + history[self.tap_indices[2]] * self.tap_weights[2]
            + history[self.tap_indices[3]] * self.tap_weights[3]
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
    fn zero_only_and_dirty_delay_resets_resume_identically() {
        use crate::echo_sidecar::EchoDelayRing;

        let mut delay = PropagationDelayLine::new(64, SAMPLE_RATE);
        let mut echo = EchoDelayRing::new(64);
        delay.reset_to(7.25);
        for _ in 0..256 {
            let _ = delay.process_sample(0.0);
            echo.push(0.0);
        }
        assert!(!delay.history_dirty);

        for _ in 0..2 {
            delay.reset_history();
            echo.reset();
            assert!(!delay.history_dirty);
            delay.reset_to(7.25);
            let mut fresh_delay = PropagationDelayLine::new(64, SAMPLE_RATE);
            let mut fresh_echo = EchoDelayRing::new(64);
            fresh_delay.reset_to(7.25);
            for frame in 0..256 {
                let input = match frame % 17 {
                    0 => 1.0,
                    1 => -0.5,
                    2 => -0.0,
                    _ => 0.0,
                };
                assert_eq!(
                    delay.process_sample(input).to_bits(),
                    fresh_delay.process_sample(input).to_bits()
                );
                echo.push(input);
                fresh_echo.push(input);
                assert_eq!(echo.read(7.25).to_bits(), fresh_echo.read(7.25).to_bits());
            }
            assert!(delay.history_dirty);
        }
    }

    #[test]
    fn bandlimited_boundary_uses_only_the_current_or_older_ring_samples() {
        const HISTORY_LEN: usize = 128;
        const WRITE_INDEX: usize = 5;
        let kernel = bandlimited_kernel_table();
        let below = VariableRateReadPlan::new(
            WRITE_INDEX,
            HISTORY_LEN,
            BANDLIMITED_NEWEST_OFFSET as f32 - 0.001,
            1.5,
            1.0,
            kernel,
        );
        assert!(below.bandlimited.is_none());

        let boundary = VariableRateReadPlan::new(
            WRITE_INDEX,
            HISTORY_LEN,
            BANDLIMITED_NEWEST_OFFSET as f32,
            1.5,
            1.0,
            kernel,
        )
        .bandlimited
        .expect("the exact guarded delay admits the causal bandlimited plan");
        let mut ages = boundary
            .tap_indices
            .map(|index| (WRITE_INDEX + HISTORY_LEN - index) % HISTORY_LEN);
        ages.sort_unstable();
        assert_eq!(
            ages,
            core::array::from_fn::<_, BANDLIMITED_TAPS, _>(|age| age)
        );
        assert_eq!(boundary.tap_indices[BANDLIMITED_TAPS - 1], WRITE_INDEX);
    }

    #[test]
    fn receding_and_stationary_reads_cannot_inherit_the_approach_filter() {
        let mut active = true;
        let mut transition_remaining = 0;
        update_bandlimited_readout_state(true, 0.1, &mut active, &mut transition_remaining);
        assert!(!active);
        assert_eq!(transition_remaining, 0);

        update_bandlimited_readout_state(true, -0.1, &mut active, &mut transition_remaining);
        assert!(active);
        assert_eq!(transition_remaining, BANDLIMITED_TRANSITION_FRAMES);

        update_bandlimited_readout_state(false, -0.1, &mut active, &mut transition_remaining);
        assert!(!active);
        assert_eq!(transition_remaining, 0);
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
            delay.observe_block_target_with_motion(
                distance * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND,
                speed_mps,
                speed_mps.abs(),
            );
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
        // reaches in the city fixture. Published velocity now makes the delay
        // line realize the reception-time textbook ratio directly.
        const HERTZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 10.0;

        let receding = doppler_hz(SPEED_MPS, 50.0);
        let approaching = doppler_hz(-SPEED_MPS, 400.0);

        let textbook = |speed: f32| HERTZ / (1.0 + speed / 343.0);
        assert!(
            (receding - textbook(SPEED_MPS)).abs() < 0.75,
            "recession measured {receding} Hz against textbook {} Hz",
            textbook(SPEED_MPS)
        );
        assert!(
            (approaching - textbook(-SPEED_MPS)).abs() < 0.75,
            "approach measured {approaching} Hz against textbook {} Hz",
            textbook(-SPEED_MPS)
        );
        assert!(
            receding < HERTZ && approaching > HERTZ,
            "recession {receding} Hz and approach {approaching} Hz did not \
             straddle the source frequency"
        );
    }

    /// Pins the exact model at the speed where position-only indexing used to
    /// have its largest documented error.
    #[test]
    fn extreme_speed_follows_the_reception_time_model_within_its_stated_error() {
        const HERTZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 34.3; // exactly c/10

        let measured = doppler_hz(SPEED_MPS, 50.0);

        let textbook = HERTZ / (1.0 + SPEED_MPS / 343.0);
        assert!(
            (measured - textbook).abs() < 0.75,
            "measured {measured} Hz against the exact model's {textbook} Hz"
        );
        let deviation = (measured - textbook).abs() / textbook;
        assert!(
            deviation < 0.001,
            "deviation from exact Doppler was {deviation}, above 0.1%"
        );
    }

    #[test]
    fn published_velocity_retires_the_position_only_second_order_error() {
        const HERTZ: f32 = 1_000.0;
        for (speed_mps, start_m) in [(20.0, 50.0), (34.3, 50.0)] {
            let measured = doppler_hz(speed_mps, start_m);
            let exact = HERTZ / (1.0 + speed_mps / SPEED_OF_SOUND_METERS_PER_SECOND);
            let relative_error = (measured - exact).abs() / exact;
            assert!(
                relative_error < 0.001,
                "{speed_mps} m/s measured {measured} Hz against {exact} Hz \
                 ({relative_error:.6} relative error)"
            );
        }
    }

    #[test]
    fn hundred_plus_meter_per_second_motion_stays_physical_before_the_safety_cap() {
        const HERTZ: f32 = 1_000.0;
        const SPEED_MPS: f32 = 110.0;
        let receding = doppler_hz(SPEED_MPS, 50.0);
        let approaching = doppler_hz(-SPEED_MPS, 500.0);
        let exact = |speed: f32| HERTZ / (1.0 + speed / SPEED_OF_SOUND_METERS_PER_SECOND);

        assert!(
            (receding - exact(SPEED_MPS)).abs() < 0.75,
            "110 m/s recession measured {receding} Hz against {} Hz",
            exact(SPEED_MPS),
        );
        assert!(
            (approaching - exact(-SPEED_MPS)).abs() < 0.75,
            "110 m/s approach measured {approaching} Hz against {} Hz",
            exact(-SPEED_MPS),
        );
    }

    #[derive(Debug, Default)]
    struct OffAxisPassMetrics {
        maximum_publication_correction_samples: f32,
        cap_hit_frames: usize,
        cap_hit_bursts: usize,
        maximum_cap_hit_burst_frames: usize,
        maximum_sample_pitch_jump: f32,
        maximum_cycle_pitch_jump: f64,
        minimum_cycle_pitch_ratio: f64,
        maximum_cycle_pitch_ratio: f64,
        measured_cycles: usize,
        missed_publications: usize,
    }

    #[derive(Clone, Copy)]
    enum OffAxisPublicationCadence {
        ExactSamples {
            phase_frames: usize,
        },
        QuantizedCallbackBlocks {
            phase_blocks: usize,
            miss_first_tick_after_closest_approach: bool,
        },
    }

    fn is_quantized_sixty_hz_publication(block: usize, phase_blocks: usize) -> bool {
        matches!((block + 25 - phase_blocks) % 25, 0 | 6 | 12 | 18)
    }

    fn off_axis_pass_metrics(publication_phase_frames: usize) -> OffAxisPassMetrics {
        off_axis_pass_metrics_for_cadence(OffAxisPublicationCadence::ExactSamples {
            phase_frames: publication_phase_frames,
        })
    }

    fn off_axis_pass_metrics_for_cadence(cadence: OffAxisPublicationCadence) -> OffAxisPassMetrics {
        const SPEED_MPS: f32 = 110.0;
        const CLOSEST_RANGE_M: f32 = 30.0;
        const CLOSEST_TIME_SECONDS: f32 = 2.5;
        const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
        const TOTAL_FRAMES: usize = SAMPLE_RATE as usize * 5;
        const MEASURE_FROM_FRAME: usize = SAMPLE_RATE as usize;
        const SOURCE_HZ: f64 = 1_000.0;
        match cadence {
            OffAxisPublicationCadence::ExactSamples { phase_frames } => {
                assert!(phase_frames < PUBLICATION_FRAMES);
            }
            OffAxisPublicationCadence::QuantizedCallbackBlocks { phase_blocks, .. } => {
                assert!(phase_blocks < 25);
            }
        }

        let mut delay = PropagationDelayLine::new(600_000, SAMPLE_RATE);
        let mut metrics = OffAxisPassMetrics {
            minimum_cycle_pitch_ratio: f64::INFINITY,
            ..OffAxisPassMetrics::default()
        };
        let mut previous_delay: Option<f32> = None;
        let mut previous_sample_pitch: Option<f32> = None;
        let mut current_cap_hit_burst = 0_usize;
        let mut previous_output: Option<f32> = None;
        let mut previous_crossing: Option<f64> = None;
        let mut previous_cycle_pitch: Option<f64> = None;
        let mut missed_publication = false;

        for frame in 0..TOTAL_FRAMES {
            let scheduled_publication = match cadence {
                OffAxisPublicationCadence::ExactSamples { phase_frames } => {
                    frame >= phase_frames && (frame - phase_frames) % PUBLICATION_FRAMES == 0
                }
                OffAxisPublicationCadence::QuantizedCallbackBlocks {
                    phase_blocks,
                    miss_first_tick_after_closest_approach,
                } => {
                    let scheduled = frame % 128 == 0
                        && is_quantized_sixty_hz_publication(frame / 128, phase_blocks);
                    if scheduled
                        && miss_first_tick_after_closest_approach
                        && !missed_publication
                        && frame >= (CLOSEST_TIME_SECONDS * SAMPLE_RATE as f32) as usize
                    {
                        missed_publication = true;
                        metrics.missed_publications += 1;
                        false
                    } else {
                        scheduled
                    }
                }
            };
            if scheduled_publication {
                let seconds = frame as f32 / SAMPLE_RATE as f32;
                let along_track_m = SPEED_MPS * (seconds - CLOSEST_TIME_SECONDS);
                let distance_m =
                    (along_track_m * along_track_m + CLOSEST_RANGE_M * CLOSEST_RANGE_M).sqrt();
                let radial_velocity_mps = SPEED_MPS * along_track_m / distance_m;
                let raw_delay_samples =
                    distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
                if delay.initialized {
                    metrics.maximum_publication_correction_samples =
                        metrics.maximum_publication_correction_samples.max(
                            (f64::from(raw_delay_samples) - delay.geometry_delay_samples).abs()
                                as f32,
                        );
                }
                delay.observe_block_target_with_motion(
                    raw_delay_samples,
                    radial_velocity_mps,
                    SPEED_MPS,
                );
            }

            let phase = core::f64::consts::TAU * SOURCE_HZ * frame as f64 / f64::from(SAMPLE_RATE);
            let output = delay.process_sample(phase.sin() as f32);
            let current_delay = delay.current_delay_samples();

            if frame >= MEASURE_FROM_FRAME {
                if let Some(previous) = previous_delay {
                    let delay_step = current_delay - previous;
                    let sample_pitch = 1.0 - delay_step;
                    if let Some(previous_pitch) = previous_sample_pitch {
                        metrics.maximum_sample_pitch_jump = metrics
                            .maximum_sample_pitch_jump
                            .max((sample_pitch - previous_pitch).abs());
                    }
                    previous_sample_pitch = Some(sample_pitch);

                    if delay_step.abs() >= 0.49 {
                        metrics.cap_hit_frames += 1;
                        if current_cap_hit_burst == 0 {
                            metrics.cap_hit_bursts += 1;
                        }
                        current_cap_hit_burst += 1;
                        metrics.maximum_cap_hit_burst_frames = metrics
                            .maximum_cap_hit_burst_frames
                            .max(current_cap_hit_burst);
                    } else {
                        current_cap_hit_burst = 0;
                    }
                }

                if let Some(previous) = previous_output
                    && previous <= 0.0
                    && output > 0.0
                {
                    let crossing_fraction = -f64::from(previous) / f64::from(output - previous);
                    let crossing = frame as f64 - 1.0 + crossing_fraction;
                    if let Some(last_crossing) = previous_crossing {
                        let pitch_ratio =
                            (f64::from(SAMPLE_RATE) / SOURCE_HZ) / (crossing - last_crossing);
                        metrics.minimum_cycle_pitch_ratio =
                            metrics.minimum_cycle_pitch_ratio.min(pitch_ratio);
                        metrics.maximum_cycle_pitch_ratio =
                            metrics.maximum_cycle_pitch_ratio.max(pitch_ratio);
                        if let Some(previous_pitch) = previous_cycle_pitch {
                            metrics.maximum_cycle_pitch_jump = metrics
                                .maximum_cycle_pitch_jump
                                .max((pitch_ratio - previous_pitch).abs());
                        }
                        previous_cycle_pitch = Some(pitch_ratio);
                        metrics.measured_cycles += 1;
                    }
                    previous_crossing = Some(crossing);
                }
                previous_output = Some(output);
            }
            previous_delay = Some(current_delay);
        }

        metrics
    }

    #[test]
    fn phase_shifted_sparse_off_axis_fast_passes_have_no_publication_pitch_pulses() {
        for publication_phase_frames in [0_usize, 137, 399, 799] {
            let metrics = off_axis_pass_metrics(publication_phase_frames);
            println!(
                "FAST_MOVER_OFF_AXIS speed_mps=110.0 closest_range_m=30.0 publication_hz=60 publication_phase_frames={publication_phase_frames} maximum_publication_correction_samples={:.6} cap_hit_frames={} cap_hit_bursts={} maximum_cap_hit_burst_frames={} maximum_sample_pitch_jump={:.6} maximum_cycle_pitch_jump={:.6} minimum_cycle_pitch_ratio={:.6} maximum_cycle_pitch_ratio={:.6} measured_cycles={}",
                metrics.maximum_publication_correction_samples,
                metrics.cap_hit_frames,
                metrics.cap_hit_bursts,
                metrics.maximum_cap_hit_burst_frames,
                metrics.maximum_sample_pitch_jump,
                metrics.maximum_cycle_pitch_jump,
                metrics.minimum_cycle_pitch_ratio,
                metrics.maximum_cycle_pitch_ratio,
                metrics.measured_cycles,
            );
            assert!(
                metrics.maximum_publication_correction_samples < 0.05,
                "phase {publication_phase_frames} corrected geometry by {} samples",
                metrics.maximum_publication_correction_samples
            );
            assert_eq!(
                metrics.cap_hit_frames, 0,
                "phase {publication_phase_frames} engaged the read-head cap"
            );
            assert_eq!(
                metrics.cap_hit_bursts, 0,
                "phase {publication_phase_frames} pulsed the read-head cap"
            );
            assert!(
                metrics.maximum_sample_pitch_jump < 0.02,
                "phase {publication_phase_frames} jumped sample pitch by {}",
                metrics.maximum_sample_pitch_jump
            );
            assert!(
                metrics.maximum_cycle_pitch_jump < 0.003,
                "phase {publication_phase_frames} jumped cycle pitch by {}",
                metrics.maximum_cycle_pitch_jump
            );
            assert!(metrics.measured_cycles > 3_000);
            assert!(metrics.minimum_cycle_pitch_ratio < 0.85);
            assert!(metrics.maximum_cycle_pitch_ratio > 1.25);
        }
    }

    #[test]
    fn callback_quantized_sixty_hz_with_a_missed_tick_has_no_pitch_pulses() {
        let first_supercycle: Vec<_> = (0..=25)
            .filter(|block| is_quantized_sixty_hz_publication(*block, 0))
            .collect();
        assert_eq!(first_supercycle, [0, 6, 12, 18, 25]);

        for publication_phase_blocks in [0_usize, 1, 3, 5] {
            let metrics = off_axis_pass_metrics_for_cadence(
                OffAxisPublicationCadence::QuantizedCallbackBlocks {
                    phase_blocks: publication_phase_blocks,
                    miss_first_tick_after_closest_approach: true,
                },
            );
            println!(
                "FAST_MOVER_CALLBACK_CADENCE speed_mps=110.0 closest_range_m=30.0 callback_frames=128 cadence_frames=768/768/768/896 publication_phase_blocks={publication_phase_blocks} missed_ticks={} maximum_publication_correction_samples={:.6} cap_hit_frames={} cap_hit_bursts={} maximum_cap_hit_burst_frames={} maximum_sample_pitch_jump={:.6} maximum_cycle_pitch_jump={:.6}",
                metrics.missed_publications,
                metrics.maximum_publication_correction_samples,
                metrics.cap_hit_frames,
                metrics.cap_hit_bursts,
                metrics.maximum_cap_hit_burst_frames,
                metrics.maximum_sample_pitch_jump,
                metrics.maximum_cycle_pitch_jump,
            );
            assert_eq!(
                metrics.missed_publications, 1,
                "phase {publication_phase_blocks} did not skip exactly one publication"
            );
            assert!(
                metrics.maximum_publication_correction_samples < 0.1,
                "phase {publication_phase_blocks} corrected geometry by {} samples",
                metrics.maximum_publication_correction_samples
            );
            assert_eq!(
                metrics.cap_hit_frames, 0,
                "phase {publication_phase_blocks} engaged the read-head cap"
            );
            assert_eq!(
                metrics.cap_hit_bursts, 0,
                "phase {publication_phase_blocks} pulsed the read-head cap"
            );
            assert!(
                metrics.maximum_sample_pitch_jump < 0.02,
                "phase {publication_phase_blocks} jumped sample pitch by {}",
                metrics.maximum_sample_pitch_jump
            );
            assert!(
                metrics.maximum_cycle_pitch_jump < 0.003,
                "phase {publication_phase_blocks} jumped cycle pitch by {}",
                metrics.maximum_cycle_pitch_jump
            );
        }
    }

    #[test]
    fn maximum_horizon_clamps_exposure_without_clamping_outward_or_inward_geometry() {
        const MAXIMUM_DELAY_SAMPLES: usize = 2_000;
        const SPEED_MPS: f32 = 110.0;
        const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
        const OUTWARD_BLOCKS: usize = 4;
        const INWARD_BLOCKS: usize = 7;
        let geometry_rate = f64::from(SPEED_MPS) / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND);
        let mut analytic_delay = 1_900.0_f64;
        let mut delay = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
        let mut maximum_publication_correction = 0.0_f64;

        for block in 0..OUTWARD_BLOCKS {
            if block > 0 {
                maximum_publication_correction = maximum_publication_correction
                    .max((analytic_delay - delay.geometry_delay_samples).abs());
            }
            delay.observe_block_target_with_motion(analytic_delay as f32, SPEED_MPS, SPEED_MPS);
            for _ in 0..PUBLICATION_FRAMES {
                let _ = delay.process_sample(0.0);
                analytic_delay += geometry_rate;
            }
        }

        assert!(
            delay.geometry_delay_samples > MAXIMUM_DELAY_SAMPLES as f64 + 800.0,
            "outward analytic geometry stopped at {}",
            delay.geometry_delay_samples
        );
        assert_eq!(
            delay.current_delay_samples().to_bits(),
            (MAXIMUM_DELAY_SAMPLES as f32).to_bits()
        );
        assert!(delay.geometry_history.iter().all(|sample| {
            sample.is_finite() && (0.0..=MAXIMUM_DELAY_SAMPLES as f32).contains(sample)
        }));

        let mut previous_applied = delay.current_delay_samples();
        let mut cap_hit_frames = 0_usize;
        for _ in 0..INWARD_BLOCKS {
            maximum_publication_correction = maximum_publication_correction
                .max((analytic_delay - delay.geometry_delay_samples).abs());
            delay.observe_block_target_with_motion(analytic_delay as f32, -SPEED_MPS, SPEED_MPS);
            for _ in 0..PUBLICATION_FRAMES {
                let _ = delay.process_sample(0.0);
                analytic_delay -= geometry_rate;
                let applied = delay.current_delay_samples();
                if (applied - previous_applied).abs() >= 0.49 {
                    cap_hit_frames += 1;
                }
                previous_applied = applied;
            }
        }

        println!(
            "FAST_MOVER_MAX_HORIZON maximum_delay_samples={MAXIMUM_DELAY_SAMPLES} maximum_publication_correction_samples={maximum_publication_correction:.6} final_analytic_delay_samples={:.6} final_exposed_delay_samples={:.6} inward_cap_hit_frames={cap_hit_frames}",
            delay.geometry_delay_samples,
            delay.current_delay_samples(),
        );
        assert!(maximum_publication_correction < 0.01);
        assert!(delay.geometry_delay_samples < MAXIMUM_DELAY_SAMPLES as f64);
        assert!(delay.current_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
        assert_eq!(cap_hit_frames, 0);
        assert!(delay.geometry_history.iter().all(|sample| {
            sample.is_finite() && (0.0..=MAXIMUM_DELAY_SAMPLES as f32).contains(sample)
        }));
    }

    #[test]
    fn bounded_acceleration_and_subsample_publication_jitter_do_not_burst_the_cap() {
        const START_SPEED_MPS: f32 = 70.0;
        const ACCELERATION_MPS2: f32 = 20.0;
        const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
        const TOTAL_FRAMES: usize = SAMPLE_RATE as usize * 2;
        const MEASURE_FROM_FRAME: usize = SAMPLE_RATE as usize * 3 / 2;
        const JITTER_SAMPLES: [f32; 4] = [0.04, -0.04, 0.02, -0.02];
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut maximum_publication_correction = 0.0_f32;
        let mut cap_hit_frames = 0_usize;
        let mut maximum_sample_pitch_jump = 0.0_f32;
        let mut previous_delay: Option<f32> = None;
        let mut previous_pitch: Option<f32> = None;

        for frame in 0..TOTAL_FRAMES {
            if frame % PUBLICATION_FRAMES == 0 {
                let seconds = frame as f32 / SAMPLE_RATE as f32;
                let speed_mps = START_SPEED_MPS + ACCELERATION_MPS2 * seconds;
                let distance_m =
                    200.0 - START_SPEED_MPS * seconds - 0.5 * ACCELERATION_MPS2 * seconds * seconds;
                let jitter = JITTER_SAMPLES[(frame / PUBLICATION_FRAMES) % JITTER_SAMPLES.len()];
                let raw_delay =
                    distance_m * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND + jitter;
                if delay.initialized {
                    maximum_publication_correction = maximum_publication_correction
                        .max((f64::from(raw_delay) - delay.geometry_delay_samples).abs() as f32);
                }
                delay.observe_block_target_with_motion(raw_delay, -speed_mps, speed_mps);
            }

            let _ = delay.process_sample(0.0);
            let current = delay.current_delay_samples();
            if frame >= MEASURE_FROM_FRAME
                && let Some(previous) = previous_delay
            {
                let step = current - previous;
                if step.abs() >= 0.49 {
                    cap_hit_frames += 1;
                }
                let pitch = 1.0 - step;
                if let Some(previous) = previous_pitch {
                    maximum_sample_pitch_jump =
                        maximum_sample_pitch_jump.max((pitch - previous).abs());
                }
                previous_pitch = Some(pitch);
            }
            previous_delay = Some(current);
        }

        println!(
            "FAST_MOVER_BOUNDED_ACCELERATION acceleration_mps2={ACCELERATION_MPS2:.1} publication_jitter_samples=0.04 maximum_publication_correction_samples={maximum_publication_correction:.6} cap_hit_frames={cap_hit_frames} maximum_sample_pitch_jump={maximum_sample_pitch_jump:.6}"
        );
        assert!(maximum_publication_correction < 0.75);
        assert_eq!(cap_hit_frames, 0);
        assert!(maximum_sample_pitch_jump < 0.02);
    }

    #[test]
    fn a10_class_approach_delivers_the_gamma_pitch_ratio_without_reversing() {
        const SPEED_MPS: f32 = -167.0;
        const BLOCK_FRAMES: usize = 128;
        const TOTAL_BLOCKS: usize = 1_500;
        let mut delay = PropagationDelayLine::new(600_000, SAMPLE_RATE);
        let mut maximum_step = 0.0_f32;
        let mut previous_delay = None;
        let mut minimum_emission_advance = f32::INFINITY;
        let mut previous_emission_position = None;
        let mut accumulated_emission_advance = 0.0_f64;
        let mut measured_emission_advances = 0_u64;

        for block in 0..TOTAL_BLOCKS {
            let seconds = block as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
            let distance = 1_000.0 + SPEED_MPS * seconds;
            let raw = distance * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
            delay.observe_block_target_with_motion(raw, SPEED_MPS, SPEED_MPS.abs());
            for frame in 0..BLOCK_FRAMES {
                let sample = block * BLOCK_FRAMES + frame;
                let _ = delay.process_sample(0.0);
                if sample > SAMPLE_RATE as usize * 3 {
                    if let Some(previous) = previous_delay.replace(delay.applied_delay_samples) {
                        maximum_step =
                            maximum_step.max((delay.applied_delay_samples - previous).abs());
                    }
                    let emission_position = sample as f32 - delay.applied_delay_samples;
                    if let Some(previous) = previous_emission_position.replace(emission_position) {
                        let advance = emission_position - previous;
                        minimum_emission_advance = minimum_emission_advance.min(advance);
                        accumulated_emission_advance += f64::from(advance);
                        measured_emission_advances += 1;
                    }
                }
            }
        }

        let exact_pitch = 1.0 / (1.0 + SPEED_MPS / SPEED_OF_SOUND_METERS_PER_SECOND);
        let delivered_pitch =
            (accumulated_emission_advance / measured_emission_advances as f64) as f32;
        let pitch_error_cents = 1_200.0 * (delivered_pitch / exact_pitch).log2().abs();
        println!(
            "FAST_MOVER_A10 approach_mps={:.1} exact_pitch_ratio={exact_pitch:.6} delivered_pitch_ratio={delivered_pitch:.6} pitch_error_cents={pitch_error_cents:.3} maximum_delay_step_samples={maximum_step:.6} minimum_emission_advance_samples={minimum_emission_advance:.6}",
            -SPEED_MPS,
        );
        assert!(
            pitch_error_cents <= 10.0,
            "167 m/s approach missed the gamma pitch gate by {pitch_error_cents:.3} cents"
        );
        assert!(maximum_step <= MAX_BANDLIMITED_APPROACH_SLEW_SAMPLES_PER_SAMPLE);
        assert!(minimum_emission_advance >= 1.0);
    }

    fn bandlimited_resampled_tone_rms(input_hz: f64, read_rate: f32) -> f64 {
        const HISTORY_LEN: usize = 16_384;
        const OUTPUT_FRAMES: usize = 8_192;
        const START_DELAY_SAMPLES: f32 = 10_000.0;
        let sample = |frame: isize| {
            (core::f64::consts::TAU * input_hz * frame as f64 / f64::from(SAMPLE_RATE)).sin() as f32
        };
        let mut history = vec![0.0_f32; HISTORY_LEN];
        for (index, value) in history.iter_mut().enumerate().skip(1) {
            *value = sample(index as isize - HISTORY_LEN as isize);
        }
        let kernel = bandlimited_kernel_table();
        let mut write_index = 0_usize;
        let mut delay_samples = START_DELAY_SAMPLES;
        let mut energy = 0.0_f64;
        let mut peak = 0.0_f32;
        for frame in 0..OUTPUT_FRAMES {
            history[write_index] = sample(frame as isize);
            let output = BandlimitedReadPlan::new(
                write_index,
                HISTORY_LEN,
                delay_samples,
                read_rate,
                kernel,
            )
            .apply(&history);
            assert!(output.is_finite());
            energy += f64::from(output) * f64::from(output);
            peak = peak.max(output.abs());
            write_index += 1;
            delay_samples -= read_rate - 1.0;
        }
        assert!(delay_samples > BANDLIMITED_NEWEST_OFFSET as f32);
        assert!(peak <= 1.01, "polyphase tone peak escaped unity: {peak}");
        (energy / OUTPUT_FRAMES as f64).sqrt()
    }

    #[test]
    fn a10_rate_aware_kernel_rejects_the_fold_band_below_minus_60_db() {
        const READ_RATE: f32 = 1.948_864;
        let wanted_rms = bandlimited_resampled_tone_rms(8_000.0, READ_RATE);
        let rejected_rms = bandlimited_resampled_tone_rms(18_000.0, READ_RATE);
        let rejection_db = 20.0 * (rejected_rms / wanted_rms).log10();
        println!(
            "WP3_A10_SPECTRAL read_rate={READ_RATE:.6} wanted_8000_hz_rms={wanted_rms:.9} rejected_18000_hz_rms={rejected_rms:.9} rejected_to_wanted_db={rejection_db:.3}"
        );
        assert!(wanted_rms >= 0.65, "8 kHz passband collapsed");
        assert!(
            rejection_db <= -60.0,
            "18 kHz fold-band rejection was only {rejection_db:.3} dB"
        );
    }

    #[test]
    fn moving_filter_transition_is_finite_peak_bounded_and_click_free() {
        const INITIAL_DELAY_SAMPLES: f32 = 20_000.0;
        const PREHISTORY_FRAMES: usize = 24_000;
        const MOTION_FRAMES: usize = 30_000;
        const BLOCK_FRAMES: usize = 128;
        const SPEED_MPS: f32 = -167.0;
        const TONE_HZ: f32 = 1_000.0;
        let mut delay = PropagationDelayLine::new(100_000, SAMPLE_RATE);
        delay.observe_block_target(INITIAL_DELAY_SAMPLES);
        for frame in 0..PREHISTORY_FRAMES {
            let _ = delay.process_sample(tone(frame, TONE_HZ));
        }

        let geometry_rate = SPEED_MPS / SPEED_OF_SOUND_METERS_PER_SECOND;
        let mut previous = delay.process_sample(tone(PREHISTORY_FRAMES, TONE_HZ));
        let mut maximum_peak = previous.abs();
        let mut maximum_adjacent_jump = 0.0_f32;
        for frame in 0..MOTION_FRAMES {
            if frame % BLOCK_FRAMES == 0 {
                let raw_delay = INITIAL_DELAY_SAMPLES + geometry_rate * frame as f32;
                delay.observe_block_target_with_motion(raw_delay, SPEED_MPS, SPEED_MPS.abs());
            }
            let output = delay.process_sample(tone(PREHISTORY_FRAMES + 1 + frame, TONE_HZ));
            assert!(output.is_finite());
            maximum_peak = maximum_peak.max(output.abs());
            maximum_adjacent_jump = maximum_adjacent_jump.max((output - previous).abs());
            previous = output;
        }

        println!(
            "WP3_FILTER_TRANSITION approach_mps={:.1} transition_frames={} maximum_peak={maximum_peak:.6} maximum_adjacent_jump={maximum_adjacent_jump:.6}",
            -SPEED_MPS, BANDLIMITED_TRANSITION_FRAMES,
        );
        assert!(delay.bandlimited_readout_active);
        assert!(maximum_peak <= 1.05);
        assert!(
            maximum_adjacent_jump <= 0.35,
            "moving-filter transition produced a click-sized jump"
        );
    }

    #[derive(Clone, Copy)]
    struct Point3 {
        x: f32,
        y: f32,
        z: f32,
    }

    impl Point3 {
        const fn new(x: f32, y: f32, z: f32) -> Self {
            Self { x, y, z }
        }
    }

    fn subtract(left: Point3, right: Point3) -> Point3 {
        Point3::new(left.x - right.x, left.y - right.y, left.z - right.z)
    }

    fn length(vector: Point3) -> f32 {
        (vector.x * vector.x + vector.y * vector.y + vector.z * vector.z).sqrt()
    }

    fn first_square_corner_state(
        waypoints: [Point3; 4],
        speed_mps: f32,
        seconds: f32,
    ) -> (Point3, Point3) {
        let first_leg = length(subtract(waypoints[1], waypoints[0]));
        let (start, end) = if seconds * speed_mps < first_leg {
            (waypoints[0], waypoints[1])
        } else {
            (waypoints[1], waypoints[2])
        };
        let direction = subtract(end, start);
        let direction_length = length(direction);
        let unit = Point3::new(
            direction.x / direction_length,
            direction.y / direction_length,
            direction.z / direction_length,
        );
        let leg_seconds = if start.x == waypoints[0].x
            && start.y == waypoints[0].y
            && start.z == waypoints[0].z
        {
            seconds
        } else {
            seconds - first_leg / speed_mps
        };
        (
            Point3::new(
                start.x + unit.x * speed_mps * leg_seconds,
                start.y + unit.y * speed_mps * leg_seconds,
                start.z + unit.z * speed_mps * leg_seconds,
            ),
            Point3::new(unit.x * speed_mps, unit.y * speed_mps, unit.z * speed_mps),
        )
    }

    fn corner_metrics(speed_mps: f32) -> (f32, f32, f32) {
        const OBSERVATION_SAMPLES: usize = 800;
        let source_waypoints = [
            Point3::new(102.5, 102.5, 55.0),
            Point3::new(482.5, 102.5, 55.0),
            Point3::new(482.5, 482.5, 55.0),
            Point3::new(102.5, 482.5, 55.0),
        ];
        let listener_waypoints = [
            Point3::new(197.5, 292.5, 1.5),
            Point3::new(292.5, 292.5, 1.5),
            Point3::new(292.5, 387.5, 1.5),
            Point3::new(197.5, 387.5, 1.5),
        ];
        let corner_seconds = 380.0 / speed_mps;
        let sample_count = ((corner_seconds + 2.0) * SAMPLE_RATE as f32) as usize;
        let motion_measurement_start =
            ((corner_seconds - 0.5).max(0.0) * SAMPLE_RATE as f32) as usize;
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut maximum_geometry_correction = 0.0_f32;
        let mut maximum_applied_step = 0.0_f32;
        let mut maximum_tracking_error = 0.0_f32;
        let mut previous_delay = None;
        let mut previous_emission_position = None;

        for sample in 0..sample_count {
            if sample % OBSERVATION_SAMPLES == 0 {
                let seconds = sample as f32 / SAMPLE_RATE as f32;
                let (source, source_velocity) =
                    first_square_corner_state(source_waypoints, speed_mps, seconds);
                let (listener, listener_velocity) =
                    first_square_corner_state(listener_waypoints, 1.5, seconds);
                let offset = subtract(source, listener);
                let distance = length(offset);
                let relative_velocity = subtract(source_velocity, listener_velocity);
                let radial_velocity = (relative_velocity.x * offset.x
                    + relative_velocity.y * offset.y
                    + relative_velocity.z * offset.z)
                    / distance;
                let relative_speed = length(relative_velocity);
                let raw_delay = distance * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
                if delay.initialized {
                    maximum_geometry_correction = maximum_geometry_correction
                        .max((f64::from(raw_delay) - delay.geometry_delay_samples).abs() as f32);
                }
                delay.observe_block_target_with_motion(raw_delay, radial_velocity, relative_speed);
            }

            let _ = delay.process_sample(0.0);
            if let Some(previous) = previous_delay.replace(delay.applied_delay_samples)
                && sample >= motion_measurement_start
            {
                maximum_applied_step =
                    maximum_applied_step.max((delay.applied_delay_samples - previous).abs());
            }
            if sample >= motion_measurement_start {
                maximum_tracking_error = maximum_tracking_error
                    .max((delay.target_delay_samples - delay.applied_delay_samples).abs());
            }
            let emission_position = sample as f32 - delay.applied_delay_samples;
            assert!(emission_position <= sample as f32 + f32::EPSILON);
            if let Some(previous) = previous_emission_position.replace(emission_position) {
                assert!(
                    emission_position > previous,
                    "read head reversed at sample {sample}: {previous} -> {emission_position}"
                );
            }
            assert!(!delay.is_crossfading());
        }

        (
            maximum_geometry_correction,
            maximum_applied_step,
            maximum_tracking_error,
        )
    }

    #[test]
    fn ninety_degree_orbit_corner_keeps_the_causal_read_head_continuous() {
        for speed_mps in [10.0_f32, 20.0, 30.0, 40.0] {
            let (anchor_delta, applied_step, tracking_error) = corner_metrics(speed_mps);
            println!(
                "FAST_MOVER_DELAY speed_mps={speed_mps:.1} geometry_correction_samples={anchor_delta:.6} maximum_applied_step_samples={applied_step:.6} maximum_tracking_error_samples={tracking_error:.6}"
            );
            assert!(
                anchor_delta < 1.0,
                "{speed_mps} m/s geometry publication corrected by {anchor_delta} samples"
            );
            assert!(
                applied_step < 0.2,
                "{speed_mps} m/s causal target stepped {applied_step} samples"
            );
            assert!(
                tracking_error < 1.0e-3,
                "slew safety became the primary mechanism at {speed_mps} m/s: {tracking_error} samples"
            );
        }
    }

    #[test]
    fn slow_walk_output_stays_close_to_the_prechange_velocity_model() {
        const SPEED_MPS: f32 = 1.5;
        const HERTZ: f32 = 440.0;
        const BLOCK_FRAMES: usize = 128;
        const TOTAL_BLOCKS: usize = 1_200;
        const MEASURE_FROM_BLOCK: usize = 750;
        let retention = (-1.0 / (PROPAGATION_SLEW_TIME_SECONDS * SAMPLE_RATE as f32)).exp();
        let ratio = 1.0 / (1.0 + SPEED_MPS / SPEED_OF_SOUND_METERS_PER_SECOND);
        let rate = (1.0 - ratio).clamp(
            -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
        );
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut legacy_applied = 0.0_f32;
        let mut legacy_target = 0.0_f32;
        let mut final_legacy_anchor = 0.0_f32;
        let mut initialized = false;
        let mut maximum_delay_delta = 0.0_f32;
        let mut squared_output_delta = 0.0_f64;
        let mut measured = 0_usize;

        for block in 0..TOTAL_BLOCKS {
            let seconds = block as f32 * BLOCK_FRAMES as f32 / SAMPLE_RATE as f32;
            let distance = 50.0 + SPEED_MPS * seconds;
            let raw = distance * SAMPLE_RATE as f32 / SPEED_OF_SOUND_METERS_PER_SECOND;
            delay.observe_block_target_with_motion(raw, SPEED_MPS, SPEED_MPS);
            let mut legacy_anchor = raw * ratio;
            if !initialized {
                legacy_applied = raw;
                legacy_target = raw;
                initialized = true;
            }

            for frame in 0..BLOCK_FRAMES {
                let _ = delay.process_sample(0.0);
                legacy_anchor += rate;
                let integrated_target = legacy_target + rate;
                legacy_target = legacy_anchor + (integrated_target - legacy_anchor) * retention;
                let one_pole = legacy_target + (legacy_applied - legacy_target) * retention;
                legacy_applied += (one_pole - legacy_applied).clamp(
                    -MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                    MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                );

                if block >= MEASURE_FROM_BLOCK {
                    let delay_delta = delay.applied_delay_samples - legacy_applied;
                    maximum_delay_delta = maximum_delay_delta.max(delay_delta.abs());
                    let sample = block * BLOCK_FRAMES + frame;
                    let radians_per_sample = TAU * HERTZ / SAMPLE_RATE as f32;
                    let actual =
                        ((sample as f32 - delay.applied_delay_samples) * radians_per_sample).sin();
                    let legacy = ((sample as f32 - legacy_applied) * radians_per_sample).sin();
                    squared_output_delta += f64::from((actual - legacy) * (actual - legacy));
                    measured += 1;
                }
            }
            final_legacy_anchor = legacy_anchor;
        }

        let output_delta_rms = (squared_output_delta / measured as f64).sqrt();
        println!(
            "FAST_MOVER_SLOW_CONTROL speed_mps={SPEED_MPS:.1} maximum_delay_delta_samples={maximum_delay_delta:.6} tone_output_delta_rms={output_delta_rms:.9} final_actual_delay={:.6} final_actual_target={:.6} final_legacy_delay={legacy_applied:.6} final_legacy_target={legacy_target:.6} final_legacy_anchor={final_legacy_anchor:.6}",
            delay.applied_delay_samples, delay.target_delay_samples,
        );
        assert!(maximum_delay_delta < 0.02);
        assert!(output_delta_rms < 0.001);
    }

    #[test]
    fn finite_zero_relative_speed_is_bit_exact_with_changing_position_only_behavior() {
        let mut position_only = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut zero_velocity = PropagationDelayLine::new(300_000, SAMPLE_RATE);

        for block in 0..300 {
            let target = 4_800.0 + block as f32 * 0.25;
            position_only.observe_block_target(target);
            zero_velocity.observe_block_target_with_zero_motion(target);
            for frame in 0..128 {
                let input = tone(block * 128 + frame, 440.0);
                assert_eq!(
                    position_only.process_sample(input).to_bits(),
                    zero_velocity.process_sample(input).to_bits()
                );
            }
            assert_eq!(
                position_only.current_delay_samples().to_bits(),
                zero_velocity.current_delay_samples().to_bits()
            );
        }
        assert!(!zero_velocity.retarded_time_guided);
        assert!(!zero_velocity.fast_motion_guided);
    }

    #[test]
    fn cold_constant_velocity_root_handles_zero_radial_and_clamped_speed() {
        let delay = 4_800.0_f64;
        let squared_delay = delay * delay;
        let root = |radial_rate: f64, speed_rate: f64| {
            let speed_squared = speed_rate * speed_rate;
            constant_velocity_retarded_delay_samples(
                squared_delay,
                2.0 * delay * radial_rate + speed_squared,
                2.0 * speed_squared,
            )
        };

        assert_eq!(root(0.0, 0.0).to_bits(), delay.to_bits());
        assert!((root(0.1, 0.1) - delay / 1.1).abs() < 1.0e-9);
        assert!((root(-0.5, 0.5) - delay / 0.5).abs() < 1.0e-9);
        assert!((root(0.0, 0.5) - delay / 0.75_f64.sqrt()).abs() < 1.0e-9);
        assert_eq!(constant_velocity_retarded_delay_samples(0.0, 0.0, 0.0), 0.0);
    }

    #[test]
    fn far_to_far_horizon_teleport_rebases_before_correct_inward_reentry() {
        const MAXIMUM_DELAY_SAMPLES: usize = 10_000;
        const SPEED_MPS: f32 = 110.0;
        const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
        let mut delay = PropagationDelayLine::new(MAXIMUM_DELAY_SAMPLES, SAMPLE_RATE);
        delay.observe_block_target_with_motion(15_000.0, SPEED_MPS, SPEED_MPS);
        for _ in 0..128 {
            let _ = delay.process_sample(0.0);
        }
        assert_eq!(
            delay.previous_raw_target_samples.to_bits(),
            15_000.0_f64.to_bits()
        );
        assert_eq!(
            delay.current_delay_samples().to_bits(),
            (MAXIMUM_DELAY_SAMPLES as f32).to_bits()
        );

        let mut analytic_geometry = 18_000.0_f64;
        delay.observe_block_target_with_motion(analytic_geometry as f32, -SPEED_MPS, SPEED_MPS);
        assert!(delay.is_crossfading());
        assert_eq!(
            delay.geometry_delay_samples.to_bits(),
            analytic_geometry.to_bits()
        );
        assert_eq!(
            delay.previous_raw_target_samples.to_bits(),
            analytic_geometry.to_bits(),
            "the far-to-far observation was stored only at the exposed cap"
        );

        let geometry_rate = f64::from(SPEED_MPS) / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND);
        let mut reentered = false;
        let mut previous_applied = delay.current_delay_samples();
        let mut cap_hit_frames = 0_usize;
        for frame in 0..45_000 {
            if frame > 0 && frame % PUBLICATION_FRAMES == 0 {
                delay.observe_block_target_with_motion(
                    analytic_geometry as f32,
                    -SPEED_MPS,
                    SPEED_MPS,
                );
            }
            let _ = delay.process_sample(0.0);
            analytic_geometry -= geometry_rate;
            let applied = delay.current_delay_samples();
            if applied < MAXIMUM_DELAY_SAMPLES as f32 {
                reentered = true;
            }
            if reentered && (applied - previous_applied).abs() >= 0.49 {
                cap_hit_frames += 1;
            }
            previous_applied = applied;
        }

        assert!(reentered, "the inward trajectory never re-entered the ring");
        assert!(delay.geometry_delay_samples < MAXIMUM_DELAY_SAMPLES as f64);
        assert!(delay.current_delay_samples() < MAXIMUM_DELAY_SAMPLES as f32);
        assert_eq!(cap_hit_frames, 0);
    }

    #[test]
    fn cold_root_hands_off_continuously_and_is_skipped_with_deep_history() {
        const SPEED_MPS: f32 = 110.0;
        let mut delay = PropagationDelayLine::new(1_024, SAMPLE_RATE);
        delay.observe_block_target_with_motion(64.0, SPEED_MPS, SPEED_MPS);
        let mut cold_frames = 0_usize;
        let mut handoffs = 0_usize;

        for _ in 0..512 {
            // `process_sample` records once before solving, so the pre-call
            // sample count is exactly the lookback available to that solve.
            let uses_cold_root =
                delay.target_delay_samples.ceil() as usize > delay.geometry_history_samples;
            let analytic_target =
                delay.expose_geometry_delay(constant_velocity_retarded_delay_samples(
                    delay.geometry_delay_squared_samples,
                    delay.geometry_squared_step_samples,
                    delay.geometry_squared_second_difference_samples,
                ));
            let _ = delay.process_sample(0.0);
            if uses_cold_root {
                cold_frames += 1;
            } else if handoffs == 0 {
                handoffs += 1;
                assert!(
                    (delay.target_delay_samples - analytic_target).abs() < 0.01,
                    "history handoff moved the causal target from {analytic_target} to {}",
                    delay.target_delay_samples
                );
            }
        }

        assert!(cold_frames > 0);
        assert_eq!(handoffs, 1);
        assert!(
            delay.target_delay_samples.ceil() as usize
                <= delay.geometry_history_samples.saturating_sub(1)
        );

        // Poison only the analytic state. Deep-history solving must not touch
        // the closed-form root or curved fallback once the prior target is
        // safely inside retained history.
        let expected = delay.retarded_delay_samples();
        let saved = (
            delay.geometry_delay_squared_samples,
            delay.geometry_squared_step_samples,
            delay.geometry_squared_second_difference_samples,
        );
        delay.geometry_delay_squared_samples = f64::NAN;
        delay.geometry_squared_step_samples = f64::NAN;
        delay.geometry_squared_second_difference_samples = f64::NAN;
        let history_only = delay.retarded_delay_samples();
        assert_eq!(history_only.to_bits(), expected.to_bits());
        (
            delay.geometry_delay_squared_samples,
            delay.geometry_squared_step_samples,
            delay.geometry_squared_second_difference_samples,
        ) = saved;
    }

    #[test]
    fn cold_prehistory_survives_reversal_and_correction_until_exact_history_handoff() {
        const SPEED_MPS: f32 = 110.0;
        const REVERSAL_FRAME: usize = 500;
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target_with_motion(4_800.0, SPEED_MPS, SPEED_MPS);

        for _ in 0..REVERSAL_FRAME {
            let _ = delay.process_sample(0.0);
        }
        assert!(
            delay.target_delay_samples.ceil() as usize
                > delay.geometry_history_samples.saturating_sub(1)
        );

        let shadow_before = (
            delay.prehistory_delay_squared_samples,
            delay.prehistory_squared_step_samples,
            delay.prehistory_squared_second_difference_samples,
        );
        let target_before_reversal = delay.target_delay_samples;
        let corrected_publication = delay.geometry_delay_samples as f32 + 0.25;
        delay.observe_block_target_with_motion(corrected_publication, -SPEED_MPS, SPEED_MPS);
        assert_eq!(
            (
                delay.prehistory_delay_squared_samples,
                delay.prehistory_squared_step_samples,
                delay.prehistory_squared_second_difference_samples,
            ),
            shadow_before,
            "a new velocity publication rewrote implicit prehistory"
        );

        let expected_cold_target = delay.prehistory_retarded_delay_samples();
        let _ = delay.process_sample(0.0);
        assert_eq!(
            delay.target_delay_samples.to_bits(),
            expected_cold_target.to_bits(),
            "the reversal retconned emission time before retained history"
        );
        assert!(
            (delay.target_delay_samples - target_before_reversal).abs() < 0.3,
            "the cold causal target jumped from {target_before_reversal} to {}",
            delay.target_delay_samples
        );

        let mut previous_target = delay.target_delay_samples;
        let mut exact_handoff = None;
        for frame in 1..10_000 {
            // `process_sample` records the present sample before solving, so
            // the pre-call history count is the solve's available lookback.
            let cold_target = delay.prehistory_retarded_delay_samples();
            let hands_off_now = cold_target.ceil() as usize == delay.geometry_history_samples;
            let _ = delay.process_sample(0.0);
            let target_step = (delay.target_delay_samples - previous_target).abs();
            assert!(
                target_step <= MAX_DELAY_SLEW_SAMPLES_PER_SAMPLE + 0.001,
                "piecewise cold history jumped {target_step} samples at frame {frame}"
            );
            if hands_off_now {
                exact_handoff = Some((frame, cold_target, delay.target_delay_samples));
                break;
            }
            previous_target = delay.target_delay_samples;
        }

        let (handoff_frame, cold_target, history_target) =
            exact_handoff.expect("cold root never reached the exact retained-history boundary");
        assert!(
            (history_target - cold_target).abs() < 0.01,
            "frame {handoff_frame} handed off from {cold_target} to {history_target}"
        );
        assert!(
            delay.target_delay_samples.ceil() as usize
                <= delay.geometry_history_samples.saturating_sub(1)
        );
    }

    #[test]
    fn guided_teleport_builds_the_incoming_trajectory_during_its_fade() {
        const SPEED_MPS: f32 = 10.0;
        const PUBLICATION_FRAMES: usize = 128;
        let geometry_rate = SPEED_MPS / SPEED_OF_SOUND_METERS_PER_SECOND;
        let mut delay = PropagationDelayLine::new(30_000, SAMPLE_RATE);
        let mut raw_delay = 4_800.0_f32;

        delay.observe_block_target_with_motion(raw_delay, SPEED_MPS, SPEED_MPS);
        for frame in 0..6_400 {
            if frame > 0 && frame % PUBLICATION_FRAMES == 0 {
                raw_delay += geometry_rate * PUBLICATION_FRAMES as f32;
                delay.observe_block_target_with_motion(raw_delay, SPEED_MPS, SPEED_MPS);
            }
            let _ = delay.process_sample(0.0);
        }

        raw_delay += 10_000.0;
        delay.observe_block_target_with_motion(raw_delay, SPEED_MPS, SPEED_MPS);
        assert!(delay.is_crossfading());
        assert_eq!(delay.geometry_history_samples, 0);
        let outgoing_delay = delay.outgoing_delay_samples;
        let incoming_start = delay.applied_delay_samples;
        let analytic_start = delay.geometry_delay_samples;

        for frame in 0..delay.crossfade_frames as usize {
            if frame > 0 && frame % PUBLICATION_FRAMES == 0 {
                raw_delay += geometry_rate * PUBLICATION_FRAMES as f32;
                delay.observe_block_target_with_motion(raw_delay, SPEED_MPS, SPEED_MPS);
            }
            let _ = delay.process_sample(0.0);
            assert_eq!(
                delay.outgoing_delay_samples.to_bits(),
                outgoing_delay.to_bits()
            );
        }

        assert!(!delay.is_crossfading());
        assert_eq!(
            delay.geometry_history_samples,
            delay.crossfade_frames as usize
        );
        assert!(delay.geometry_delay_samples > analytic_start);
        assert_ne!(
            delay.applied_delay_samples.to_bits(),
            incoming_start.to_bits()
        );
        assert!(
            (delay.target_delay_samples - delay.applied_delay_samples).abs() < 0.01,
            "incoming head ended the fade with {} samples of catch-up pending",
            delay.target_delay_samples - delay.applied_delay_samples
        );
    }

    #[test]
    fn phase_shifted_long_delay_fast_teleports_seed_the_cold_incoming_head() {
        const SPEED_MPS: f32 = 110.0;
        const PUBLICATION_FRAMES: usize = SAMPLE_RATE as usize / 60;
        const CAPTURE_FRAMES: usize = SAMPLE_RATE as usize / 10;
        let geometry_rate = f64::from(SPEED_MPS) / f64::from(SPEED_OF_SOUND_METERS_PER_SECOND);

        for publication_phase_frames in [0_usize, 137, 399, 799] {
            let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
            delay.observe_block_target(4_000.0);
            for _ in 0..45_000 {
                let _ = delay.process_sample(0.0);
            }

            let outgoing_delay = delay.applied_delay_samples;
            let mut analytic_geometry = 40_000.0_f64;
            delay.observe_block_target_with_motion(analytic_geometry as f32, SPEED_MPS, SPEED_MPS);
            let expected_incoming = delay.prehistory_retarded_delay_samples();
            assert!(delay.is_crossfading());
            assert_eq!(
                delay.outgoing_delay_samples.to_bits(),
                outgoing_delay.to_bits()
            );
            assert_eq!(
                delay.applied_delay_samples.to_bits(),
                expected_incoming.to_bits(),
                "phase {publication_phase_frames} did not seed the incoming cold root"
            );
            assert_eq!(
                delay.target_delay_samples.to_bits(),
                expected_incoming.to_bits()
            );

            let mut next_publication = if publication_phase_frames == 0 {
                PUBLICATION_FRAMES
            } else {
                PUBLICATION_FRAMES - publication_phase_frames
            };
            let mut previous_delay = delay.applied_delay_samples;
            let mut maximum_step = 0.0_f32;
            let mut maximum_pending_catchup = 0.0_f32;
            for frame in 0..CAPTURE_FRAMES {
                if frame == next_publication {
                    delay.observe_block_target_with_motion(
                        analytic_geometry as f32,
                        SPEED_MPS,
                        SPEED_MPS,
                    );
                    next_publication += PUBLICATION_FRAMES;
                }
                let was_crossfading = delay.is_crossfading();
                let _ = delay.process_sample(0.0);
                if was_crossfading {
                    assert_eq!(
                        delay.outgoing_delay_samples.to_bits(),
                        outgoing_delay.to_bits()
                    );
                }
                let current_delay = delay.applied_delay_samples;
                maximum_step = maximum_step.max((current_delay - previous_delay).abs());
                maximum_pending_catchup =
                    maximum_pending_catchup.max((delay.target_delay_samples - current_delay).abs());
                previous_delay = current_delay;
                analytic_geometry += geometry_rate;
            }

            println!(
                "FAST_MOVER_LONG_TELEPORT speed_mps={SPEED_MPS:.1} publication_phase_frames={publication_phase_frames} maximum_delay_step_samples={maximum_step:.6} maximum_pending_catchup_samples={maximum_pending_catchup:.6}"
            );
            assert!(!delay.is_crossfading());
            assert!(
                maximum_step < 0.3,
                "phase {publication_phase_frames} hit a teleport catch-up step of {maximum_step}"
            );
            assert!(maximum_pending_catchup < 0.01);
        }
    }

    #[test]
    fn zero_radial_tangency_with_nonzero_relative_speed_remains_guided() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        delay.observe_block_target_with_motion(4_800.0, 0.0, 30.0);

        assert!(delay.retarded_time_guided);
        assert!(delay.fast_motion_guided);
        for _ in 0..128 {
            assert!(delay.process_sample(0.0).is_finite());
        }
        let expected = constant_velocity_retarded_delay_samples(
            delay.geometry_delay_squared_samples,
            delay.geometry_squared_step_samples,
            delay.geometry_squared_second_difference_samples,
        );
        assert!(delay.current_delay_samples() > 4_800.0);
        assert!((f64::from(delay.current_delay_samples()) - expected).abs() < 0.1);
    }

    #[test]
    fn fast_delay_switch_does_not_flutter_inside_the_hysteresis_band() {
        let mut delay = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let target = 4_800.0;

        delay.observe_block_target_with_motion(target, 1.0, 8.0);
        assert!(delay.fast_motion_guided);
        for speed in [7.99_f32, 7.5, 7.01, 7.99] {
            delay.observe_block_target_with_motion(target, 1.0, speed);
            assert!(
                delay.fast_motion_guided,
                "fast delay path fluttered off at {speed} m/s"
            );
        }

        delay.observe_block_target_with_motion(target, 1.0, 7.0);
        assert!(!delay.fast_motion_guided);
        for speed in [7.01_f32, 7.5, 7.99] {
            delay.observe_block_target_with_motion(target, 1.0, speed);
            assert!(
                !delay.fast_motion_guided,
                "legacy delay path fluttered on at {speed} m/s"
            );
        }
        delay.observe_block_target_with_motion(target, 1.0, 8.0);
        assert!(delay.fast_motion_guided);
    }

    #[test]
    fn non_finite_velocity_is_ignored_safely() {
        let mut position_only = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut invalid_velocity = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let velocities = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

        for block in 0..300 {
            let target = 4_800.0 + block as f32 * 0.25;
            position_only.observe_block_target(target);
            let velocity = velocities[block % velocities.len()];
            invalid_velocity.observe_block_target_with_motion(target, velocity, velocity.abs());
            for frame in 0..128 {
                let input = tone(block * 128 + frame, 440.0);
                let expected = position_only.process_sample(input);
                let actual = invalid_velocity.process_sample(input);
                assert!(actual.is_finite());
                assert_eq!(expected.to_bits(), actual.to_bits());
            }
        }
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
    fn static_prehistory_delays_a_motion_corner_until_its_retarded_arrival() {
        const STATIC_DELAY_SAMPLES: f32 = 4_800.0;
        const SPEED_MPS: f32 = 110.0;
        const WARMUP_FRAMES: usize = 12_000;
        const SOURCE_HZ: f32 = 997.0;
        let mut moving = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        let mut static_reference = PropagationDelayLine::new(300_000, SAMPLE_RATE);
        moving.observe_block_target(STATIC_DELAY_SAMPLES);
        static_reference.observe_block_target(STATIC_DELAY_SAMPLES);

        let mut frame = 0_usize;
        for _ in 0..WARMUP_FRAMES {
            let input = tone(frame, SOURCE_HZ);
            assert_eq!(
                moving.process_sample(input).to_bits(),
                static_reference.process_sample(input).to_bits()
            );
            frame += 1;
        }

        moving.observe_block_target_with_motion(STATIC_DELAY_SAMPLES, SPEED_MPS, SPEED_MPS);
        assert_eq!(
            moving.prehistory_delay_squared_samples.to_bits(),
            f64::from(STATIC_DELAY_SAMPLES).powi(2).to_bits()
        );
        assert_eq!(
            moving.prehistory_squared_step_samples.to_bits(),
            0.0_f64.to_bits()
        );
        assert_eq!(
            moving
                .prehistory_squared_second_difference_samples
                .to_bits(),
            0.0_f64.to_bits()
        );
        assert!(moving.geometry_squared_step_samples > 0.0);

        let mut maximum_audible_output = 0.0_f32;
        // At offset `STATIC_DELAY_SAMPLES`, the read reaches the position-
        // continuous corner itself. Its new derivative becomes audible on the
        // following frame, not anywhere in this pre-corner interval.
        for causal_offset in 0..=STATIC_DELAY_SAMPLES as usize {
            let input = tone(frame, SOURCE_HZ);
            let actual = moving.process_sample(input);
            let expected = static_reference.process_sample(input);
            maximum_audible_output = maximum_audible_output.max(actual.abs());
            assert_eq!(
                actual.to_bits(),
                expected.to_bits(),
                "motion arrived early in audio at causal offset {causal_offset}"
            );
            assert_eq!(
                moving.target_delay_samples.to_bits(),
                STATIC_DELAY_SAMPLES.to_bits(),
                "retarded target moved early at causal offset {causal_offset}"
            );
            assert_eq!(
                moving.applied_delay_samples.to_bits(),
                STATIC_DELAY_SAMPLES.to_bits(),
                "read head moved early at causal offset {causal_offset}"
            );
            frame += 1;
        }
        assert!(maximum_audible_output > 0.5);

        let input = tone(frame, SOURCE_HZ);
        let actual = moving.process_sample(input);
        let expected = static_reference.process_sample(input);
        assert!(moving.target_delay_samples > STATIC_DELAY_SAMPLES);
        assert!(moving.applied_delay_samples > STATIC_DELAY_SAMPLES);
        assert_ne!(
            actual.to_bits(),
            expected.to_bits(),
            "the velocity corner did not become audible at its causal arrival"
        );
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
