//! Opt-in host audition bound, deliberately separate from scene calibration.
//!
//! Capping every sample below the approved reference RMS also bounds RMS for
//! every window. This sacrifices transient headroom and is not transparent DSP.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) const QUIET_CEILING_DBFS: f32 = -55.1;
const QUIET_CEILING_LINEAR: f32 = 0.001_757_923_6;
const FADE_SECONDS: f64 = 0.100;
const RELEASE_SECONDS: f64 = 0.100;

#[derive(Clone, Copy, Debug)]
pub(crate) struct QuietOutputStatus {
    pub processed_frames: u64,
    pub limit_frames: u64,
    pub engagement_frames: u64,
    pub nonfinite_frames: u64,
    pub expired: bool,
}

struct SharedBudget {
    limit_frames: u64,
    processed_frames: AtomicU64,
    engagement_frames: AtomicU64,
    nonfinite_frames: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct QuietOutputReader(Arc<SharedBudget>);

impl QuietOutputReader {
    /// Latest block-published counters; individual counters are not one atomic snapshot.
    pub fn read(&self) -> QuietOutputStatus {
        let processed_frames = self.0.processed_frames.load(Ordering::Acquire);
        QuietOutputStatus {
            processed_frames,
            limit_frames: self.0.limit_frames,
            engagement_frames: self.0.engagement_frames.load(Ordering::Relaxed),
            nonfinite_frames: self.0.nonfinite_frames.load(Ordering::Relaxed),
            expired: processed_frames >= self.0.limit_frames,
        }
    }
}

/// Clone only during stopped-stream setup. Clones share one non-rearming budget;
/// there must be only one active output producer for that budget at a time.
#[derive(Clone)]
pub(crate) struct QuietOutputGuard {
    shared: Arc<SharedBudget>,
    fade_frames: u64,
    release_retention: f64,
    gain: f64,
}

impl QuietOutputGuard {
    pub fn new(seconds: u32, sample_rate_hz: u32) -> Result<Self, &'static str> {
        if !(1..=60).contains(&seconds) || sample_rate_hz == 0 {
            return Err("quiet audition requires 1..=60 seconds and a positive sample rate");
        }
        Ok(Self {
            shared: Arc::new(SharedBudget {
                limit_frames: u64::from(seconds) * u64::from(sample_rate_hz),
                processed_frames: AtomicU64::new(0),
                engagement_frames: AtomicU64::new(0),
                nonfinite_frames: AtomicU64::new(0),
            }),
            fade_frames: (FADE_SECONDS * f64::from(sample_rate_hz)).round().max(1.0) as u64,
            release_retention: (-1.0 / (RELEASE_SECONDS * f64::from(sample_rate_hz))).exp(),
            gain: 1.0,
        })
    }

    pub fn reader(&self) -> QuietOutputReader {
        QuietOutputReader(Arc::clone(&self.shared))
    }

    /// Final host transform, after graph output and before both capture and device copy.
    /// Stereo-linked immediate attenuation, smooth release, and a final finite clamp.
    /// No gain above unity, no allocation, and no reset/rearm operation.
    pub fn process_stereo(&mut self, left: &mut [f32], right: &mut [f32]) {
        if left.len() != right.len() {
            left.fill(0.0);
            right.fill(0.0);
            // Invalid host shape fails closed and exhausts the audition.
            self.shared
                .processed_frames
                .store(self.shared.limit_frames, Ordering::Release);
            return;
        }
        let mut frame = self.shared.processed_frames.load(Ordering::Acquire);
        let mut engaged = 0_u64;
        let mut nonfinite = 0_u64;
        let ceiling = f64::from(QUIET_CEILING_LINEAR);
        for (left, right) in left.iter_mut().zip(right) {
            if frame >= self.shared.limit_frames {
                *left = 0.0;
                *right = 0.0;
                continue;
            }
            if !left.is_finite() || !right.is_finite() {
                *left = 0.0;
                *right = 0.0;
                nonfinite += 1;
            } else {
                let peak = f64::from(left.abs().max(right.abs()));
                let required = if peak > ceiling { ceiling / peak } else { 1.0 };
                self.gain = required.min(1.0 - (1.0 - self.gain) * self.release_retention);
                if self.gain < 1.0 {
                    engaged += 1;
                }
                let fade_in = (frame as f64 / self.fade_frames as f64).min(1.0);
                let remaining = self.shared.limit_frames - 1 - frame;
                let fade_out = (remaining as f64 / self.fade_frames as f64).min(1.0);
                let gain = self.gain * fade_in.min(fade_out);
                *left = (f64::from(*left) * gain).clamp(-ceiling, ceiling) as f32;
                *right = (f64::from(*right) * gain).clamp(-ceiling, ceiling) as f32;
            }
            frame += 1;
        }
        self.shared
            .engagement_frames
            .fetch_add(engaged, Ordering::Relaxed);
        self.shared
            .nonfinite_frames
            .fetch_add(nonfinite, Ordering::Relaxed);
        self.shared.processed_frames.store(frame, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adversarial_stereo_program_is_finite_and_below_approved_peak_and_rms() {
        let mut guard = QuietOutputGuard::new(2, 48_000).unwrap();
        let mut energy = 0.0_f64;
        let mut peak = 0.0_f32;
        let mut count = 0;
        for block in 0..750 {
            let mut left = [0.0; 128];
            let mut right = [0.0; 128];
            for i in 0..128 {
                let x = match (block * 128 + i) % 9 {
                    0 => f32::MAX,
                    1 => -f32::MAX,
                    2 => f32::NAN,
                    3 => f32::INFINITY,
                    4 => f32::NEG_INFINITY,
                    5 => 0.5,
                    6 => -0.0001,
                    _ => 0.0,
                };
                left[i] = x;
                right[i] = -x * 0.5;
            }
            guard.process_stereo(&mut left, &mut right);
            for sample in left.into_iter().chain(right) {
                assert!(sample.is_finite());
                peak = peak.max(sample.abs());
                energy += f64::from(sample).powi(2);
                count += 1;
            }
        }
        let approved_rms = 0.001_767_830_346_187_934_6_f64;
        assert!(f64::from(peak) <= approved_rms);
        assert!((energy / f64::from(count)).sqrt() <= approved_rms);
        assert!(guard.reader().read().nonfinite_frames > 0);
        assert!(guard.reader().read().engagement_frames > 0);
    }

    #[test]
    fn quiet_program_fades_then_expires_and_stopped_stream_clone_cannot_rearm() {
        let mut guard = QuietOutputGuard::new(1, 1_000).unwrap();
        let mut left = [0.0001; 1_010];
        let mut right = [-0.00005; 1_010];
        guard.process_stereo(&mut left, &mut right);
        assert_eq!(left[0], 0.0);
        assert!(left[50] > left[1] && left[50] < left[100]);
        assert_eq!(left[100], 0.0001, "quiet input is never normalized upward");
        assert_eq!(right[100], -0.00005);
        assert!(left[950] < left[900]);
        assert!(left[999..].iter().all(|x| *x == 0.0));
        assert!(guard.reader().read().expired);
        let mut replacement = guard.clone();
        left.fill(1.0);
        right.fill(1.0);
        replacement.process_stereo(&mut left, &mut right);
        assert!(left.iter().chain(&right).all(|x| *x == 0.0));
        assert_eq!(replacement.reader().read().processed_frames, 1_000);
    }
}
