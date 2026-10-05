//! Feature-gated device and paced device-free output around the shared block processor.

use crate::backend::{MAX_SPATIAL_PROGRAM_PLANES, SpatialProgramBlock};
use crate::live_input::{DeviceNameError, resolve_device_name};
use crate::{
    BlockProcessor, CallbackTimingPublication, CallbackTimingReader, CallbackTimingWriter,
    FaultCounters, MAX_ACTIVE_SOURCES, ProcessBlock, RealtimeClock, RealtimeClockError,
    RunTimingHistogram, SafetyTelemetry, SoakReport, SourceBlock, TimingPercentiles,
};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, Stream, StreamConfig, SupportedBufferSize};
use fightbox_api::EngineConfig;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveOutputError {
    InvalidConfig,
    ClockInitialization(RealtimeClockError),
    NoOutputDevice,
    OutputDeviceNotFound,
    OutputDeviceAmbiguous,
    NoStereoF32Config,
    BuildStream,
    StartStream,
    StopStream,
}

struct AtomicLiveTelemetry {
    timings: CallbackTimingWriter,
    run_timing_p50_ms: AtomicU64,
    run_timing_p95_ms: AtomicU64,
    run_timing_p99_ms: AtomicU64,
    run_timing_p99_9_ms: AtomicU64,
    actual_block_frames: AtomicUsize,
    callback_count: AtomicU64,
    rendered_frames: AtomicU64,
    late_blocks: AtomicU64,
    deadline_misses: AtomicU64,
    processing_errors: AtomicU64,
    stream_errors: AtomicU64,
    snapshot_stale: AtomicU64,
    graph_deadline_miss: AtomicU64,
    backend_render_error: AtomicU64,
    proximity_ceiling_engagements: AtomicU64,
    limiter_engagements: AtomicU64,
    pre_limiter_peak: AtomicU32,
    post_limiter_peak: AtomicU32,
    non_finite_blocks: AtomicU64,
}

impl AtomicLiveTelemetry {
    fn new(timings: CallbackTimingWriter) -> Self {
        Self {
            timings,
            run_timing_p50_ms: AtomicU64::new(0),
            run_timing_p95_ms: AtomicU64::new(0),
            run_timing_p99_ms: AtomicU64::new(0),
            run_timing_p99_9_ms: AtomicU64::new(0),
            actual_block_frames: AtomicUsize::new(0),
            callback_count: AtomicU64::new(0),
            rendered_frames: AtomicU64::new(0),
            late_blocks: AtomicU64::new(0),
            deadline_misses: AtomicU64::new(0),
            processing_errors: AtomicU64::new(0),
            stream_errors: AtomicU64::new(0),
            snapshot_stale: AtomicU64::new(0),
            graph_deadline_miss: AtomicU64::new(0),
            backend_render_error: AtomicU64::new(0),
            proximity_ceiling_engagements: AtomicU64::new(0),
            limiter_engagements: AtomicU64::new(0),
            pre_limiter_peak: AtomicU32::new(0),
            post_limiter_peak: AtomicU32::new(0),
            non_finite_blocks: AtomicU64::new(0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveOutputTelemetry {
    pub callback_count: u64,
    pub rendered_frames: u64,
    /// Null-output blocks started at least one full cadence after their deadline.
    pub late_blocks: u64,
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
    pub safety: SafetyTelemetry,
}

/// A stereo f32 device or paced null stream whose callback owns the block processor.
///
/// The callback captures only preallocated buffers and atomic telemetry. It
/// performs no allocation, locking, logging, filesystem access, or simulation.
pub struct LiveOutput {
    stream: OutputStream,
    telemetry: Arc<AtomicLiveTelemetry>,
    sample_rate_hz: u32,
    device_name: String,
}

enum OutputStream {
    Device(Stream),
    Null(NullOutput),
}

const NULL_PAUSED: u8 = 0;
const NULL_RUNNING: u8 = 1;
const NULL_PAUSING: u8 = 2;
const NULL_SHUTDOWN: u8 = 3;

struct NullOutput {
    state: Arc<AtomicU8>,
    thread: Option<JoinHandle<()>>,
}

impl NullOutput {
    fn start(&self) -> Result<(), LiveOutputError> {
        match self.state.compare_exchange(
            NULL_PAUSED,
            NULL_RUNNING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(NULL_RUNNING) => {
                self.thread.as_ref().unwrap().thread().unpark();
                Ok(())
            }
            Err(_) => Err(LiveOutputError::StartStream),
        }
    }

    fn stop(&self) -> Result<(), LiveOutputError> {
        let state = self.state.compare_exchange(
            NULL_RUNNING,
            NULL_PAUSING,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if state == Err(NULL_PAUSED) {
            return Ok(());
        }
        if !matches!(state, Ok(_) | Err(NULL_PAUSING)) {
            return Err(LiveOutputError::StopStream);
        }
        let thread = self.thread.as_ref().unwrap();
        thread.thread().unpark();
        while self.state.load(Ordering::Acquire) == NULL_PAUSING {
            if thread.is_finished() {
                return Err(LiveOutputError::StopStream);
            }
            thread::yield_now();
        }
        Ok(())
    }
}

impl Drop for NullOutput {
    fn drop(&mut self) {
        self.state.store(NULL_SHUTDOWN, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

enum PacingDecision {
    Sleep(Duration),
    Render { late: bool },
}

struct BlockPacer {
    block_frames: u64,
    sample_rate_hz: u32,
    next_frame: u64,
}

impl BlockPacer {
    fn new(block_frames: usize, sample_rate_hz: u32) -> Self {
        Self {
            block_frames: block_frames as u64,
            sample_rate_hz,
            next_frame: 0,
        }
    }

    fn deadline_ns(&self, frame: u64) -> u64 {
        ((u128::from(frame) * 1_000_000_000) / u128::from(self.sample_rate_hz))
            .min(u128::from(u64::MAX)) as u64
    }

    fn next_block(&mut self, clock: impl FnOnce() -> u64) -> PacingDecision {
        let now_ns = clock();
        let deadline_ns = self.deadline_ns(self.next_frame);
        if now_ns < deadline_ns {
            return PacingDecision::Sleep(Duration::from_nanos(deadline_ns - now_ns));
        }
        self.next_frame = self.next_frame.saturating_add(self.block_frames);
        PacingDecision::Render {
            late: now_ns >= self.deadline_ns(self.next_frame),
        }
    }
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

/// Mutable one- or two-plane program slot returned to a spatial input provider.
/// Plane zero is mono or authored left; plane one is authored right when
/// present. Both fixed backing vectors are zeroed before this value is returned.
pub struct LiveSpatialProgramPlanes<'a> {
    pub plane_zero: &'a mut [f32],
    pub plane_one: Option<&'a mut [f32]>,
}

/// Fixed-capacity two-plane input staging for a future neutral live callback.
///
/// Construction sizes both vectors for every logical source. `clear`,
/// `add_source`, provider calls, and descriptor assembly allocate nothing and
/// do not alter the preserved mono [`LiveSourceBuffer`] contract.
pub struct LiveSpatialSourceBuffer {
    samples: [[Vec<f32>; MAX_SPATIAL_PROGRAM_PLANES]; MAX_ACTIVE_SOURCES],
    source_indices: [usize; MAX_ACTIVE_SOURCES],
    program_plane_counts: [usize; MAX_ACTIVE_SOURCES],
    len: usize,
}

impl LiveSpatialSourceBuffer {
    #[must_use]
    pub fn new(block_size: usize) -> Self {
        Self {
            samples: std::array::from_fn(|_| std::array::from_fn(|_| vec![0.0; block_size])),
            source_indices: [0; MAX_ACTIVE_SOURCES],
            program_plane_counts: [0; MAX_ACTIVE_SOURCES],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Adds one logical source and returns its fixed engine-block-sized planes.
    /// Providers must fill every sample of each returned active plane.
    pub fn add_source(
        &mut self,
        source_index: usize,
        program_plane_count: usize,
    ) -> Option<LiveSpatialProgramPlanes<'_>> {
        if self.len == MAX_ACTIVE_SOURCES
            || source_index >= MAX_ACTIVE_SOURCES
            || !(1..=MAX_SPATIAL_PROGRAM_PLANES).contains(&program_plane_count)
        {
            return None;
        }
        let slot = self.len;
        self.len += 1;
        self.source_indices[slot] = source_index;
        self.program_plane_counts[slot] = program_plane_count;
        let [plane_zero, plane_one] = &mut self.samples[slot];
        plane_zero.fill(0.0);
        plane_one.fill(0.0);
        Some(LiveSpatialProgramPlanes {
            plane_zero,
            plane_one: (program_plane_count == 2).then_some(plane_one),
        })
    }

    #[must_use]
    pub fn source_blocks(&self) -> [SpatialProgramBlock<'_>; MAX_ACTIVE_SOURCES] {
        std::array::from_fn(|slot| {
            if slot >= self.len {
                return SpatialProgramBlock {
                    source_index: 0,
                    program_plane_count: 0,
                    program_planes: [&[], &[]],
                };
            }
            let program_plane_count = self.program_plane_counts[slot];
            SpatialProgramBlock {
                source_index: self.source_indices[slot],
                program_plane_count,
                program_planes: [
                    &self.samples[slot][0],
                    if program_plane_count == 2 {
                        &self.samples[slot][1]
                    } else {
                        &[]
                    },
                ],
            }
        })
    }
}

/// Supplies decoded one- or two-plane source programs without allocation or
/// synchronization. This is additive; the existing mono provider remains the
/// sole input contract for [`LiveOutput`].
pub trait LiveSpatialInputProvider: Send {
    fn fill_block(&mut self, sources: &mut LiveSpatialSourceBuffer);
}

/// Adapts channel-aware staging to the existing device callback and telemetry.
pub struct ProgramInputProcessor<P> {
    processor: P,
    input: Box<dyn LiveSpatialInputProvider>,
    sources: LiveSpatialSourceBuffer,
}

impl<P: BlockProcessor> ProgramInputProcessor<P> {
    pub fn new(processor: P, input: Box<dyn LiveSpatialInputProvider>) -> Self {
        let sources = LiveSpatialSourceBuffer::new(processor.block_size_frames());
        Self {
            processor,
            input,
            sources,
        }
    }
}

impl<P: BlockProcessor> BlockProcessor for ProgramInputProcessor<P> {
    fn block_size_frames(&self) -> usize {
        self.processor.block_size_frames()
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), crate::RenderError> {
        self.sources.clear();
        self.input.fill_block(&mut self.sources);
        let sources = self.sources.source_blocks();
        self.processor
            .process_program_block(crate::ProgramProcessBlock {
                now_ns: block.now_ns,
                sources: &sources[..self.sources.len()],
                output_left: block.output_left,
                output_right: block.output_right,
            })
    }

    fn fault_counters(&self) -> FaultCounters {
        self.processor.fault_counters()
    }

    fn safety_telemetry(&self) -> SafetyTelemetry {
        self.processor.safety_telemetry()
    }
}

impl LiveOutput {
    /// Builds a device-free stream, paused until `start`, at the engine block cadence.
    pub fn new_null_with_input_and_timing<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
        timing_writer: CallbackTimingWriter,
    ) -> Result<Self, LiveOutputError> {
        Self::new_null_with_input_and_timing_limit(
            processor,
            engine_config,
            input,
            timing_writer,
            None,
        )
    }

    /// Stops automatically after a positive whole-engine-block count of stereo frames.
    pub fn new_null_with_input_and_timing_limit<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
        timing_writer: CallbackTimingWriter,
        frame_limit: Option<u64>,
    ) -> Result<Self, LiveOutputError> {
        engine_config
            .validate()
            .map_err(|_| LiveOutputError::InvalidConfig)?;
        let engine_block = processor.block_size_frames();
        if engine_block == 0
            || frame_limit.is_some_and(|limit| limit == 0 || limit % engine_block as u64 != 0)
        {
            return Err(LiveOutputError::InvalidConfig);
        }
        let sample_rate_hz = engine_config.sample_rate_hz;
        let telemetry = Arc::new(AtomicLiveTelemetry::new(timing_writer));
        telemetry
            .actual_block_frames
            .store(engine_block, Ordering::Release);
        let callback_telemetry = Arc::clone(&telemetry);
        let mut callback = CallbackState::new(processor, engine_block, input);
        let mut output = vec![0.0; engine_block * 2];
        let realtime_clock = RealtimeClock::new().map_err(LiveOutputError::ClockInitialization)?;
        let state = Arc::new(AtomicU8::new(NULL_PAUSED));
        let thread_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("fightbox-null-output".to_owned())
            .spawn(move || {
                #[cfg(target_os = "macos")]
                null_thread_policy::prepare();
                let mut rendered_frames = 0_u64;
                loop {
                    match thread_state.load(Ordering::Acquire) {
                        NULL_SHUTDOWN => break,
                        NULL_PAUSING => {
                            let _ = thread_state.compare_exchange(
                                NULL_PAUSING,
                                NULL_PAUSED,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            );
                        }
                        NULL_RUNNING => {
                            let epoch = Instant::now();
                            let mut pacer = BlockPacer::new(engine_block, sample_rate_hz);
                            while thread_state.load(Ordering::Acquire) == NULL_RUNNING {
                                if frame_limit.is_some_and(|limit| rendered_frames >= limit) {
                                    let _ = thread_state.compare_exchange(
                                        NULL_RUNNING,
                                        NULL_PAUSED,
                                        Ordering::AcqRel,
                                        Ordering::Acquire,
                                    );
                                    break;
                                }
                                match pacer.next_block(|| {
                                    epoch.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
                                }) {
                                    PacingDecision::Sleep(duration) => {
                                        thread::park_timeout(duration)
                                    }
                                    PacingDecision::Render { late } => {
                                        if late {
                                            callback_telemetry
                                                .late_blocks
                                                .fetch_add(1, Ordering::Relaxed);
                                        }
                                        let frames = frame_limit.map_or(engine_block, |limit| {
                                            (limit - rendered_frames).min(engine_block as u64)
                                                as usize
                                        });
                                        callback.render_callback(
                                            &mut output[..frames * 2],
                                            sample_rate_hz,
                                            &callback_telemetry,
                                            &realtime_clock,
                                        );
                                        rendered_frames += frames as u64;
                                    }
                                }
                            }
                        }
                        _ => thread::park(),
                    }
                }
            })
            .map_err(|_| LiveOutputError::BuildStream)?;
        Ok(Self {
            stream: OutputStream::Null(NullOutput {
                state,
                thread: Some(thread),
            }),
            telemetry,
            sample_rate_hz,
            device_name: "null-output".to_owned(),
        })
    }

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
        let (timing_writer, _timing_reader) = CallbackTimingPublication::new();
        Self::new_default_with_input_and_timing(processor, engine_config, input, timing_writer)
    }

    /// Opens the default device and publishes every completed callback timing
    /// into the supplied wait-free control-side channel.
    pub fn new_default_with_input_and_timing<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
        timing_writer: CallbackTimingWriter,
    ) -> Result<Self, LiveOutputError> {
        engine_config
            .validate()
            .map_err(|_| LiveOutputError::InvalidConfig)?;
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(LiveOutputError::NoOutputDevice)?;
        Self::new_on_device(processor, engine_config, input, timing_writer, device)
    }

    /// Opens an exact named output device, or a unique case-insensitive substring.
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
        let (timing_writer, _timing_reader) = CallbackTimingPublication::new();
        Self::new_named_with_input_and_timing(
            processor,
            engine_config,
            device_name,
            input,
            timing_writer,
        )
    }

    /// Opens a named device and publishes every completed callback timing into
    /// the supplied wait-free control-side channel.
    pub fn new_named_with_input_and_timing<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        device_name: &str,
        input: Box<dyn LiveInputProvider>,
        timing_writer: CallbackTimingWriter,
    ) -> Result<Self, LiveOutputError> {
        engine_config
            .validate()
            .map_err(|_| LiveOutputError::InvalidConfig)?;
        let host = cpal::default_host();
        let devices: Vec<_> = host
            .output_devices()
            .map_err(|_| LiveOutputError::OutputDeviceNotFound)?
            .filter_map(|device| device.name().ok().map(|name| (name, device)))
            .collect();
        let names: Vec<_> = devices.iter().map(|(name, _)| name.clone()).collect();
        let resolved = resolve_device_name(&names, device_name).map_err(|error| match error {
            DeviceNameError::NotFound => LiveOutputError::OutputDeviceNotFound,
            DeviceNameError::Ambiguous => LiveOutputError::OutputDeviceAmbiguous,
        })?;
        let device = devices
            .into_iter()
            .find(|(name, _)| name == resolved)
            .map(|(_, device)| device)
            .ok_or(LiveOutputError::OutputDeviceNotFound)?;
        Self::new_on_device(processor, engine_config, input, timing_writer, device)
    }

    fn new_on_device<P: BlockProcessor + Send + 'static>(
        processor: P,
        engine_config: EngineConfig,
        input: Box<dyn LiveInputProvider>,
        timing_writer: CallbackTimingWriter,
        device: cpal::Device,
    ) -> Result<Self, LiveOutputError> {
        let device_name = device
            .name()
            .unwrap_or_else(|_| "unknown output device".to_owned());
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
        let telemetry = Arc::new(AtomicLiveTelemetry::new(timing_writer));
        telemetry
            .actual_block_frames
            .store(requested_frames as usize, Ordering::Release);
        let callback_telemetry = Arc::clone(&telemetry);
        let error_telemetry = Arc::clone(&telemetry);
        let engine_block = processor.block_size_frames();
        let sample_rate_hz = engine_config.sample_rate_hz;
        let mut state = CallbackState::new(processor, engine_block, input);
        let realtime_clock = RealtimeClock::new().map_err(LiveOutputError::ClockInitialization)?;

        let stream = device
            .build_output_stream(
                &stream_config,
                move |output: &mut [f32], _| {
                    state.render_callback(
                        output,
                        sample_rate_hz,
                        &callback_telemetry,
                        &realtime_clock,
                    );
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
            stream: OutputStream::Device(stream),
            telemetry,
            sample_rate_hz,
            device_name,
        })
    }

    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn start(&self) -> Result<(), LiveOutputError> {
        match &self.stream {
            OutputStream::Device(stream) => stream.play().map_err(|_| LiveOutputError::StartStream),
            OutputStream::Null(stream) => stream.start(),
        }
    }

    pub fn stop(&self) -> Result<(), LiveOutputError> {
        match &self.stream {
            OutputStream::Device(stream) => stream.pause().map_err(|_| LiveOutputError::StopStream),
            OutputStream::Null(stream) => stream.stop(),
        }
    }

    #[must_use]
    pub fn telemetry(&self) -> LiveOutputTelemetry {
        let actual_block_frames = self.telemetry.actual_block_frames.load(Ordering::Acquire);
        let block_period_ms = actual_block_frames as f64 * 1_000.0 / f64::from(self.sample_rate_hz);
        let history = self.telemetry.timings.snapshot();
        LiveOutputTelemetry {
            callback_count: self.telemetry.callback_count.load(Ordering::Acquire),
            rendered_frames: self.telemetry.rendered_frames.load(Ordering::Acquire),
            late_blocks: self.telemetry.late_blocks.load(Ordering::Acquire),
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
            safety: SafetyTelemetry {
                proximity_ceiling_engagements: self
                    .telemetry
                    .proximity_ceiling_engagements
                    .load(Ordering::Acquire),
                limiter_engagements: self.telemetry.limiter_engagements.load(Ordering::Acquire),
                pre_limiter_peak: f32::from_bits(
                    self.telemetry.pre_limiter_peak.load(Ordering::Acquire),
                ),
                post_limiter_peak: f32::from_bits(
                    self.telemetry.post_limiter_peak.load(Ordering::Acquire),
                ),
                non_finite_blocks: self.telemetry.non_finite_blocks.load(Ordering::Acquire),
            },
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod null_thread_policy {
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    }

    // CoreAudio owns device callback scheduling. The paced renderer owns its
    // thread, so request latency-sensitive QoS before its first audio block.
    pub(super) fn prepare() {
        const USER_INTERACTIVE: u32 = 0x21;
        let status = unsafe { pthread_set_qos_class_self_np(USER_INTERACTIVE, 0) };
        eprintln!("[null-output] scheduling: UserInteractive qos_status={status}");
    }
}

struct CallbackState<P> {
    processor: P,
    run_timings: RunTimingHistogram,
    input: Box<dyn LiveInputProvider>,
    sources: LiveSourceBuffer,
    left: Vec<f32>,
    right: Vec<f32>,
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

    fn render_callback(
        &mut self,
        output: &mut [f32],
        sample_rate_hz: u32,
        telemetry: &AtomicLiveTelemetry,
        realtime_clock: &RealtimeClock,
    ) {
        let started = realtime_clock.start();
        let output_frames = output.len() / 2;
        telemetry
            .actual_block_frames
            .store(output_frames, Ordering::Release);
        self.render(output, sample_rate_hz, telemetry);
        let duration_ns = realtime_clock.elapsed_ns(started);
        telemetry.timings.record(duration_ns);
        self.record_run_timing(duration_ns, telemetry);
        telemetry.callback_count.fetch_add(1, Ordering::Relaxed);
        telemetry
            .rendered_frames
            .fetch_add(output_frames as u64, Ordering::Release);
        let period_ns = (output_frames as u64)
            .saturating_mul(1_000_000_000)
            .checked_div(u64::from(sample_rate_hz))
            .unwrap_or(0);
        if duration_ns > period_ns.saturating_mul(8) / 10 {
            telemetry.deadline_misses.fetch_add(1, Ordering::Relaxed);
        }
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
                self.ring_read = 0;
                self.ring_len = self.left.len();
                self.rendered_frames = self.rendered_frames.saturating_add(self.left.len() as u64);
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
                let safety = self.processor.safety_telemetry();
                telemetry
                    .proximity_ceiling_engagements
                    .store(safety.proximity_ceiling_engagements, Ordering::Relaxed);
                telemetry
                    .limiter_engagements
                    .store(safety.limiter_engagements, Ordering::Relaxed);
                telemetry
                    .pre_limiter_peak
                    .store(safety.pre_limiter_peak.to_bits(), Ordering::Relaxed);
                telemetry
                    .post_limiter_peak
                    .store(safety.post_limiter_peak.to_bits(), Ordering::Relaxed);
                telemetry
                    .non_finite_blocks
                    .store(safety.non_finite_blocks, Ordering::Relaxed);
            }
            frame[0] = self.left[self.ring_read];
            frame[1] = self.right[self.ring_read];
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
        safety: telemetry.safety,
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
        safety: telemetry.safety,
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
    C: FnMut(Duration, &mut CallbackTimingReader) -> std::ops::ControlFlow<()>,
{
    let (timing_writer, mut timing_reader) = CallbackTimingPublication::new();
    let output = LiveOutput::new_default_with_input_and_timing(
        processor,
        engine_config,
        input,
        timing_writer,
    )?;
    output.start()?;
    let started = Instant::now();
    let requested = Duration::from_secs(seconds);
    let control_interval = Duration::from_millis(10);
    let mut control_broke = false;
    loop {
        let elapsed = started.elapsed();
        if elapsed >= requested {
            break;
        }
        if control(elapsed, &mut timing_reader).is_break() {
            control_broke = true;
            break;
        }
        std::thread::sleep(control_interval.min(requested.saturating_sub(elapsed)));
    }
    output.stop()?;
    if !control_broke {
        let _ = control(started.elapsed(), &mut timing_reader);
    }
    let telemetry = output.telemetry();
    Ok(SoakReport {
        rendered_blocks: telemetry.callback_count,
        window_callback_timings: telemetry.callback_timings,
        run_callback_timings: telemetry.run_callback_timings,
        deadline_misses: telemetry.deadline_misses,
        faults: telemetry.faults,
        safety: telemetry.safety,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderError;

    #[test]
    fn null_pacer_uses_frame_deadlines_without_rounding_drift() {
        let mut pacer = BlockPacer::new(128, 48_000);
        assert!(matches!(
            pacer.next_block(|| 0),
            PacingDecision::Render { late: false }
        ));
        assert!(matches!(
            pacer.next_block(|| 2_000_000),
            PacingDecision::Sleep(duration) if duration == Duration::from_nanos(666_666)
        ));
        assert_eq!(pacer.next_frame, 128);
        assert!(matches!(
            pacer.next_block(|| 2_666_666),
            PacingDecision::Render { late: false }
        ));
        for block in 2_u64..375 {
            let deadline = block * 128 * 1_000_000_000 / 48_000;
            assert!(matches!(
                pacer.next_block(|| deadline),
                PacingDecision::Render { late: false }
            ));
        }
        assert!(matches!(
            pacer.next_block(|| 999_999_999),
            PacingDecision::Sleep(duration) if duration == Duration::from_nanos(1)
        ));
    }

    #[test]
    fn null_pacer_counts_missed_blocks_and_catches_up_without_sleep() {
        let mut pacer = BlockPacer::new(128, 48_000);
        let mut late_blocks = 0;
        let injected_now_ns = 10_700_000;
        for expected_late in [true, true, true, true, false] {
            match pacer.next_block(|| injected_now_ns) {
                PacingDecision::Render { late } => {
                    assert_eq!(late, expected_late);
                    late_blocks += u64::from(late);
                }
                PacingDecision::Sleep(_) => panic!("missed blocks must render immediately"),
            }
        }
        assert_eq!(late_blocks, 4);
        assert!(matches!(
            pacer.next_block(|| injected_now_ns),
            PacingDecision::Sleep(duration) if duration == Duration::from_nanos(2_633_333)
        ));
    }

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
        let (timing_writer, _timing_reader) = CallbackTimingPublication::new();
        let telemetry = AtomicLiveTelemetry::new(timing_writer);
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

    struct FixedSpatialProvider;

    impl LiveSpatialInputProvider for FixedSpatialProvider {
        fn fill_block(&mut self, sources: &mut LiveSpatialSourceBuffer) {
            let mono = sources.add_source(2, 1).unwrap();
            mono.plane_zero.copy_from_slice(&[1.0, 2.0, 3.0, 4.0]);
            assert!(mono.plane_one.is_none());

            let stereo = sources.add_source(5, 2).unwrap();
            stereo.plane_zero.copy_from_slice(&[10.0, 20.0, 30.0, 40.0]);
            stereo
                .plane_one
                .unwrap()
                .copy_from_slice(&[-10.0, -20.0, -30.0, -40.0]);
        }
    }

    #[test]
    fn fixed_spatial_provider_preserves_mono_and_stereo_plane_order() {
        let mut sources = LiveSpatialSourceBuffer::new(4);
        FixedSpatialProvider.fill_block(&mut sources);
        assert_eq!(sources.len(), 2);
        let blocks = sources.source_blocks();
        assert_eq!(blocks[0].source_index, 2);
        assert_eq!(blocks[0].program_plane_count, 1);
        assert_eq!(blocks[0].program_planes[0], [1.0, 2.0, 3.0, 4.0]);
        assert!(blocks[0].program_planes[1].is_empty());
        assert_eq!(blocks[1].source_index, 5);
        assert_eq!(blocks[1].program_plane_count, 2);
        assert_eq!(blocks[1].program_planes[0], [10.0, 20.0, 30.0, 40.0]);
        assert_eq!(blocks[1].program_planes[1], [-10.0, -20.0, -30.0, -40.0]);
        assert!(blocks[2..].iter().all(|block| {
            block.program_plane_count == 0
                && block.program_planes[0].is_empty()
                && block.program_planes[1].is_empty()
        }));
    }

    #[test]
    fn fixed_spatial_provider_zeroes_reused_planes_and_rejects_bad_counts() {
        let mut sources = LiveSpatialSourceBuffer::new(4);
        {
            let stereo = sources.add_source(0, 2).unwrap();
            stereo.plane_zero.fill(9.0);
            stereo.plane_one.unwrap().fill(-9.0);
        }
        sources.clear();
        assert!(sources.is_empty());
        let stereo = sources.add_source(0, 2).unwrap();
        assert_eq!(stereo.plane_zero, [0.0; 4]);
        assert_eq!(stereo.plane_one.unwrap(), [0.0; 4]);
        assert!(sources.add_source(1, 0).is_none());
        assert!(sources.add_source(1, 3).is_none());
        assert!(sources.add_source(MAX_ACTIVE_SOURCES, 1).is_none());
    }
}
