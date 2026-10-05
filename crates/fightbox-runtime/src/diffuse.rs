//! One listener-centric, pack-local diffuse field.
//!
//! Demoted sources and enclosure sends share this fixed topology. The field is
//! intentionally directionless: discrete source reflections and echo tables
//! retain directional authority, while this processor supplies only late bloom.

use fightbox_api::diffuse::{DiffuseFieldProfile, DiffuseFieldProfileError};

const REFERENCE_SAMPLE_RATE_HZ: f64 = 44_100.0;
const COMB_DELAYS: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_DELAYS: [usize; 4] = [556, 441, 341, 225];
const RIGHT_CHANNEL_SPREAD: usize = 23;
const ALLPASS_FEEDBACK: f32 = 0.5;
const TRANSITION_SECONDS: f64 = 0.05;

struct DelayRing {
    samples: Vec<f32>,
    cursor: usize,
}

impl DelayRing {
    fn new(frames: usize) -> Self {
        Self {
            samples: vec![0.0; frames.max(1)],
            cursor: 0,
        }
    }

    #[inline]
    fn read(&self) -> f32 {
        self.samples[self.cursor]
    }

    #[inline]
    fn write_advance(&mut self, sample: f32) {
        self.samples[self.cursor] = sample;
        self.cursor += 1;
        if self.cursor == self.samples.len() {
            self.cursor = 0;
        }
    }

    fn reset(&mut self) {
        self.samples.fill(0.0);
        self.cursor = 0;
    }

    fn payload_bytes(&self) -> usize {
        self.samples.len() * core::mem::size_of::<f32>()
    }
}

struct CombFilter {
    delay: DelayRing,
    lowpass_state: f32,
    feedback: f32,
    feedback_target: f32,
    feedback_step: f32,
}

impl CombFilter {
    fn new(frames: usize, sample_rate_hz: u32, rt60_s: f32) -> Self {
        let feedback = feedback_for_rt60(frames, sample_rate_hz, rt60_s);
        Self {
            delay: DelayRing::new(frames),
            lowpass_state: 0.0,
            feedback,
            feedback_target: feedback,
            feedback_step: 0.0,
        }
    }

    #[inline]
    fn process(&mut self, input: f32, damping: f32) -> f32 {
        let delayed = self.delay.read();
        self.lowpass_state = delayed * (1.0 - damping) + self.lowpass_state * damping;
        self.delay
            .write_advance(input + self.lowpass_state * self.feedback);
        delayed
    }

    fn set_rt60_target(&mut self, sample_rate_hz: u32, rt60_s: f32, transition_frames: usize) {
        self.feedback_target = feedback_for_rt60(self.delay.samples.len(), sample_rate_hz, rt60_s);
        self.feedback_step =
            (self.feedback_target - self.feedback) / transition_frames.max(1) as f32;
    }

    #[inline]
    fn advance_feedback(&mut self, finishing: bool) {
        if finishing {
            self.feedback = self.feedback_target;
        } else {
            self.feedback += self.feedback_step;
        }
    }

    fn reset(&mut self) {
        self.delay.reset();
        self.lowpass_state = 0.0;
    }
}

struct AllpassFilter {
    delay: DelayRing,
}

impl AllpassFilter {
    fn new(frames: usize) -> Self {
        Self {
            delay: DelayRing::new(frames),
        }
    }

    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        let delayed = self.delay.read();
        let output = delayed - input;
        self.delay.write_advance(input + delayed * ALLPASS_FEEDBACK);
        output
    }
}

struct DiffuseChannel {
    combs: [CombFilter; COMB_DELAYS.len()],
    allpasses: [AllpassFilter; ALLPASS_DELAYS.len()],
}

impl DiffuseChannel {
    fn new(sample_rate_hz: u32, right_channel: bool, rt60_s: f32) -> Self {
        let spread = if right_channel {
            RIGHT_CHANNEL_SPREAD
        } else {
            0
        };
        Self {
            combs: COMB_DELAYS.map(|delay| {
                CombFilter::new(
                    scaled_delay(delay + spread, sample_rate_hz),
                    sample_rate_hz,
                    rt60_s,
                )
            }),
            allpasses: ALLPASS_DELAYS
                .map(|delay| AllpassFilter::new(scaled_delay(delay + spread, sample_rate_hz))),
        }
    }

    #[inline]
    fn process(&mut self, input: f32, damping: f32) -> f32 {
        let mut output = 0.0;
        for comb in &mut self.combs {
            output += comb.process(input, damping);
        }
        output *= 1.0 / COMB_DELAYS.len() as f32;
        for allpass in &mut self.allpasses {
            output = allpass.process(output);
        }
        output
    }

    fn set_rt60_target(&mut self, sample_rate_hz: u32, rt60_s: f32, transition_frames: usize) {
        for comb in &mut self.combs {
            comb.set_rt60_target(sample_rate_hz, rt60_s, transition_frames);
        }
    }

    #[inline]
    fn advance_feedback(&mut self, finishing: bool) {
        for comb in &mut self.combs {
            comb.advance_feedback(finishing);
        }
    }

    fn reset(&mut self) {
        for comb in &mut self.combs {
            comb.reset();
        }
        for allpass in &mut self.allpasses {
            allpass.delay.reset();
        }
    }

    fn payload_bytes(&self) -> usize {
        self.combs
            .iter()
            .map(|comb| comb.delay.payload_bytes())
            .chain(
                self.allpasses
                    .iter()
                    .map(|allpass| allpass.delay.payload_bytes()),
            )
            .sum()
    }
}

fn scaled_delay(reference_frames: usize, sample_rate_hz: u32) -> usize {
    (reference_frames as f64 * f64::from(sample_rate_hz) / REFERENCE_SAMPLE_RATE_HZ)
        .round()
        .max(1.0) as usize
}

fn feedback_for_rt60(delay_frames: usize, sample_rate_hz: u32, rt60_s: f32) -> f32 {
    let delay_s = delay_frames as f32 / sample_rate_hz as f32;
    10.0_f32.powf(-3.0 * delay_s / rt60_s).clamp(0.0, 0.9995)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SharedDiffuseMemoryTelemetry {
    pub delay_payload_bytes: usize,
}

/// Preallocated stereo environmental tail shared by all inexpensive sends.
pub struct SharedDiffuseField {
    sample_rate_hz: u32,
    left: DiffuseChannel,
    right: DiffuseChannel,
    current: DiffuseFieldProfile,
    target: DiffuseFieldProfile,
    wet_step: f32,
    rt60_step: f32,
    damping_step: f32,
    transition_remaining: usize,
    transition_frames: usize,
}

impl SharedDiffuseField {
    pub fn new(
        sample_rate_hz: u32,
        profile: DiffuseFieldProfile,
    ) -> Result<Self, SharedDiffuseError> {
        if sample_rate_hz == 0 {
            return Err(SharedDiffuseError::InvalidSampleRate);
        }
        profile
            .validate()
            .map_err(SharedDiffuseError::InvalidProfile)?;
        let transition_frames = (f64::from(sample_rate_hz) * TRANSITION_SECONDS)
            .round()
            .max(1.0) as usize;
        Ok(Self {
            sample_rate_hz,
            left: DiffuseChannel::new(sample_rate_hz, false, profile.rt60_s),
            right: DiffuseChannel::new(sample_rate_hz, true, profile.rt60_s),
            current: profile,
            target: profile,
            wet_step: 0.0,
            rt60_step: 0.0,
            damping_step: 0.0,
            transition_remaining: 0,
            transition_frames,
        })
    }

    /// Begins a 50 ms coefficient transition without clearing the existing tail.
    pub fn set_profile(&mut self, profile: DiffuseFieldProfile) -> Result<(), SharedDiffuseError> {
        profile
            .validate()
            .map_err(SharedDiffuseError::InvalidProfile)?;
        self.target = profile;
        let frames = self.transition_frames as f32;
        self.wet_step = (profile.wet_gain - self.current.wet_gain) / frames;
        self.rt60_step = (profile.rt60_s - self.current.rt60_s) / frames;
        self.damping_step =
            (profile.high_frequency_damping - self.current.high_frequency_damping) / frames;
        self.left
            .set_rt60_target(self.sample_rate_hz, profile.rt60_s, self.transition_frames);
        self.right
            .set_rt60_target(self.sample_rate_hz, profile.rt60_s, self.transition_frames);
        self.transition_remaining = self.transition_frames;
        Ok(())
    }

    #[must_use]
    pub const fn current_profile(&self) -> DiffuseFieldProfile {
        self.current
    }

    #[must_use]
    pub fn memory_telemetry(&self) -> SharedDiffuseMemoryTelemetry {
        SharedDiffuseMemoryTelemetry {
            delay_payload_bytes: self.left.payload_bytes() + self.right.payload_bytes(),
        }
    }

    /// Adds one shared wet field to stereo accumulators. The input is the
    /// already-summed mono send of all participating sources for this block.
    pub fn process_block(
        &mut self,
        input_mono: &[f32],
        output_left: &mut [f32],
        output_right: &mut [f32],
    ) -> Result<(), SharedDiffuseError> {
        if input_mono.len() != output_left.len() || input_mono.len() != output_right.len() {
            return Err(SharedDiffuseError::BlockLengthMismatch);
        }
        for frame in 0..input_mono.len() {
            self.advance_transition();
            let drive = if self.current.wet_gain > 0.0 || self.target.wet_gain > 0.0 {
                input_mono[frame]
            } else {
                0.0
            };
            let left = self
                .left
                .process(drive, self.current.high_frequency_damping);
            let right = self
                .right
                .process(drive, self.current.high_frequency_damping);
            output_left[frame] += left * self.current.wet_gain;
            output_right[frame] += right * self.current.wet_gain;
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }

    #[inline]
    fn advance_transition(&mut self) {
        if self.transition_remaining == 0 {
            return;
        }
        let finishing = self.transition_remaining == 1;
        self.transition_remaining -= 1;
        self.left.advance_feedback(finishing);
        self.right.advance_feedback(finishing);
        if finishing {
            self.current = self.target;
        } else {
            self.current.wet_gain += self.wet_step;
            self.current.rt60_s += self.rt60_step;
            self.current.high_frequency_damping += self.damping_step;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedDiffuseError {
    InvalidSampleRate,
    InvalidProfile(DiffuseFieldProfileError),
    BlockLengthMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 48_000;
    const BLOCK: usize = 128;

    #[test]
    fn one_impulse_produces_a_decorrelated_late_tail_without_a_dry_copy() {
        let mut field =
            SharedDiffuseField::new(SAMPLE_RATE, DiffuseFieldProfile::SMALL_INTERIOR).unwrap();
        let mut tail_energy = 0.0_f64;
        let mut stereo_difference_energy = 0.0_f64;
        let mut first_nonzero = None;
        for block in 0..750 {
            let mut input = [0.0_f32; BLOCK];
            if block == 0 {
                input[0] = 1.0;
            }
            let mut left = [0.0_f32; BLOCK];
            let mut right = [0.0_f32; BLOCK];
            field.process_block(&input, &mut left, &mut right).unwrap();
            for frame in 0..BLOCK {
                let absolute_frame = block * BLOCK + frame;
                if first_nonzero.is_none() && (left[frame] != 0.0 || right[frame] != 0.0) {
                    first_nonzero = Some(absolute_frame);
                }
                if absolute_frame >= SAMPLE_RATE as usize / 10 {
                    tail_energy +=
                        f64::from(left[frame] * left[frame] + right[frame] * right[frame]);
                    stereo_difference_energy += f64::from((left[frame] - right[frame]).powi(2));
                }
            }
        }
        println!(
            "SHARED_DIFFUSE_SMOKE first_nonzero_ms={:.3} tail_energy={tail_energy:.9} stereo_difference_energy={stereo_difference_energy:.9}",
            first_nonzero.unwrap() as f64 * 1_000.0 / SAMPLE_RATE as f64
        );
        assert!(
            first_nonzero.unwrap() > 0,
            "diffuse field leaked a dry copy"
        );
        assert!(tail_energy > 1.0e-7, "diffuse tail ended before 100 ms");
        assert!(stereo_difference_energy > 1.0e-8, "tail was dual mono");
    }

    #[test]
    fn profile_change_ramps_without_resetting_an_admitted_tail() {
        let mut field =
            SharedDiffuseField::new(SAMPLE_RATE, DiffuseFieldProfile::SMALL_INTERIOR).unwrap();
        let mut input = [0.0_f32; BLOCK];
        input[0] = 1.0;
        let mut left = [0.0_f32; BLOCK];
        let mut right = [0.0_f32; BLOCK];
        field.process_block(&input, &mut left, &mut right).unwrap();
        field.set_profile(DiffuseFieldProfile::OFF).unwrap();
        assert_eq!(field.current_profile(), DiffuseFieldProfile::SMALL_INTERIOR);
        for _ in 0..20 {
            field
                .process_block(&[0.0; BLOCK], &mut left, &mut right)
                .unwrap();
        }
        assert_eq!(field.current_profile(), DiffuseFieldProfile::OFF);
    }

    #[test]
    fn field_payload_is_far_below_one_mebibyte() {
        let field =
            SharedDiffuseField::new(SAMPLE_RATE, DiffuseFieldProfile::SMALL_INTERIOR).unwrap();
        let bytes = field.memory_telemetry().delay_payload_bytes;
        println!(
            "SHARED_DIFFUSE_MEMORY bytes={bytes} kib={:.3}",
            bytes as f64 / 1024.0
        );
        assert!(bytes < 1024 * 1024);
    }
}
