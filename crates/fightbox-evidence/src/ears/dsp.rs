//! Small dependency-free DSP helpers adapted from the `ssim-ears` donor.

use std::cmp::Ordering;

pub(crate) const EPS: f64 = 1.0e-20;

pub(crate) fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

pub(crate) fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        0.5 * (sorted[middle - 1] + sorted[middle])
    } else {
        sorted[middle]
    }
}

pub(crate) fn robust_sigma(values: &[f64]) -> f64 {
    let center = median(values);
    let deviations: Vec<f64> = values.iter().map(|value| (value - center).abs()).collect();
    1.4826 * median(&deviations)
}

pub(crate) fn rms(values: &[f32]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    (values
        .iter()
        .map(|&sample| {
            let sample = sample as f64;
            sample * sample
        })
        .sum::<f64>()
        / values.len() as f64)
        .sqrt()
}

pub(crate) fn db20(amplitude: f64) -> f64 {
    20.0 * (amplitude + EPS).log10()
}

pub(crate) fn normalized_correlation(a: &[f32], b: &[f32], lag: isize, stride: usize) -> f64 {
    let (a_start, b_start) = if lag >= 0 {
        (0, lag as usize)
    } else {
        ((-lag) as usize, 0)
    };
    let count = a.len().min(b.len()).saturating_sub(a_start.max(b_start));
    if count == 0 {
        return 0.0;
    }
    let mut cross = 0.0;
    let mut power_a = 0.0;
    let mut power_b = 0.0;
    let mut index = 0;
    while index < count {
        let left = a[a_start + index] as f64;
        let right = b[b_start + index] as f64;
        cross += left * right;
        power_a += left * left;
        power_b += right * right;
        index += stride;
    }
    cross / (power_a * power_b + EPS).sqrt()
}

pub(crate) fn mono(left: &[f32], right: &[f32]) -> Vec<f32> {
    left.iter()
        .zip(right)
        .map(|(&left, &right)| 0.5 * (left + right))
        .collect()
}

pub(crate) fn frame_rms(signal: &[f32], frame: usize, hop: usize) -> Vec<f64> {
    if frame == 0 || hop == 0 || signal.len() < frame {
        return Vec::new();
    }
    (0..=signal.len() - frame)
        .step_by(hop)
        .map(|start| rms(&signal[start..start + frame]))
        .collect()
}

pub(crate) fn goertzel_power(signal: &[f32], sample_rate_hz: u32, frequency_hz: f64) -> f64 {
    let (real, imaginary) = goertzel(signal, sample_rate_hz, frequency_hz);
    (real * real + imaginary * imaginary) / (signal.len() as f64).powi(2)
}

pub(crate) fn goertzel(signal: &[f32], sample_rate_hz: u32, frequency_hz: f64) -> (f64, f64) {
    if signal.is_empty() || frequency_hz <= 0.0 {
        return (0.0, 0.0);
    }
    let omega = std::f64::consts::TAU * frequency_hz / sample_rate_hz as f64;
    let coefficient = 2.0 * omega.cos();
    let mut previous = 0.0;
    let mut previous_two = 0.0;
    for &sample in signal {
        let value = sample as f64 + coefficient * previous - previous_two;
        previous_two = previous;
        previous = value;
    }
    (
        previous - previous_two * omega.cos(),
        previous_two * omega.sin(),
    )
}

pub(crate) fn spectral_profile(signal: &[f32], sample_rate_hz: u32) -> [f64; 8] {
    const FREQUENCIES: [f64; 8] = [
        125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 12_000.0,
    ];
    let mut profile = [0.0; 8];
    for (slot, frequency) in profile.iter_mut().zip(FREQUENCIES) {
        *slot = goertzel_power(signal, sample_rate_hz, frequency);
    }
    let total = profile.iter().sum::<f64>() + EPS;
    for value in &mut profile {
        *value /= total;
    }
    profile
}

pub(crate) fn profile_distance(left: &[f64; 8], right: &[f64; 8]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).abs())
        .sum()
}

pub(crate) fn low_high_ratio_db(signal: &[f32], sample_rate_hz: u32) -> f64 {
    let low = [250.0, 500.0, 1_000.0]
        .iter()
        .map(|&frequency| goertzel_power(signal, sample_rate_hz, frequency))
        .sum::<f64>();
    let high = [4_000.0, 8_000.0, 12_000.0]
        .iter()
        .map(|&frequency| goertzel_power(signal, sample_rate_hz, frequency))
        .sum::<f64>();
    10.0 * ((low + EPS) / (high + EPS)).log10()
}
