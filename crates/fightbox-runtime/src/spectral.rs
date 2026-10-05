//! Realtime application of the composed eight-band environmental transfer.

use fightbox_api::spectral::{SPECTRAL_BAND_COUNT, SpectralTransfer};

const CROSSOVER_COUNT: usize = SPECTRAL_BAND_COUNT - 1;
/// Canonical γ7 maximum target movement in one 128-frame render block.
const MAX_SMOOTHED_GAIN_STEP_DB_PER_BLOCK: f32 = 0.25;
const CANONICAL_BLOCK_FRAMES: f32 = 128.0;
// Geometric means of adjacent 125 Hz through 16 kHz octave-band centres.
const CROSSOVER_FREQUENCIES_HZ: [f32; CROSSOVER_COUNT] = [
    176.776_7,
    353.553_4,
    707.106_8,
    1_414.213_6,
    2_828.427_2,
    5_656.854_5,
    11_313.709,
];

/// Invalid construction input for [`SpectralTransferFilter`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpectralFilterError {
    InvalidSampleRate,
}

/// Fixed-state realtime filter for one mono program plane.
///
/// Seven parallel one-pole lowpasses define eight complementary octave
/// regions. The band regions telescope back to the input; a canonical neutral
/// transfer therefore takes a structural bypass and returns the input bits
/// untouched. Flat non-neutral transfers take one scalar multiplication.
/// Shaped transfers perform fixed work with no allocation, lock, syscall, or
/// data-dependent iteration.
#[derive(Clone, Debug)]
pub struct SpectralTransferFilter {
    transfer: SpectralTransfer,
    mode: SpectralFilterMode,
    current_linear_gain: [f32; SPECTRAL_BAND_COUNT],
    target_linear_gain: [f32; SPECTRAL_BAND_COUNT],
    smoothed_gain_up_ratio: f32,
    smoothed_gain_down_ratio: f32,
    smoothing_active: bool,
    crossover_alpha: [f32; CROSSOVER_COUNT],
    lowpass_state: [f32; CROSSOVER_COUNT],
    shaped_state_ready: bool,
}

#[derive(Clone, Copy, Debug)]
enum SpectralFilterMode {
    Neutral,
    Flat(f32),
    Shaped([f32; SPECTRAL_BAND_COUNT]),
}

impl SpectralTransferFilter {
    pub fn new(sample_rate_hz: u32) -> Result<Self, SpectralFilterError> {
        if sample_rate_hz == 0 {
            return Err(SpectralFilterError::InvalidSampleRate);
        }
        let sample_rate_hz = sample_rate_hz as f32;
        let crossover_alpha = CROSSOVER_FREQUENCIES_HZ.map(|frequency_hz| {
            let coefficient = 1.0 - (-core::f32::consts::TAU * frequency_hz / sample_rate_hz).exp();
            coefficient.clamp(0.0, 1.0)
        });
        let gain_step_db = MAX_SMOOTHED_GAIN_STEP_DB_PER_BLOCK / CANONICAL_BLOCK_FRAMES;
        let smoothed_gain_up_ratio = 10.0_f32.powf(gain_step_db / 20.0);
        Ok(Self {
            transfer: SpectralTransfer::NEUTRAL,
            mode: SpectralFilterMode::Neutral,
            current_linear_gain: [1.0; SPECTRAL_BAND_COUNT],
            target_linear_gain: [1.0; SPECTRAL_BAND_COUNT],
            smoothed_gain_up_ratio,
            smoothed_gain_down_ratio: smoothed_gain_up_ratio.recip(),
            smoothing_active: false,
            crossover_alpha,
            lowpass_state: [0.0; CROSSOVER_COUNT],
            shaped_state_ready: false,
        })
    }

    /// Replaces the already-composed control-rate target.
    ///
    /// This is a fixed-size copy. Converting dB contributions to linear gains
    /// happened before the transfer reached the realtime processor.
    pub fn set_transfer(&mut self, transfer: SpectralTransfer) {
        let gains = transfer.combined_linear_gain();
        self.transfer = transfer;
        self.current_linear_gain = gains;
        self.target_linear_gain = gains;
        self.smoothing_active = false;
        self.install_mode(mode_for(transfer, gains));
    }

    /// Replaces the composed target while rate-limiting every band to at most
    /// 0.25 dB of target movement per canonical 128-frame block.
    ///
    /// The audio callback performs only fixed multiplies/comparisons. Ratio
    /// coefficients are precomputed at construction; no logarithm, allocation,
    /// lock, syscall, or data-dependent loop enters realtime processing.
    pub fn set_transfer_smoothed(&mut self, transfer: SpectralTransfer) {
        let target = transfer.combined_linear_gain();
        self.transfer = transfer;
        self.target_linear_gain = target;
        self.smoothing_active = self
            .current_linear_gain
            .iter()
            .zip(target)
            .any(|(current, target)| current.to_bits() != target.to_bits());
    }

    #[must_use]
    pub const fn transfer(&self) -> SpectralTransfer {
        self.transfer
    }

    /// Clears signal history without changing the control-rate transfer.
    pub fn reset(&mut self) {
        self.lowpass_state = [0.0; CROSSOVER_COUNT];
        self.shaped_state_ready = false;
    }

    /// Applies the composed transfer exactly once to one sample.
    #[inline]
    #[must_use]
    pub fn process_sample(&mut self, input: f32) -> f32 {
        if self.smoothing_active {
            self.advance_smoothed_gains();
        }
        let gains = match self.mode {
            SpectralFilterMode::Neutral => return input,
            SpectralFilterMode::Flat(gain) => return input * gain,
            SpectralFilterMode::Shaped(gains) => gains,
        };

        // A structural bypass deliberately does no hidden filter work. Prime
        // all lowpasses from the first shaped sample so enabling coloration
        // cannot expose a zero-state DC transient.
        if !self.shaped_state_ready {
            self.lowpass_state = [input; CROSSOVER_COUNT];
            self.shaped_state_ready = true;
        }

        for index in 0..CROSSOVER_COUNT {
            self.lowpass_state[index] +=
                self.crossover_alpha[index] * (input - self.lowpass_state[index]);
        }

        let mut output = self.lowpass_state[0] * gains[0];
        for band_index in 1..CROSSOVER_COUNT {
            output += (self.lowpass_state[band_index] - self.lowpass_state[band_index - 1])
                * gains[band_index];
        }
        output + (input - self.lowpass_state[CROSSOVER_COUNT - 1]) * gains[SPECTRAL_BAND_COUNT - 1]
    }

    fn advance_smoothed_gains(&mut self) {
        let mut complete = true;
        for band in 0..SPECTRAL_BAND_COUNT {
            let current = self.current_linear_gain[band];
            let target = self.target_linear_gain[band];
            let next = if current < target {
                (current * self.smoothed_gain_up_ratio).min(target)
            } else if current > target {
                (current * self.smoothed_gain_down_ratio).max(target)
            } else {
                current
            };
            self.current_linear_gain[band] = next;
            complete &= next.to_bits() == target.to_bits();
        }
        self.smoothing_active = !complete;
        self.install_mode(mode_for_linear_gains(self.current_linear_gain));
    }

    fn install_mode(&mut self, mode: SpectralFilterMode) {
        let was_shaped = matches!(self.mode, SpectralFilterMode::Shaped(_));
        let will_be_shaped = matches!(mode, SpectralFilterMode::Shaped(_));
        self.mode = mode;
        if !was_shaped || !will_be_shaped {
            self.shaped_state_ready = false;
        }
    }
}

fn mode_for(transfer: SpectralTransfer, gains: [f32; SPECTRAL_BAND_COUNT]) -> SpectralFilterMode {
    if transfer.is_neutral() {
        SpectralFilterMode::Neutral
    } else {
        mode_for_linear_gains(gains)
    }
}

fn mode_for_linear_gains(gains: [f32; SPECTRAL_BAND_COUNT]) -> SpectralFilterMode {
    if gains.iter().all(|gain| gain.to_bits() == 1.0_f32.to_bits()) {
        SpectralFilterMode::Neutral
    } else if gains[1..]
        .iter()
        .all(|gain| gain.to_bits() == gains[0].to_bits())
    {
        SpectralFilterMode::Flat(gains[0])
    } else {
        SpectralFilterMode::Shaped(gains)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_api::spectral::{SpectralStage, SpectralTransferError};

    #[test]
    fn neutral_bypass_preserves_every_input_bit() {
        let mut filter = SpectralTransferFilter::new(48_000).unwrap();
        let samples = [
            0.0_f32,
            -0.0,
            1.0,
            -1.0,
            f32::from_bits(1),
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::from_bits(0x7fc0_1234),
        ];

        for input in samples {
            assert_eq!(filter.process_sample(input).to_bits(), input.to_bits());
        }
    }

    #[test]
    fn shaped_transfer_keeps_low_body_and_removes_high_frequency_crack() {
        const SAMPLE_RATE: u32 = 48_000;
        const FRAMES: usize = SAMPLE_RATE as usize;
        let transfer = SpectralTransfer::default()
            .with_stage(
                SpectralStage::Atmosphere,
                [0.0, -3.0, -24.0, -60.0, -80.0, -100.0, -120.0, -120.0],
            )
            .unwrap();
        let mut low_filter = SpectralTransferFilter::new(SAMPLE_RATE).unwrap();
        let mut high_filter = SpectralTransferFilter::new(SAMPLE_RATE).unwrap();
        low_filter.set_transfer(transfer);
        high_filter.set_transfer(transfer);
        let mut low_energy = 0.0_f64;
        let mut high_energy = 0.0_f64;
        let measured_from = FRAMES / 2;

        for frame in 0..FRAMES {
            let seconds = frame as f32 / SAMPLE_RATE as f32;
            let low = (core::f32::consts::TAU * 125.0 * seconds).sin();
            let high = (core::f32::consts::TAU * 8_000.0 * seconds).sin();
            let low_output = low_filter.process_sample(low);
            let high_output = high_filter.process_sample(high);
            if frame >= measured_from {
                low_energy += f64::from(low_output * low_output);
                high_energy += f64::from(high_output * high_output);
            }
        }

        let measured_frames = (FRAMES - measured_from) as f64;
        let low_rms = (low_energy / measured_frames).sqrt();
        let high_rms = (high_energy / measured_frames).sqrt();
        println!("SPECTRAL_SMOKE low_125_hz_rms={low_rms:.6} high_8000_hz_rms={high_rms:.6}");
        assert!(low_rms > 0.55, "125 Hz body fell to {low_rms:.6} RMS");
        assert!(high_rms < 0.08, "8 kHz crack remained at {high_rms:.6} RMS");
    }

    #[test]
    fn smoothed_transfer_moves_no_band_more_than_quarter_db_per_128_samples() {
        let deeply_closed = SpectralTransfer::default()
            .with_stage(SpectralStage::Enclosure, [-120.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        let mut filter = SpectralTransferFilter::new(48_000).unwrap();
        filter.set_transfer_smoothed(deeply_closed);

        let mut previous_endpoint_db = 0.0_f32;
        for _ in 0..8 {
            let mut endpoint = 1.0_f32;
            for _ in 0..128 {
                endpoint = filter.process_sample(1.0);
            }
            let endpoint_db = 20.0 * endpoint.log10();
            let step_db = (endpoint_db - previous_endpoint_db).abs();
            assert!(
                step_db <= 0.2501,
                "smoothed target moved {step_db:.6} dB in 128 samples"
            );
            previous_endpoint_db = endpoint_db;
        }
    }

    #[test]
    fn smoothed_reopening_is_also_bounded_and_reaches_exact_neutral_bypass() {
        let deeply_closed = SpectralTransfer::default()
            .with_stage(SpectralStage::Enclosure, [-120.0; SPECTRAL_BAND_COUNT])
            .unwrap();
        let mut filter = SpectralTransferFilter::new(48_000).unwrap();
        filter.set_transfer(deeply_closed);
        filter.set_transfer_smoothed(SpectralTransfer::NEUTRAL);

        let mut previous_endpoint_db = -120.0_f32;
        for _ in 0..512 {
            let mut endpoint = 0.0_f32;
            for _ in 0..128 {
                endpoint = filter.process_sample(1.0);
            }
            let endpoint_db = 20.0 * endpoint.log10();
            let step_db = (endpoint_db - previous_endpoint_db).abs();
            assert!(
                step_db <= 0.2501,
                "smoothed reopening moved {step_db:.6} dB in 128 samples"
            );
            previous_endpoint_db = endpoint_db;
            if endpoint.to_bits() == 1.0_f32.to_bits() {
                assert_eq!(filter.process_sample(0.375).to_bits(), 0.375_f32.to_bits());
                return;
            }
        }
        panic!("smoothed reopening did not reach exact neutral bypass");
    }

    #[test]
    fn filter_construction_rejects_only_an_invalid_sample_rate() {
        assert_eq!(
            SpectralTransferFilter::new(0).unwrap_err(),
            SpectralFilterError::InvalidSampleRate
        );
        let invalid_transfer: Result<SpectralTransfer, SpectralTransferError> =
            SpectralTransfer::default()
                .with_stage(SpectralStage::Atmosphere, [f32::NAN; SPECTRAL_BAND_COUNT]);
        assert!(invalid_transfer.is_err());
    }
}
