//! Bounded stereo capture and clock-drift correction for live source programs.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, SizedSample, Stream, StreamConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const TARGET_SECONDS: f64 = 0.080;
const MAX_CORRECTION: f64 = 0.005;
// Fill error e (s) obeys e'' + KP e' + KI e = 0: critically damped, wn = 0.1 rad/s.
// The 0.5 s fill average removes callback-size sawtooth, so the ratio cannot
// wobble at block rate (audible as pitch jitter on music).
const FILL_SMOOTHING_SECONDS: f64 = 0.5;
const KP_PER_SECOND: f64 = 0.2;
const KI_PER_SECOND_SQUARED: f64 = 0.01;
// Excess beyond this (an output stall) is discarded instead of drained at 0.5 %.
const RESYNC_EXCESS_SECONDS: f64 = 0.25;

struct StereoRing {
    // Each channel carries its sample and the same wrapping sequence tag.
    // Reads validate both tags, so an overwrite cannot split a stereo frame.
    slots: Box<[[AtomicU64; 2]]>,
    written: AtomicU64,
    read: AtomicU64,
    underruns: AtomicU64,
    overruns: AtomicU64,
    ratio: AtomicU64,
    stream_errors: AtomicU64,
    sample_rate: u32,
}

pub struct MonoProducer {
    producer: StereoProducer,
}

pub struct MonoConsumer {
    consumer: StereoConsumer,
}

pub struct StereoProducer {
    ring: Arc<StereoRing>,
    next: u64,
}

pub struct StereoConsumer {
    ring: Arc<StereoRing>,
    next: u64,
}

#[derive(Clone)]
pub struct LiveInputTelemetryReader {
    ring: Arc<StereoRing>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveInputTelemetry {
    /// Number of starvation episodes after playback has begun.
    pub underruns: u64,
    /// Number of oldest input frames discarded when the ring was full.
    pub overruns: u64,
    pub fill_frames: usize,
    pub fill_ms: f64,
    pub ratio: f64,
    pub stream_errors: u64,
}

pub fn mono_ring(sample_rate: u32) -> (MonoProducer, MonoConsumer, LiveInputTelemetryReader) {
    let (producer, consumer, telemetry) = stereo_ring(sample_rate);
    (
        MonoProducer { producer },
        MonoConsumer { consumer },
        telemetry,
    )
}

pub fn stereo_ring(sample_rate: u32) -> (StereoProducer, StereoConsumer, LiveInputTelemetryReader) {
    let sample_rate = sample_rate.max(1);
    let capacity = (sample_rate as usize).next_power_of_two().max(2);
    let ring = Arc::new(StereoRing {
        slots: (0..capacity)
            .map(|_| [AtomicU64::new(0), AtomicU64::new(0)])
            .collect(),
        written: AtomicU64::new(0),
        read: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        overruns: AtomicU64::new(0),
        ratio: AtomicU64::new(1.0_f64.to_bits()),
        stream_errors: AtomicU64::new(0),
        sample_rate,
    });
    (
        StereoProducer {
            ring: Arc::clone(&ring),
            next: 0,
        },
        StereoConsumer {
            ring: Arc::clone(&ring),
            next: 0,
        },
        LiveInputTelemetryReader { ring },
    )
}

impl MonoProducer {
    pub fn push(&mut self, sample: f32) {
        self.producer.push([sample, sample]);
    }
}

impl MonoConsumer {
    pub fn pop(&mut self) -> Option<f32> {
        self.consumer
            .pop()
            .map(|[left, right]| downmix_stereo(left, right))
    }

    pub fn fill_frames(&self) -> usize {
        self.consumer.fill_frames()
    }
}

impl From<MonoConsumer> for StereoConsumer {
    fn from(consumer: MonoConsumer) -> Self {
        consumer.consumer
    }
}

impl StereoProducer {
    pub fn push(&mut self, frame: [f32; 2]) {
        let capacity = self.ring.slots.len() as u64;
        if self
            .next
            .saturating_sub(self.ring.read.load(Ordering::Acquire))
            >= capacity
        {
            self.ring.overruns.fetch_add(1, Ordering::Relaxed);
        }
        let slot = &self.ring.slots[self.next as usize & (capacity as usize - 1)];
        for (channel, sample) in slot.iter().zip(frame) {
            let sample = if sample.is_finite() { sample } else { 0.0 };
            let tagged = ((self.next as u32 as u64) << 32) | u64::from(sample.to_bits());
            channel.store(tagged, Ordering::Release);
        }
        self.next += 1;
        self.ring.written.store(self.next, Ordering::Release);
    }
}

impl StereoConsumer {
    pub fn pop(&mut self) -> Option<[f32; 2]> {
        let written = self.ring.written.load(Ordering::Acquire);
        let capacity = self.ring.slots.len() as u64;
        self.next = self.next.max(written.saturating_sub(capacity));
        if self.next >= written {
            return None;
        }
        let slot = &self.ring.slots[self.next as usize & (capacity as usize - 1)];
        let tagged = [
            slot[0].load(Ordering::Acquire),
            slot[1].load(Ordering::Acquire),
        ];
        if tagged
            .iter()
            .any(|channel| (channel >> 32) as u32 != self.next as u32)
        {
            // A producer overwrite raced this read. Retry on the next sample;
            // neither callback spins waiting for the other.
            let written = self.ring.written.load(Ordering::Acquire);
            self.next = self.next.max(written.saturating_sub(capacity));
            self.ring.read.store(self.next, Ordering::Release);
            return None;
        }
        self.next += 1;
        self.ring.read.store(self.next, Ordering::Release);
        Some(tagged.map(|channel| f32::from_bits(channel as u32)))
    }

    /// Drops the oldest unread frames so at most `keep` remain.
    fn discard_to(&mut self, keep: usize) {
        let floor = self
            .ring
            .written
            .load(Ordering::Acquire)
            .saturating_sub(keep as u64);
        if self.next < floor {
            self.ring
                .overruns
                .fetch_add(floor - self.next, Ordering::Relaxed);
            self.next = floor;
            self.ring.read.store(self.next, Ordering::Release);
        }
    }

    pub fn fill_frames(&self) -> usize {
        self.ring
            .written
            .load(Ordering::Acquire)
            .saturating_sub(self.next)
            .min(self.ring.slots.len() as u64) as usize
    }
}

impl LiveInputTelemetryReader {
    pub fn snapshot(&self) -> LiveInputTelemetry {
        let written = self.ring.written.load(Ordering::Acquire);
        let read = self.ring.read.load(Ordering::Acquire);
        let fill_frames = written
            .saturating_sub(read)
            .min(self.ring.slots.len() as u64) as usize;
        LiveInputTelemetry {
            underruns: self.ring.underruns.load(Ordering::Relaxed),
            overruns: self.ring.overruns.load(Ordering::Relaxed),
            fill_frames,
            fill_ms: fill_frames as f64 * 1_000.0 / f64::from(self.ring.sample_rate),
            ratio: f64::from_bits(self.ring.ratio.load(Ordering::Relaxed)),
            stream_errors: self.ring.stream_errors.load(Ordering::Relaxed),
        }
    }
}

pub struct AdaptiveInput {
    consumer: StereoConsumer,
    nominal: f64,
    ratio: f64,
    input_rate: u32,
    output_rate: u32,
    target_frames: usize,
    integral: f64,
    fill_average_s: Option<f64>,
    primed: bool,
    current: [f32; 2],
    next: [f32; 2],
    phase: f64,
    last_output: [f32; 2],
    fade_start: [f32; 2],
    fade_remaining: usize,
    fade_frames: usize,
    recovery_remaining: usize,
}

impl AdaptiveInput {
    pub fn new(consumer: impl Into<StereoConsumer>, input_rate: u32, output_rate: u32) -> Self {
        let consumer = consumer.into();
        let input_rate = input_rate.max(1);
        let output_rate = output_rate.max(1);
        let nominal = f64::from(input_rate) / f64::from(output_rate);
        consumer
            .ring
            .ratio
            .store(nominal.to_bits(), Ordering::Relaxed);
        Self {
            consumer,
            nominal,
            ratio: nominal,
            input_rate,
            output_rate,
            target_frames: ((f64::from(input_rate) * TARGET_SECONDS) as usize).max(2),
            integral: 0.0,
            fill_average_s: None,
            primed: false,
            current: [0.0; 2],
            next: [0.0; 2],
            phase: 0.0,
            last_output: [0.0; 2],
            fade_start: [0.0; 2],
            fade_remaining: 0,
            fade_frames: (output_rate as usize / 200).max(1),
            recovery_remaining: 0,
        }
    }

    pub fn fill_block(&mut self, output: &mut [f32]) {
        self.begin_block(output.len());
        for sample in output {
            let [left, right] = self.read_frame();
            *sample = downmix_stereo(left, right);
        }
    }

    pub fn fill_stereo_block(&mut self, left: &mut [f32], right: &mut [f32]) {
        assert_eq!(left.len(), right.len());
        self.begin_block(left.len());
        for (left, right) in left.iter_mut().zip(right) {
            [*left, *right] = self.read_frame();
        }
    }

    fn begin_block(&mut self, frames: usize) {
        if self.primed {
            let input_rate = f64::from(self.input_rate);
            let fill_s = self.consumer.fill_frames() as f64 / input_rate;
            let target_s = self.target_frames as f64 / input_rate;
            if fill_s > target_s + RESYNC_EXCESS_SECONDS && self.fade_remaining == 0 {
                // Fade out; re-priming then discards back down to the target.
                self.primed = false;
                self.fill_average_s = None;
                self.fade_start = self.last_output;
                self.fade_remaining = self.fade_frames;
            }
        }
        if self.primed {
            let input_rate = f64::from(self.input_rate);
            let fill_s = self.consumer.fill_frames() as f64 / input_rate;
            let seconds = frames as f64 / f64::from(self.output_rate);
            let average = self.fill_average_s.map_or(fill_s, |average| {
                average + (fill_s - average) * seconds / (FILL_SMOOTHING_SECONDS + seconds)
            });
            self.fill_average_s = Some(average);
            let error = average - self.target_frames as f64 / input_rate;
            // Conditional integration: no windup while the correction is clamped.
            if (KP_PER_SECOND * error + self.integral).abs() < MAX_CORRECTION {
                self.integral += KI_PER_SECOND_SQUARED * error * seconds;
            }
            let correction =
                (KP_PER_SECOND * error + self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
            self.ratio = self.nominal * (1.0 + correction);
            self.consumer
                .ring
                .ratio
                .store(self.ratio.to_bits(), Ordering::Relaxed);
        }
    }

    fn read_frame(&mut self) -> [f32; 2] {
        if self.fade_remaining > 0 {
            self.fade_remaining -= 1;
            self.last_output = self
                .fade_start
                .map(|sample| sample * self.fade_remaining as f32 / self.fade_frames as f32);
            return self.last_output;
        }
        if !self.primed {
            if self.consumer.fill_frames() < self.target_frames {
                self.last_output = [0.0; 2];
                return [0.0; 2];
            }
            self.consumer.discard_to(self.target_frames);
            let (Some(current), Some(next)) = (self.consumer.pop(), self.consumer.pop()) else {
                return [0.0; 2];
            };
            self.current = current;
            self.next = next;
            self.phase = 0.0;
            self.recovery_remaining = self.fade_frames;
            self.primed = true;
        }
        let mut output = [
            self.current[0] + (self.next[0] - self.current[0]) * self.phase as f32,
            self.current[1] + (self.next[1] - self.current[1]) * self.phase as f32,
        ];
        if self.recovery_remaining > 0 {
            self.recovery_remaining -= 1;
            let gain = 1.0 - self.recovery_remaining as f32 / self.fade_frames as f32;
            output = output.map(|sample| sample * gain);
        }
        self.last_output = output;
        self.phase += self.ratio;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.current = self.next;
            let Some(next) = self.consumer.pop() else {
                self.primed = false;
                self.fill_average_s = None;
                self.fade_start = self.last_output;
                self.fade_remaining = self.fade_frames;
                self.consumer.ring.underruns.fetch_add(1, Ordering::Relaxed);
                break;
            };
            self.next = next;
        }
        output
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceNameError {
    NotFound,
    Ambiguous,
}

pub fn resolve_device_name<'a>(
    names: &'a [String],
    requested: &str,
) -> Result<&'a str, DeviceNameError> {
    let requested = requested.trim().to_lowercase();
    if requested.is_empty() {
        return Err(DeviceNameError::NotFound);
    }
    if let Some(name) = names.iter().find(|name| name.to_lowercase() == requested) {
        return Ok(name);
    }
    let mut matches = names
        .iter()
        .filter(|name| name.to_lowercase().contains(&requested));
    match (matches.next(), matches.next()) {
        (Some(name), None) => Ok(name),
        (None, _) => Err(DeviceNameError::NotFound),
        _ => Err(DeviceNameError::Ambiguous),
    }
}

pub fn resolved_output_device_name(requested: Option<&str>) -> Result<String, String> {
    let host = cpal::default_host();
    let Some(requested) = requested else {
        return host
            .default_output_device()
            .ok_or_else(|| "No default output device".to_owned())?
            .name()
            .map_err(|error| error.to_string());
    };
    let names: Vec<_> = host
        .output_devices()
        .map_err(|error| error.to_string())?
        .filter_map(|device| device.name().ok())
        .collect();
    resolve_device_name(&names, requested).map(str::to_owned).map_err(|error| match error {
        DeviceNameError::NotFound => format!("Output device '{requested}' not found; edit --device to an available headphone device"),
        DeviceNameError::Ambiguous => format!("Output device '{requested}' is ambiguous; use its full device name"),
    })
}

pub fn feedback_guard(input_device: &str, output_device: &str) -> Result<(), String> {
    if input_device.trim().to_lowercase() == output_device.trim().to_lowercase() {
        Err(format!(
            "Live input and output both use '{input_device}'; set --device to your headphones to prevent feedback"
        ))
    } else {
        Ok(())
    }
}

#[must_use]
pub fn downmix_stereo(left: f32, right: f32) -> f32 {
    left * 0.5 + right * 0.5
}

pub struct LiveCapture {
    _stream: Stream,
    pub device_name: String,
    pub sample_rate: u32,
}

impl LiveCapture {
    /// Builds and starts input capture. Call only after checking feedback.
    pub fn open(
        device_name: &str,
        output_rate: u32,
    ) -> Result<(Self, AdaptiveInput, LiveInputTelemetryReader), String> {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .map_err(|error| error.to_string())?
            .find(|device| {
                device
                    .name()
                    .is_ok_and(|name| name.eq_ignore_ascii_case(device_name.trim()))
            })
            .ok_or_else(|| format!("Live input device '{device_name}' not found"))?;
        let name = device.name().map_err(|error| error.to_string())?;
        let native = device
            .default_input_config()
            .map_err(|error| error.to_string())?;
        let preferred = device
            .supported_input_configs()
            .map_err(|error| error.to_string())?
            .find(|range| {
                range.channels() > 0
                    && range.sample_format() == SampleFormat::F32
                    && range.min_sample_rate().0 <= output_rate
                    && range.max_sample_rate().0 >= output_rate
            })
            .map(|range| range.with_sample_rate(SampleRate(output_rate)));
        let supported = preferred.unwrap_or(native);
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let rate = config.sample_rate.0;
        let (producer, consumer, telemetry) = stereo_ring(rate);
        let stream = match format {
            SampleFormat::F32 => build_capture::<f32>(&device, &config, producer),
            SampleFormat::I16 => build_capture::<i16>(&device, &config, producer),
            SampleFormat::U16 => build_capture::<u16>(&device, &config, producer),
            SampleFormat::I32 => build_capture::<i32>(&device, &config, producer),
            _ => {
                return Err(format!(
                    "Live input '{device_name}' has unsupported sample format {format:?}"
                ));
            }
        }?;
        stream
            .play()
            .map_err(|error| format!("Could not start live input '{device_name}': {error}"))?;
        Ok((
            Self {
                _stream: stream,
                device_name: name,
                sample_rate: rate,
            },
            AdaptiveInput::new(consumer, rate, output_rate),
            telemetry,
        ))
    }
}

fn build_capture<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut producer: StereoProducer,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let channels = usize::from(config.channels);
    if channels == 0 {
        return Err("Live input device has no input channels".to_owned());
    }
    let error_ring = Arc::clone(&producer.ring);
    device
        .build_input_stream(
            config,
            move |samples: &[T], _| {
                for frame in samples.chunks_exact(channels) {
                    producer.push(capture_frame(frame));
                }
            },
            move |_| {
                error_ring.stream_errors.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|error| format!("Could not build live input stream: {error}"))
}

fn capture_frame<T>(frame: &[T]) -> [f32; 2]
where
    T: SizedSample,
    f32: cpal::FromSample<T>,
{
    let left = frame[0].to_sample::<f32>();
    let right = frame
        .get(1)
        .map_or(left, |sample| sample.to_sample::<f32>());
    [left, right]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_input_stereo_ring_keeps_frames_paired_across_wraparound() {
        let (mut producer, mut consumer, telemetry) = stereo_ring(4);
        for sample in 0..4 {
            producer.push([sample as f32, 100.0 + sample as f32]);
        }
        assert_eq!(consumer.pop(), Some([0.0, 100.0]));
        assert_eq!(consumer.pop(), Some([1.0, 101.0]));
        for sample in 4..8 {
            producer.push([sample as f32, 100.0 + sample as f32]);
        }
        assert_eq!(telemetry.snapshot().overruns, 2);
        for sample in 4..8 {
            assert_eq!(consumer.pop(), Some([sample as f32, 100.0 + sample as f32]));
        }
        assert_eq!(consumer.pop(), None);
    }

    #[test]
    fn live_input_stereo_resampling_uses_one_fractional_phase_and_fade() {
        let (mut producer, consumer, _) = stereo_ring(1_000);
        for sample in 0..80 {
            producer.push([sample as f32, -(2.0 * sample as f32 + 1.0)]);
        }
        let mut input = AdaptiveInput::new(consumer, 1_000, 2_000);
        let mut left = [0.0; 40];
        let mut right = [0.0; 40];
        input.fill_stereo_block(&mut left, &mut right);
        for (index, (left, right)) in left.iter().zip(right).enumerate() {
            let position = index as f32 * 0.5;
            let gain = if index < 10 {
                1.0 - (9 - index) as f32 / 10.0
            } else {
                1.0
            };
            assert!((*left - position * gain).abs() < 1e-5);
            assert!((right - -(2.0 * position + 1.0) * gain).abs() < 1e-5);
        }
    }

    #[test]
    fn live_input_mono_capture_and_ring_duplicate_into_stereo() {
        assert_eq!(capture_frame(&[0.75_f32]), [0.75, 0.75]);
        assert_eq!(capture_frame(&[0.75_f32, -0.25]), [0.75, -0.25]);
        let (mut producer, consumer, _) = mono_ring(1_000);
        for _ in 0..80 {
            producer.push(0.75);
        }
        let mut input = AdaptiveInput::new(consumer, 1_000, 1_000);
        let mut left = [0.0; 16];
        let mut right = [0.0; 16];
        input.fill_stereo_block(&mut left, &mut right);
        assert_eq!(left, right);
        assert!(left.iter().any(|sample| *sample == 0.75));
    }

    #[test]
    fn live_input_ring_wraparound_and_drop_oldest() {
        let (mut producer, mut consumer, telemetry) = mono_ring(4);
        for sample in 0..4 {
            producer.push(sample as f32);
        }
        assert_eq!(consumer.pop(), Some(0.0));
        assert_eq!(consumer.pop(), Some(1.0));
        for sample in 4..8 {
            producer.push(sample as f32);
        }
        assert_eq!(telemetry.snapshot().overruns, 2);
        for sample in 4..8 {
            assert_eq!(consumer.pop(), Some(sample as f32));
        }
        assert_eq!(consumer.pop(), None);
        assert_eq!(telemetry.snapshot().fill_frames, 0);
    }

    #[test]
    fn live_input_ratio_clamp_smooth_drift_tracking_and_resync() {
        let (mut producer, consumer, telemetry) = mono_ring(44_100);
        let mut input = AdaptiveInput::new(consumer, 44_100, 48_000);
        let nominal = 44_100.0 / 48_000.0;
        let target = 3_528.0;
        for _ in 0..5_000 {
            producer.push(0.5);
        }
        let mut output = [0.0; 480];
        input.fill_block(&mut output);
        // Priming discards startup excess down to the target.
        assert!(telemetry.snapshot().fill_frames as f64 <= target);
        // 0.1 s excess stays below resync, so it drains at the clamped rate.
        for _ in 0..4_410 {
            producer.push(0.5);
        }
        let mut produced = 0.0;
        let mut feed = |producer: &mut MonoProducer, drift: f64, chunk: f64| {
            produced += 441.0 * drift;
            while produced >= chunk {
                for _ in 0..chunk as usize {
                    producer.push(0.5);
                }
                produced -= chunk;
            }
        };
        for _ in 0..50 {
            feed(&mut producer, 1.0, 1.0);
            input.fill_block(&mut output);
        }
        assert!((telemetry.snapshot().ratio - nominal * 1.005).abs() < 1e-12);
        // A 0.2 % clock offset fed in 512-frame callbacks: the fill settles
        // on target and the ratio barely moves between blocks.
        let mut previous = telemetry.snapshot().ratio;
        let mut late_jitter: f64 = 0.0;
        for block in 0..9_000 {
            feed(&mut producer, 1.002, 512.0);
            input.fill_block(&mut output);
            let ratio = telemetry.snapshot().ratio;
            assert!((ratio / nominal - 1.0).abs() <= MAX_CORRECTION + 1e-12);
            if block > 6_000 {
                late_jitter = late_jitter.max((ratio - previous).abs() / nominal);
            }
            previous = ratio;
        }
        let correction = previous / nominal - 1.0;
        assert!((correction - 0.002).abs() < 0.000_3, "{correction}");
        assert!(late_jitter < 1e-4, "{late_jitter}");
        assert_eq!(telemetry.snapshot().underruns, 0);
        // An output stall's backlog is discarded, not drained over minutes.
        for _ in 0..22_050 {
            producer.push(0.5);
        }
        for _ in 0..3 {
            input.fill_block(&mut output);
        }
        assert!((telemetry.snapshot().fill_frames as f64) < target + 1_024.0);
    }

    #[test]
    fn live_input_underrun_fades_to_silence_without_step() {
        let (mut producer, consumer, telemetry) = mono_ring(1_000);
        for _ in 0..100 {
            producer.push(1.0);
        }
        let mut input = AdaptiveInput::new(consumer, 1_000, 1_000);
        let mut output = [0.0; 160];
        input.fill_block(&mut output);
        assert_eq!(telemetry.snapshot().underruns, 1);
        assert!(
            output
                .windows(2)
                .all(|pair| (pair[1] - pair[0]).abs() <= 0.201)
        );
        assert!(output[110..].iter().all(|sample| *sample == 0.0));
        for _ in 0..100 {
            producer.push(1.0);
        }
        input.fill_block(&mut output);
        assert!(output[0] <= 0.201);
        assert!(
            output
                .windows(2)
                .all(|pair| (pair[1] - pair[0]).abs() <= 0.201)
        );
    }

    #[test]
    fn live_input_downmix_is_arithmetic_mean() {
        assert_eq!(downmix_stereo(0.75, -0.25), 0.25);
        assert_eq!(downmix_stereo(0.7, 0.7), 0.7);
        assert_eq!(downmix_stereo(1.0, -1.0), 0.0);
    }

    #[test]
    fn live_input_feedback_guard_checks_resolved_devices() {
        assert!(feedback_guard("BlackHole 2ch", " blackhole 2CH ").is_err());
        assert!(feedback_guard("BlackHole 2ch", "md’s AirPods Pro").is_ok());
    }

    #[test]
    fn live_input_device_resolution_exact_substring_and_ambiguity() {
        let names = vec![
            "md’s AirPods Pro".to_owned(),
            "BlackHole 2ch".to_owned(),
            "Mac mini Speakers".to_owned(),
        ];
        assert_eq!(
            resolve_device_name(&names, "airpods"),
            Ok("md’s AirPods Pro")
        );
        assert_eq!(
            resolve_device_name(&names, " blackhole 2CH "),
            Ok("BlackHole 2ch")
        );
        assert_eq!(
            resolve_device_name(&names, "none"),
            Err(DeviceNameError::NotFound)
        );
        let ambiguous = vec![
            "AirPods Pro".to_owned(),
            "AirPods Max".to_owned(),
            "AirPods".to_owned(),
        ];
        assert_eq!(resolve_device_name(&ambiguous, "AirPods"), Ok("AirPods"));
        assert_eq!(resolve_device_name(&ambiguous, "Pro"), Ok("AirPods Pro"));
        assert_eq!(
            resolve_device_name(&ambiguous, "Pods"),
            Err(DeviceNameError::Ambiguous)
        );
    }
}
