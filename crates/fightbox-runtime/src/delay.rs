//! Fractional propagation delay with bounded per-sample target slewing.

/// A preallocated cubic-interpolating delay line.
#[derive(Debug)]
pub struct FractionalDelayLine {
    samples: Vec<f32>,
    write_index: usize,
    maximum_delay_samples: f32,
    current_delay_samples: f32,
    target_delay_samples: f32,
    max_slew_samples_per_sample: f32,
}

impl FractionalDelayLine {
    /// Constructs a delay line whose target is clamped to
    /// `0..=maximum_delay_samples`.
    #[must_use]
    pub fn new(
        maximum_delay_samples: usize,
        initial_delay_samples: f32,
        max_slew_samples_per_sample: f32,
    ) -> Self {
        assert!(
            max_slew_samples_per_sample.is_finite() && max_slew_samples_per_sample > 0.0,
            "delay slew must be finite and positive"
        );
        let maximum = maximum_delay_samples as f32;
        let initial = initial_delay_samples.clamp(0.0, maximum);
        Self {
            // Four guard samples keep all cubic taps distinct, including at
            // the configured maximum delay.
            samples: vec![0.0; maximum_delay_samples.saturating_add(4)],
            write_index: 0,
            maximum_delay_samples: maximum,
            current_delay_samples: initial,
            target_delay_samples: initial,
            max_slew_samples_per_sample,
        }
    }

    #[must_use]
    pub fn maximum_delay_samples(&self) -> f32 {
        self.maximum_delay_samples
    }

    #[must_use]
    pub const fn current_delay_samples(&self) -> f32 {
        self.current_delay_samples
    }

    pub fn set_target_delay_samples(&mut self, delay_samples: f32) {
        self.target_delay_samples = delay_samples.clamp(0.0, self.maximum_delay_samples());
    }

    /// Processes one sample without allocating or synchronizing.
    #[must_use]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        let change = (self.target_delay_samples - self.current_delay_samples).clamp(
            -self.max_slew_samples_per_sample,
            self.max_slew_samples_per_sample,
        );
        self.current_delay_samples += change;
        self.process_at_current_delay(input)
    }

    /// Processes one sample at an explicit delay. The runtime uses this to
    /// finish an intra-block ramp exactly at its target; the public target-slew
    /// API above retains its existing behavior.
    #[must_use]
    pub(crate) fn process_sample_at_delay(&mut self, input: f32, delay_samples: f32) -> f32 {
        let delay_samples = delay_samples.clamp(0.0, self.maximum_delay_samples);
        self.current_delay_samples = delay_samples;
        self.target_delay_samples = delay_samples;
        self.process_at_current_delay(input)
    }

    #[inline]
    fn process_at_current_delay(&mut self, input: f32) -> f32 {
        self.samples[self.write_index] = input;

        let len = self.samples.len();
        let mut read_position = self.write_index as f32 - self.current_delay_samples;
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

        // Third-order Lagrange uses four causal taps here: at fractional
        // delays the newest tap is at most the sample just written. Unlike
        // Catmull-Rom centered on the read point, it therefore needs no
        // unavailable future sample for delays between zero and one.
        let x = fraction;
        let x_minus_1 = x - 1.0;
        let x_plus_1 = x + 1.0;
        let x_plus_2 = x + 2.0;
        let weight_previous_2 = -(x_plus_1 * x * x_minus_1) / 6.0;
        let weight_previous = (x_plus_2 * x * x_minus_1) * 0.5;
        let weight_center = -(x_plus_2 * x_plus_1 * x_minus_1) * 0.5;
        let weight_next = (x_plus_2 * x_plus_1 * x) / 6.0;
        let output = self.samples[previous_2] * weight_previous_2
            + self.samples[previous] * weight_previous
            + self.samples[center] * weight_center
            + self.samples[next] * weight_next;

        self.write_index = (self.write_index + 1) % self.samples.len();
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn linear_interpolate_sine(sample_index: usize, delay: f32, radians_per_sample: f32) -> f32 {
        let read = sample_index as f32 - delay;
        let first = read.floor();
        let fraction = read - first;
        let a = (first * radians_per_sample).sin();
        let b = ((first + 1.0) * radians_per_sample).sin();
        a + (b - a) * fraction
    }

    #[test]
    fn tiny_negative_read_offset_on_a_large_ring_stays_in_bounds() {
        // 131072-sample ring: a -0.002 read offset wraps to a value that
        // rounds to exactly the ring length in f32.
        let mut delay = FractionalDelayLine::new(131_068, 0.002, 1.0);
        let output = delay.process_sample(1.0);
        assert!(output.is_finite());
    }

    #[test]
    fn cubic_error_is_well_below_linear_for_a_fractional_sine_delay() {
        const DELAY: f32 = 19.37;
        const WARMUP: usize = 256;
        const SAMPLES: usize = 8_192;
        let radians_per_sample = TAU * 5_700.0 / 48_000.0;
        let mut delay = FractionalDelayLine::new(128, DELAY, 1.0);
        let mut cubic_squared_error = 0.0_f64;
        let mut linear_squared_error = 0.0_f64;
        let mut measured = 0_usize;

        for frame in 0..SAMPLES {
            let input = (frame as f32 * radians_per_sample).sin();
            let cubic = delay.process_sample(input);
            if frame >= WARMUP {
                let expected = ((frame as f32 - DELAY) * radians_per_sample).sin();
                let linear = linear_interpolate_sine(frame, DELAY, radians_per_sample);
                cubic_squared_error += f64::from((cubic - expected).powi(2));
                linear_squared_error += f64::from((linear - expected).powi(2));
                measured += 1;
            }
        }

        let cubic_rms = (cubic_squared_error / measured as f64).sqrt();
        let linear_rms = (linear_squared_error / measured as f64).sqrt();
        assert!(
            cubic_rms < 0.2 * linear_rms,
            "cubic RMS error {cubic_rms:.8} was not well below linear {linear_rms:.8}"
        );
        assert!(
            cubic_rms < 0.01,
            "cubic RMS error {cubic_rms:.8} exceeded the analytic bound"
        );
    }

    #[test]
    fn fractional_impulse_response_is_finite_bounded_and_unity_dc() {
        const DELAY: f32 = 12.25;
        let mut delay = FractionalDelayLine::new(64, DELAY, 1.0);
        let response: Vec<_> = (0..32)
            .map(|frame| delay.process_sample(if frame == 0 { 1.0 } else { 0.0 }))
            .collect();
        let nonzero: Vec<_> = response
            .iter()
            .enumerate()
            .filter(|(_, sample)| sample.abs() > 1.0e-7)
            .collect();
        let sum: f32 = response.iter().sum();

        assert_eq!(nonzero.len(), 4, "third-order interpolation has four taps");
        assert!(response.iter().all(|sample| sample.is_finite()));
        assert!(response.iter().all(|sample| sample.abs() <= 1.0));
        assert!((sum - 1.0).abs() < 1.0e-6, "impulse DC gain was {sum}");
        let peak_index = nonzero
            .iter()
            .max_by(|(_, a), (_, b)| a.abs().total_cmp(&b.abs()))
            .map(|(index, _)| *index)
            .unwrap();
        assert!(
            (12..=13).contains(&peak_index),
            "fractional impulse peak was at sample {peak_index}"
        );
    }

    #[test]
    fn small_delay_steps_do_not_create_a_discontinuity() {
        let mut delay = FractionalDelayLine::new(128, 8.0, 0.01);
        let mut previous = 0.0;
        let mut largest_step = 0.0_f32;
        let radians_per_sample = TAU * 220.0 / 48_000.0;

        for frame in 0..4_096 {
            if frame >= 1_024 && frame % 32 == 0 {
                delay.set_target_delay_samples(8.0 + (frame - 1_024) as f32 / 32.0 * 0.01);
            }
            let input = (frame as f32 * radians_per_sample).sin();
            let output = delay.process_sample(input);
            if frame > 256 {
                largest_step = largest_step.max((output - previous).abs());
            }
            previous = output;
        }

        assert!(
            largest_step < 0.031,
            "delay target introduced a {largest_step} sample step"
        );
    }
}
