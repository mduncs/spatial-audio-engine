//! Feature-gated CPAL output around the shared block processor.

use crate::{
    BlockProcessor, FaultCounters, MAX_ACTIVE_SOURCES, MAX_TIMING_RECORDS, ProcessBlock,
    RunTimingHistogram, SoakReport, SourceBlock, TimingHistory, TimingPercentiles,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, Stream, StreamConfig, SupportedBufferSize};
use fightbox_api::EngineConfig;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOutputError {
    InvalidConfig,
    NoOutputDevice,
    OutputDeviceNotFound,
    NoStereoF32Config,
    BuildStream,
    StartStream,
    StopStream,
}

struct AtomicTimingHistory {
    records_ns: [AtomicU64; MAX_TIMING_RECORDS],
    next: AtomicUsize,
    len: AtomicUsize,
}

impl Default for AtomicTimingHistory {
    fn default() -> Self {
        Self {
            records_ns: std::array::from_fn(|_| AtomicU64::new(0)),
            next: AtomicUsize::new(0),
            len: AtomicUsize::new(0),
        }
    }
}

impl AtomicTimingHistory {
    fn record(&self, duration_ns: u64) {
        let index = self.next.fetch_add(1, Ordering::Relaxed) % MAX_TIMING_RECORDS;
        self.records_ns[index].store(duration_ns, Ordering::Release);
        self.len
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |len| {
                Some(len.saturating_add(1).min(MAX_TIMING_RECORDS))
            })
            .ok();
    }

    fn snapshot(&self) -> TimingHistory {
        let len = self.len.load(Ordering::Acquire).min(MAX_TIMING_RECORDS);
        let mut history = TimingHistory::default();
        for record in self.records_ns.iter().take(len) {
            history.record(record.load(Ordering::Acquire));
        }
        history
    }
}

#[derive(Default)]
struct AtomicLiveTelemetry {
    timings: AtomicTimingHistory,
    run_timing_p50_ms: AtomicU64,
    run_timing_p95_ms: AtomicU64,
    run_timing_p99_ms: AtomicU64,
    run_timing_p99_9_ms: AtomicU64,
    actual_block_frames: AtomicUsize,
    callback_count: AtomicU64,
    deadline_misses: AtomicU64,
    processing_errors: AtomicU64,
    stream_errors: AtomicU64,
    snapshot_stale: AtomicU64,
    graph_deadline_miss: AtomicU64,
    backend_render_error: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveOutputTelemetry {
    pub callback_count: u64,
    pub actual_block_frames: usize,
    pub block_period_ms: f64,
    pub p99_target_ms: f64,
    pub p99_9_ceiling_ms: f64,
    pub callback_timings: TimingPercentiles,
    pub run_callback_timings: TimingPercentiles,
    pub deadline_misses: u64,
    pub processing_errors: u64,
    pub stream_errors: u64,
    pub faults: FaultCounters,
}

/// A stereo f32 device stream whose callback owns the block processor.
///
/// The callback captures only preallocated buffers and atomic telemetry. It
/// performs no allocation, locking, logging, filesystem access, or simulation.
pub struct LiveOutput {
    stream: Stream,
    telemetry: Arc<AtomicLiveTelemetry>,
    sample_rate_hz: u32,
}

/// Fixed-capacity mono input staging owned by the device callback.
pub struct LiveSourceBuffer {
    samples: [Vec<f32>; MAX_ACTIVE_SOURCES],
    source_indices: [usize; MAX_ACTIVE_SOURCES],
    len: usize,
}

impl LiveSourceBuffer {
    fn new(block_size: usize) -> Self {
        Self {
            samples: std::array::from_fn(|_| vec![0.0; block_size]),
            source_indices: [0; MAX_ACTIVE_SOURCES],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    /// Adds one source and returns its full engine-block-sized mono buffer.
    /// Providers must fill every sample before returning from `fill_block`.
    pub fn add_source(&mut self, source_index: usize) -> Option<&mut [f32]> {
        if self.len == MAX_ACTIVE_SOURCES || source_index >= MAX_ACTIVE_SOURCES {
            return None;
        }
        let slot = self.len;
        self.len += 1;
        self.source_indices[slot] = source_index;
        Some(&mut self.samples[slot])
    }
}

/// Supplies decoded mono engine blocks without allocation or synchronization.
pub trait LiveInputProvider: Send {
    fn fill_block(&mut self, sources: &mut LiveSourceBuffer);
}

struct SilentInput;

impl LiveInputProvider for SilentInput {
    fn fill_block(&mut self, _sources: &mut LiveSourceBuffer) {}
}

impl LiveOutput {
    pub fn new_default<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
    ) -> Result<Self, LiveOutputError> {
        Self::new_default_with_input(processor, engine_config, Box::new(SilentInput))
    }

    pub fn new_default_with_input<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
    ) -> Result<Self, LiveOutputError> {
        engine_config
            .validate()
            .map_err(|_| LiveOutputError::InvalidConfig)?;
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(LiveOutputError::NoOutputDevice)?;
        Self::new_on_device(processor, engine_config, input, device)
    }

    /// Opens the first output device whose CPAL name exactly matches `device_name`.
    pub fn new_named<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        device_name: &str,
    ) -> Result<Self, LiveOutputError> {
        Self::new_named_with_input(processor, engine_config, device_name, Box::new(SilentInput))
    }

    /// Opens a named output device with a caller-supplied decoded input provider.
    pub fn new_named_with_input<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        device_name: &str,
        input: Box<dyn LiveInputProvider>,
    ) -> Result<Self, LiveOutputError> {
        engine_config
            .validate()
            .map_err(|_| LiveOutputError::InvalidConfig)?;
        let host = cpal::default_host();
        let device = host
            .output_devices()
            .map_err(|_| LiveOutputError::OutputDeviceNotFound)?
            .find(|device| {
                device
                    .name()
                    .is_ok_and(|candidate| device_name_matches(&candidate, device_name))
            })
            .ok_or(LiveOutputError::OutputDeviceNotFound)?;
        Self::new_on_device(processor, engine_config, input, device)
    }

    fn new_on_device<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
        device: cpal::Device,
    ) -> Result<Self, LiveOutputError> {
        let mut supported = device
            .supported_output_configs()
            .map_err(|_| LiveOutputError::NoStereoF32Config)?;
        let range = supported
            .find(|range| {
                range.channels() == 2
                    && range.sample_format() == SampleFormat::F32
                    && range.min_sample_rate().0 <= engine_config.sample_rate_hz
                    && range.max_sample_rate().0 >= engine_config.sample_rate_hz
            })
            .ok_or(LiveOutputError::NoStereoF32Config)?;

        let requested_frames = match range.buffer_size() {
            SupportedBufferSize::Range { min, max } => {
                engine_config.block_size_frames.clamp(*min, *max)
            }
            SupportedBufferSize::Unknown => engine_config.block_size_frames,
        };
        let stream_config = StreamConfig {
            channels: 2,
            sample_rate: SampleRate(engine_config.sample_rate_hz),
            buffer_size: BufferSize::Fixed(requested_frames),
        };
        let telemetry = Arc::new(AtomicLiveTelemetry::default());
        telemetry
            .actual_block_frames
            .store(requested_frames as usize, Ordering::Release);
        let callback_telemetry = Arc::clone(&telemetry);
        let error_telemetry = Arc::clone(&telemetry);
        let engine_block = processor.block_size_frames();
        let sample_rate_hz = engine_config.sample_rate_hz;
        let mut state = CallbackState::new(processor, engine_block, input);

        let stream = device
            .build_output_stream(
                &stream_config,
                move |output: &mut [f32], _| {
                    let started = Instant::now();
                    let device_frames = output.len() / 2;
                    callback_telemetry
                        .actual_block_frames
                        .store(device_frames, Ordering::Release);
                    state.render(output, sample_rate_hz, &callback_telemetry);
                    let duration_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                    callback_telemetry.timings.record(duration_ns);
                    state.record_run_timing(duration_ns, &callback_telemetry);
                    callback_telemetry
                        .callback_count
                        .fetch_add(1, Ordering::Relaxed);
                    let period_ns = (device_frames as u64)
                        .saturating_mul(1_000_000_000)
                        .checked_div(u64::from(sample_rate_hz))
                        .unwrap_or(0);
                    if duration_ns > period_ns.saturating_mul(8) / 10 {
                        callback_telemetry
                            .deadline_misses
                            .fetch_add(1, Ordering::Relaxed);
                    }
                },
                move |_| {
                    error_telemetry
                        .stream_errors
                        .fetch_add(1, Ordering::Relaxed);
                },
                None,
            )
            .map_err(|_| LiveOutputError::BuildStream)?;

        Ok(Self {
            stream,
            telemetry,
            sample_rate_hz,
        })
    }

    pub fn start(&self) -> Result<(), LiveOutputError> {
        self.stream.play().map_err(|_| LiveOutputError::StartStream)
    }

    pub fn stop(&self) -> Result<(), LiveOutputError> {
        self.stream.pause().map_err(|_| LiveOutputError::StopStream)
    }

    #[must_use]
    pub fn telemetry(&self) -> LiveOutputTelemetry {
        let actual_block_frames = self.telemetry.actual_block_frames.load(Ordering::Acquire);
        let block_period_ms = actual_block_frames as f64 * 1_000.0 / f64::from(self.sample_rate_hz);
        let history = self.telemetry.timings.snapshot();
        LiveOutputTelemetry {
            callback_count: self.telemetry.callback_count.load(Ordering::Acquire),
            actual_block_frames,
            block_period_ms,
            p99_target_ms: block_period_ms * 0.5,
            p99_9_ceiling_ms: block_period_ms * 0.8,
            callback_timings: TimingPercentiles::from_history(&history),
            run_callback_timings: TimingPercentiles {
                p50_ms: f64::from_bits(self.telemetry.run_timing_p50_ms.load(Ordering::Acquire)),
                p95_ms: f64::from_bits(self.telemetry.run_timing_p95_ms.load(Ordering::Acquire)),
                p99_ms: f64::from_bits(self.telemetry.run_timing_p99_ms.load(Ordering::Acquire)),
                p99_9_ms: f64::from_bits(
                    self.telemetry.run_timing_p99_9_ms.load(Ordering::Acquire),
                ),
            },
            deadline_misses: self.telemetry.deadline_misses.load(Ordering::Acquire),
            processing_errors: self.telemetry.processing_errors.load(Ordering::Acquire),
            stream_errors: self.telemetry.stream_errors.load(Ordering::Acquire),
            faults: FaultCounters {
                snapshot_stale: self.telemetry.snapshot_stale.load(Ordering::Acquire),
                deadline_miss: self.telemetry.graph_deadline_miss.load(Ordering::Acquire),
                backend_render_error: self.telemetry.backend_render_error.load(Ordering::Acquire),
            },
        }
    }
}

fn device_name_matches(candidate: &str, requested: &str) -> bool {
    !requested.trim().is_empty() && candidate.eq_ignore_ascii_case(requested.trim())
}

struct CallbackState<P> {
    processor: P,
    run_timings: RunTimingHistogram,
    input: Box<dyn LiveInputProvider>,
    sources: LiveSourceBuffer,
    left: Vec<f32>,
    right: Vec<f32>,
    ring_left: Vec<f32>,
    ring_right: Vec<f32>,
    ring_read: usize,
    ring_len: usize,
    rendered_frames: u64,
}

impl<P: BlockProcessor> CallbackState<P> {
    fn new(processor: P, block_size: usize, input: Box<dyn LiveInputProvider>) -> Self {
        Self {
            processor,
            run_timings: RunTimingHistogram::default(),
            input,
            sources: LiveSourceBuffer::new(block_size),
            left: vec![0.0; block_size],
            right: vec![0.0; block_size],
            ring_left: vec![0.0; block_size],
            ring_right: vec![0.0; block_size],
            ring_read: 0,
            ring_len: 0,
            rendered_frames: 0,
        }
    }

    fn record_run_timing(&mut self, duration_ns: u64, telemetry: &AtomicLiveTelemetry) {
        self.run_timings.record(duration_ns);
        let percentiles = TimingPercentiles::from_histogram(&self.run_timings);
        telemetry
            .run_timing_p50_ms
            .store(percentiles.p50_ms.to_bits(), Ordering::Release);
        telemetry
            .run_timing_p95_ms
            .store(percentiles.p95_ms.to_bits(), Ordering::Release);
        telemetry
            .run_timing_p99_ms
            .store(percentiles.p99_ms.to_bits(), Ordering::Release);
        telemetry
            .run_timing_p99_9_ms
            .store(percentiles.p99_9_ms.to_bits(), Ordering::Release);
    }

    fn render(&mut self, output: &mut [f32], sample_rate_hz: u32, telemetry: &AtomicLiveTelemetry) {
        for frame in output.chunks_exact_mut(2) {
            if self.ring_len == 0 {
                let now_ns = self
                    .rendered_frames
                    .saturating_mul(1_000_000_000)
                    .checked_div(u64::from(sample_rate_hz))
                    .unwrap_or(0);
                self.sources.clear();
                self.input.fill_block(&mut self.sources);
                let source_blocks: [SourceBlock<'_>; MAX_ACTIVE_SOURCES] =
                    std::array::from_fn(|slot| SourceBlock {
                        source_index: self.sources.source_indices[slot],
                        decoded_mono: if slot < self.sources.len {
                            &self.sources.samples[slot]
                        } else {
                            &[]
                        },
                    });
                if self
                    .processor
                    .process_block(ProcessBlock {
                        now_ns,
                        sources: &source_blocks[..self.sources.len],
                        output_left: &mut self.left,
                        output_right: &mut self.right,
                    })
                    .is_err()
                {
                    self.left.fill(0.0);
                    self.right.fill(0.0);
                    telemetry.processing_errors.fetch_add(1, Ordering::Relaxed);
                }
                self.ring_left.copy_from_slice(&self.left);
                self.ring_right.copy_from_slice(&self.right);
                self.ring_read = 0;
                self.ring_len = self.ring_left.len();
                self.rendered_frames = self
                    .rendered_frames
                    .saturating_add(self.ring_left.len() as u64);
                let faults = self.processor.fault_counters();
                telemetry
                    .snapshot_stale
                    .store(faults.snapshot_stale, Ordering::Relaxed);
                telemetry
                    .graph_deadline_miss
                    .store(faults.deadline_miss, Ordering::Relaxed);
                telemetry
                    .backend_render_error
                    .store(faults.backend_render_error, Ordering::Relaxed);
            }
            frame[0] = self.ring_left[self.ring_read];
            frame[1] = self.ring_right[self.ring_read];
            self.ring_read += 1;
            self.ring_len -= 1;
        }
    }
}

/// Runs a real device soak. `NoOutputDevice` and `NoStereoF32Config` are the
/// explicit self-skip results for headless test machines.
pub fn run_live_soak<P: BlockProcessor + Send + 'static>(
    processor: P,
    engine_config: EngineConfig,
    seconds: u64,
) -> Result<SoakReport, LiveOutputError> {
    let output = LiveOutput::new_default(processor, engine_config)?;
    output.start()?;
    std::thread::sleep(Duration::from_secs(seconds));
    output.stop()?;
    let telemetry = output.telemetry();
    Ok(SoakReport {
        rendered_blocks: telemetry.callback_count,
        window_callback_timings: telemetry.callback_timings,
        run_callback_timings: telemetry.run_callback_timings,
        deadline_misses: telemetry.deadline_misses,
        faults: telemetry.faults,
    })
}

pub fn run_live_soak_with_input<P: BlockProcessor + Send + 'static>(
    processor: P,
    engine_config: EngineConfig,
    input: Box<dyn LiveInputProvider>,
    seconds: u64,
) -> Result<SoakReport, LiveOutputError> {
    let output = LiveOutput::new_default_with_input(processor, engine_config, input)?;
    output.start()?;
    std::thread::sleep(Duration::from_secs(seconds));
    output.stop()?;
    let telemetry = output.telemetry();
    Ok(SoakReport {
        rendered_blocks: telemetry.callback_count,
        window_callback_timings: telemetry.callback_timings,
        run_callback_timings: telemetry.run_callback_timings,
        deadline_misses: telemetry.deadline_misses,
        faults: telemetry.faults,
    })
}

/// Runs a real device soak while giving the caller a 10 ms control-side tick.
///
/// The control closure runs on the caller's thread, never in the device
/// callback. Returning `Break` stops playback early so the caller can surface a
/// control-side failure after this function has paused the stream.
pub fn run_live_soak_with_input_and_control<P, C>(
    processor: P,
    engine_config: EngineConfig,
    input: Box<dyn LiveInputProvider>,
    seconds: u64,
    mut control: C,
) -> Result<SoakReport, LiveOutputError>
where
    P: BlockProcessor + Send + 'static,
    C: FnMut(Duration) -> std::ops::ControlFlow<()>,
{
    let output = LiveOutput::new_default_with_input(processor, engine_config, input)?;
    output.start()?;
    let started = Instant::now();
    let requested = Duration::from_secs(seconds);
    let control_interval = Duration::from_millis(10);
    loop {
        let elapsed = started.elapsed();
        if elapsed >= requested {
            break;
        }
        if control(elapsed).is_break() {
            break;
        }
        std::thread::sleep(control_interval.min(requested.saturating_sub(elapsed)));
    }
    output.stop()?;
    let telemetry = output.telemetry();
    Ok(SoakReport {
        rendered_blocks: telemetry.callback_count,
        window_callback_timings: telemetry.callback_timings,
        run_callback_timings: telemetry.run_callback_timings,
        deadline_misses: telemetry.deadline_misses,
        faults: telemetry.faults,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderError;

    struct RampProcessor {
        next: f32,
        blocks: u64,
    }

    impl BlockProcessor for RampProcessor {
        fn block_size_frames(&self) -> usize {
            4
        }

        fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
            for frame in 0..4 {
                block.output_left[frame] = self.next;
                block.output_right[frame] = -self.next;
                self.next += 1.0;
            }
            self.blocks += 1;
            Ok(())
        }
    }

    #[test]
    fn ring_adapter_bridges_non_engine_device_block_sizes() {
        let processor = RampProcessor {
            next: 0.0,
            blocks: 0,
        };
        let telemetry = AtomicLiveTelemetry::default();
        let mut state = CallbackState::new(processor, 4, Box::new(SilentInput));
        let mut first = [0.0_f32; 12];
        state.render(&mut first, 48_000, &telemetry);
        assert_eq!(
            first,
            [
                0.0, -0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0, 5.0, -5.0
            ]
        );
        let mut second = [0.0_f32; 6];
        state.render(&mut second, 48_000, &telemetry);
        assert_eq!(second, [6.0, -6.0, 7.0, -7.0, 8.0, -8.0]);
        assert_eq!(state.processor.blocks, 3);
    }

    #[test]
    fn named_device_matching_is_exact_case_insensitive_and_trimmed() {
        assert!(device_name_matches("Studio Display", " studio display "));
        assert!(!device_name_matches("Studio Display", "Studio"));
        assert!(!device_name_matches("Studio Display", " "));
    }
}
