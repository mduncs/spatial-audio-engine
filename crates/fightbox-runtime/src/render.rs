//! Unified offline/live block processor and per-source runtime graph shell.

use crate::backend::{
    BackendRenderError, BackendRenderGraph, BackendSourceBlock, ListenerOrientation,
    MAX_SPATIAL_ENVIRONMENT_PLANES, MAX_SPATIAL_PRESENTATION_FEEDS,
    MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE, MAX_SPATIAL_PROGRAM_PLANES, PropagationRenderBlock,
    SpatialAmbisonicChannelOrder, SpatialAmbisonicNormalization, SpatialBackendRenderError,
    SpatialBackendRenderGraph, SpatialBackendSourceBlock, SpatialFeedPlacement,
    SpatialOutputMetadata, SpatialOutputValidity, SpatialProcessBlock, SpatialProgramBlock,
    SpatialPropagationRenderBlock, SpatialRenderError,
};
use crate::safety::{
    OutputSafetyPublication, OutputSafetyReader, SafetyTelemetry, TruePeakLimiter,
};
use crate::spectral::SpectralTransferFilter;
use crate::{
    FractionalDelayLine, MonitorRoute, MonitorRouteReader, RAW_MONITOR_PAD_GAIN, RealtimeClock,
    RealtimeClockError, SnapshotReader,
};
use fightbox_api::spectral::SpectralTransfer;
use fightbox_api::{
    CalibrationError, EngineConfig, ListenerState, OutputSafetyConfig, SceneCalibration,
    SourceDrive, SourceError, SourceProfile,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub const MAX_ACTIVE_SOURCES: usize = 16;
pub const MAX_TIMING_RECORDS: usize = 4096;
pub const RUN_TIMING_HISTOGRAM_BUCKETS: usize = 128;
const RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS: usize = RUN_TIMING_HISTOGRAM_BUCKETS - 1;
const RUN_TIMING_HISTOGRAM_MIN_NS: u64 = 1_000;
const RUN_TIMING_HISTOGRAM_MAX_REGULAR_NS: u64 = 100_000_000;
const RUN_TIMING_HISTOGRAM_EDGES_NS: [u64; RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS] =
    build_run_timing_histogram_edges();
const DEFAULT_MAX_DELAY_SECONDS: f32 = 2.0;
const DEFAULT_DELAY_SLEW_SAMPLES_PER_SAMPLE: f32 = 0.01;
const DEFAULT_SNAPSHOT_STALE_NS: u64 = 100_000_000;
// Direct occlusion and pathing are simulation-rate controls. An 80 ms
// one-pole time constant removes corner zippering while remaining perceptually
// prompt; block endpoints follow the exponential and samples interpolate
// linearly between endpoints.
const SNAPSHOT_GAIN_SLEW_TIME_SECONDS: f32 = 0.080;
const OUTPUT_SAFETY_GAIN_SLEW_TIME_SECONDS: f32 = 0.020;
// Backends normalize in f32. This rejects zero/arbitrary direction vectors
// while allowing the few-ulp error of a correctly normalized feed.
const SPATIAL_DIRECTION_LENGTH_SQUARED_TOLERANCE: f32 = 1.0e-3;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SourcePropagation {
    pub active: bool,
    pub target_delay_samples: f32,
    pub left_gain: f32,
    pub right_gain: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PropagationSnapshot {
    pub sequence: u64,
    pub simulated_at_ns: u64,
    pub sources: [SourcePropagation; MAX_ACTIVE_SOURCES],
}

impl Default for PropagationSnapshot {
    fn default() -> Self {
        Self {
            sequence: 0,
            simulated_at_ns: 0,
            sources: [SourcePropagation::default(); MAX_ACTIVE_SOURCES],
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SourceBlock<'a> {
    pub source_index: usize,
    pub decoded_mono: &'a [f32],
}

pub struct ProcessBlock<'a> {
    pub now_ns: u64,
    pub sources: &'a [SourceBlock<'a>],
    pub output_left: &'a mut [f32],
    pub output_right: &'a mut [f32],
}

/// One- or two-plane programs rendered through the same final output chain.
pub struct ProgramProcessBlock<'a> {
    pub now_ns: u64,
    pub sources: &'a [SpatialProgramBlock<'a>],
    pub output_left: &'a mut [f32],
    pub output_right: &'a mut [f32],
}

pub struct ProgramRenderBlock<'a> {
    pub listener_orientation: ListenerOrientation,
    pub sources: &'a [SpatialBackendSourceBlock<'a>],
    pub output_left: &'a mut [f32],
    pub output_right: &'a mut [f32],
}

/// Binaural endpoint for the existing channel-aware program contract.
pub trait BinauralProgramBackend: Send {
    fn render_program_block(
        &mut self,
        block: ProgramRenderBlock<'_>,
    ) -> Result<(), BackendRenderError>;
}

/// The sole render entry point shared by offline and future device wrappers.
pub trait BlockProcessor {
    fn block_size_frames(&self) -> usize;
    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError>;

    fn process_program_block(&mut self, block: ProgramProcessBlock<'_>) -> Result<(), RenderError> {
        if block.sources.len() > MAX_ACTIVE_SOURCES {
            return Err(RenderError::TooManySources);
        }
        for source in block.sources {
            if source.program_plane_count != 1 {
                return Err(RenderError::InvalidSpatialProgramPlaneCount {
                    source_index: source.source_index,
                    supplied_plane_count: source.program_plane_count,
                });
            }
        }
        let sources = std::array::from_fn::<_, MAX_ACTIVE_SOURCES, _>(|slot| SourceBlock {
            source_index: block
                .sources
                .get(slot)
                .map_or(0, |source| source.source_index),
            decoded_mono: block
                .sources
                .get(slot)
                .map_or(&[][..], |source| source.program_planes[0]),
        });
        self.process_block(ProcessBlock {
            now_ns: block.now_ns,
            sources: &sources[..block.sources.len()],
            output_left: block.output_left,
            output_right: block.output_right,
        })
    }

    #[must_use]
    fn fault_counters(&self) -> FaultCounters {
        FaultCounters::default()
    }

    #[must_use]
    fn safety_telemetry(&self) -> SafetyTelemetry {
        SafetyTelemetry::default()
    }
}

/// Thin offline transport wrapper. It intentionally adds no DSP path.
pub struct OfflineDriver<P> {
    processor: P,
}

impl<P: BlockProcessor> OfflineDriver<P> {
    #[must_use]
    pub const fn new(processor: P) -> Self {
        Self { processor }
    }

    pub fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        self.processor.process_block(block)
    }

    #[must_use]
    pub const fn processor(&self) -> &P {
        &self.processor
    }

    #[must_use]
    pub const fn processor_mut(&mut self) -> &mut P {
        &mut self.processor
    }

    #[must_use]
    pub fn into_processor(self) -> P {
        self.processor
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FaultCounters {
    pub snapshot_stale: u64,
    pub deadline_miss: u64,
    pub backend_render_error: u64,
}

#[derive(Clone, Debug)]
pub struct TimingHistory {
    records_ns: [u64; MAX_TIMING_RECORDS],
    next: usize,
    len: usize,
}

impl Default for TimingHistory {
    fn default() -> Self {
        Self {
            records_ns: [0; MAX_TIMING_RECORDS],
            next: 0,
            len: 0,
        }
    }
}

impl TimingHistory {
    pub fn record(&mut self, duration_ns: u64) {
        self.records_ns[self.next] = duration_ns;
        self.next = (self.next + 1) % MAX_TIMING_RECORDS;
        self.len = self.len.saturating_add(1).min(MAX_TIMING_RECORDS);
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[must_use]
    pub fn newest_ns(&self) -> Option<u64> {
        (self.len > 0).then(|| {
            let index = (self.next + MAX_TIMING_RECORDS - 1) % MAX_TIMING_RECORDS;
            self.records_ns[index]
        })
    }

    /// Returns the nearest-rank percentile without allocating.
    #[must_use]
    pub fn percentile_ns(&self, percentile: f64) -> Option<u64> {
        if self.is_empty() || !percentile.is_finite() {
            return None;
        }
        let mut sorted = [0_u64; MAX_TIMING_RECORDS];
        sorted[..self.len].copy_from_slice(&self.records_ns[..self.len]);
        sorted[..self.len].sort_unstable();
        let rank =
            (((percentile.clamp(0.0, 100.0) / 100.0) * self.len as f64).ceil() as usize).max(1) - 1;
        Some(sorted[rank])
    }
}

struct CallbackTimingSlot {
    duration_ns: AtomicU64,
    stamp: AtomicU64,
}

impl CallbackTimingSlot {
    const fn new() -> Self {
        Self {
            duration_ns: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }
    }

    fn stable_stamp(sequence: u64) -> u64 {
        sequence.wrapping_mul(2).wrapping_add(2)
    }

    /// Returns the duration only when both sequence reads prove that the slot
    /// was not overwritten while its payload was being copied.
    fn read(&self, sequence: u64) -> Option<u64> {
        let expected = Self::stable_stamp(sequence);
        let before = self.stamp.load(Ordering::SeqCst);
        if before != expected {
            return None;
        }
        let duration_ns = self.duration_ns.load(Ordering::SeqCst);
        let after = self.stamp.load(Ordering::SeqCst);
        (after == expected).then_some(duration_ns)
    }
}

struct CallbackTimingShared {
    slots: [CallbackTimingSlot; MAX_TIMING_RECORDS],
    published: AtomicU64,
}

/// Factory for a single-producer callback-timing publication.
///
/// The audio-side writer performs one cursor load and four fixed atomic stores
/// per callback. The control-side reader owns its cursor and drains observations
/// published since its preceding control tick without locks or allocation on
/// either side.
pub struct CallbackTimingPublication;

impl CallbackTimingPublication {
    /// Exact semantic payload shared by one callback-timing channel.
    ///
    /// This is 4,096 two-atomic timing slots plus the shared published cursor.
    /// It deliberately excludes the `Arc` control block, allocator metadata,
    /// struct padding, and the reader's private cursor/drop counters.
    #[must_use]
    pub const fn shared_payload_bytes() -> u64 {
        let slot_payload_bytes = 2_u64.saturating_mul(core::mem::size_of::<AtomicU64>() as u64);
        (MAX_TIMING_RECORDS as u64)
            .saturating_mul(slot_payload_bytes)
            .saturating_add(core::mem::size_of::<AtomicU64>() as u64)
    }

    #[must_use]
    pub fn new() -> (CallbackTimingWriter, CallbackTimingReader) {
        let shared = Arc::new(CallbackTimingShared {
            slots: std::array::from_fn(|_| CallbackTimingSlot::new()),
            published: AtomicU64::new(0),
        });
        (
            CallbackTimingWriter {
                shared: Arc::clone(&shared),
            },
            CallbackTimingReader {
                shared,
                next_sequence: 0,
                dropped_observations: 0,
            },
        )
    }
}

/// Unique audio-side producer for measured callback durations.
pub struct CallbackTimingWriter {
    shared: Arc<CallbackTimingShared>,
}

impl CallbackTimingWriter {
    /// Publishes one completed callback duration with bounded atomic work.
    pub fn record(&self, duration_ns: u64) {
        let sequence = self.shared.published.load(Ordering::Relaxed);
        let index = sequence as usize % MAX_TIMING_RECORDS;
        let slot = &self.shared.slots[index];
        let stable_stamp = CallbackTimingSlot::stable_stamp(sequence);

        // These operations are sequentially consistent so a reader cannot
        // observe the replacement payload while both stamp reads still name
        // the displaced sequence. The odd stamp marks the slot as in flight.
        slot.stamp
            .store(stable_stamp.wrapping_sub(1), Ordering::SeqCst);
        slot.duration_ns.store(duration_ns, Ordering::SeqCst);
        slot.stamp.store(stable_stamp, Ordering::SeqCst);
        self.shared
            .published
            .store(sequence.wrapping_add(1), Ordering::Release);
    }

    #[cfg(feature = "live-output")]
    pub(crate) fn snapshot(&self) -> TimingHistory {
        let published = self.shared.published.load(Ordering::Acquire);
        let first = published.saturating_sub(MAX_TIMING_RECORDS as u64);
        let mut history = TimingHistory::default();
        for sequence in first..published {
            if let Some(duration_ns) =
                self.shared.slots[sequence as usize % MAX_TIMING_RECORDS].read(sequence)
            {
                history.record(duration_ns);
            }
        }
        history
    }
}

/// Control-side cursor over callback durations published since its last drain.
pub struct CallbackTimingReader {
    shared: Arc<CallbackTimingShared>,
    next_sequence: u64,
    dropped_observations: u64,
}

impl CallbackTimingReader {
    /// Delivers each new duration in publication order.
    ///
    /// If the producer laps the consumer before or during a drain, only the
    /// newest sequence-validated observations are delivered. Every skipped or
    /// concurrently overwritten slot is retained in
    /// [`Self::dropped_observations`].
    pub fn drain(&mut self, mut observe: impl FnMut(u64)) -> usize {
        let published = self.shared.published.load(Ordering::Acquire);
        let first_available = published.saturating_sub(MAX_TIMING_RECORDS as u64);
        if self.next_sequence < first_available {
            self.dropped_observations = self
                .dropped_observations
                .saturating_add(first_available - self.next_sequence);
            self.next_sequence = first_available;
        }
        let mut delivered = 0;
        for sequence in self.next_sequence..published {
            let slot = &self.shared.slots[sequence as usize % MAX_TIMING_RECORDS];
            if let Some(duration_ns) = slot.read(sequence) {
                observe(duration_ns);
                delivered += 1;
            } else {
                self.dropped_observations = self.dropped_observations.saturating_add(1);
            }
        }
        self.next_sequence = published;
        delivered
    }

    #[must_use]
    pub const fn dropped_observations(&self) -> u64 {
        self.dropped_observations
    }
}

/// Fixed-size run-wide timing distribution with log-spaced bucket edges.
///
/// The first 127 buckets cover durations through 100 ms. The final bucket
/// records larger values and uses the exact run maximum as its conservative
/// upper edge. Recording performs no allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunTimingHistogram {
    buckets: [u64; RUN_TIMING_HISTOGRAM_BUCKETS],
    count: u64,
    min_ns: u64,
    max_ns: u64,
}

impl Default for RunTimingHistogram {
    fn default() -> Self {
        Self {
            buckets: [0; RUN_TIMING_HISTOGRAM_BUCKETS],
            count: 0,
            min_ns: 0,
            max_ns: 0,
        }
    }
}

impl RunTimingHistogram {
    pub fn record(&mut self, duration_ns: u64) {
        let bucket = RUN_TIMING_HISTOGRAM_EDGES_NS.partition_point(|&edge| edge < duration_ns);
        self.buckets[bucket] += 1;
        self.count += 1;
        if self.count == 1 {
            self.min_ns = duration_ns;
            self.max_ns = duration_ns;
        } else {
            self.min_ns = self.min_ns.min(duration_ns);
            self.max_ns = self.max_ns.max(duration_ns);
        }
    }

    #[must_use]
    pub const fn len(&self) -> u64 {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[must_use]
    pub const fn min_ns(&self) -> Option<u64> {
        if self.is_empty() {
            None
        } else {
            Some(self.min_ns)
        }
    }

    #[must_use]
    pub const fn max_ns(&self) -> Option<u64> {
        if self.is_empty() {
            None
        } else {
            Some(self.max_ns)
        }
    }

    /// Returns a conservative nearest-rank percentile without allocating.
    ///
    /// Regular buckets report their upper edge. The overflow bucket reports
    /// the exact run maximum, which is also an upper bound for its samples.
    #[must_use]
    pub fn percentile_ns(&self, percentile: f64) -> Option<u64> {
        if self.is_empty() || !percentile.is_finite() {
            return None;
        }
        let rank =
            (((percentile.clamp(0.0, 100.0) / 100.0) * self.count as f64).ceil() as u64).max(1);
        let mut cumulative = 0_u64;
        for (index, count) in self.buckets.iter().enumerate() {
            cumulative += count;
            if cumulative >= rank {
                return Some(if index < RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS {
                    run_timing_bucket_upper_bound_ns(index)
                } else {
                    self.max_ns
                });
            }
        }
        Some(self.max_ns)
    }
}

/// Upper edge for a regular run-timing histogram bucket.
#[must_use]
pub const fn run_timing_bucket_upper_bound_ns(index: usize) -> u64 {
    assert!(index < RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS);
    RUN_TIMING_HISTOGRAM_EDGES_NS[index]
}

const fn build_run_timing_histogram_edges() -> [u64; RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS] {
    let mut edges = [0; RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS];
    edges[0] = RUN_TIMING_HISTOGRAM_MIN_NS;
    let mut index = 1;
    while index < RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS - 1 {
        edges[index] = edges[index - 1].saturating_mul(1_095).saturating_add(999) / 1_000;
        index += 1;
    }
    edges[RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS - 1] = RUN_TIMING_HISTOGRAM_MAX_REGULAR_NS;
    edges
}

#[derive(Clone, Debug, Default)]
pub struct Telemetry {
    pub timings: TimingHistory,
    pub faults: FaultCounters,
    pub safety: SafetyTelemetry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderError {
    InvalidConfig,
    ClockInitialization(RealtimeClockError),
    TooManySources,
    SpatialProgramShapeCountMismatch {
        configured_source_count: usize,
        supplied_shape_count: usize,
    },
    InvalidSpatialProgramPlaneCount {
        source_index: usize,
        supplied_plane_count: usize,
    },
    InvalidSourceIndex,
    InvalidBlockLength,
    DuplicateSourceBlock,
    InvalidPropagation,
    Source(SourceError),
    Calibration(CalibrationError),
}

struct SourceNode {
    drive: Option<SourceDrive>,
    delay: FractionalDelayLine,
    spectral_filters: [SpectralTransferFilter; MAX_SPATIAL_PROGRAM_PLANES],
    calibrated: Vec<f32>,
    delayed: Vec<f32>,
    delay_initialized: bool,
    applied_delay_samples: f32,
    snapshot_gain_initialized: bool,
    applied_left_gain: f32,
    applied_right_gain: f32,
    safety_gain_initialized: bool,
    applied_safety_gain: f32,
}

impl SourceNode {
    fn new(block_size: usize, maximum_delay_samples: usize, sample_rate_hz: u32) -> Self {
        Self {
            drive: None,
            delay: FractionalDelayLine::new(
                maximum_delay_samples,
                0.0,
                DEFAULT_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            ),
            spectral_filters: std::array::from_fn(|_| {
                SpectralTransferFilter::new(sample_rate_hz)
                    .expect("validated runtime sample rate is nonzero")
            }),
            calibrated: vec![0.0; block_size],
            delayed: vec![0.0; block_size],
            delay_initialized: false,
            applied_delay_samples: 0.0,
            snapshot_gain_initialized: false,
            applied_left_gain: 0.0,
            applied_right_gain: 0.0,
            safety_gain_initialized: false,
            applied_safety_gain: 1.0,
        }
    }

    fn reset_smoothing(&mut self) {
        self.delay_initialized = false;
        self.snapshot_gain_initialized = false;
        self.safety_gain_initialized = false;
        for filter in &mut self.spectral_filters {
            filter.reset();
        }
    }

    fn set_spectral_transfer(&mut self, transfer: SpectralTransfer) {
        for filter in &mut self.spectral_filters {
            filter.set_transfer(transfer);
        }
    }

    fn set_spectral_transfer_smoothed(&mut self, transfer: SpectralTransfer) {
        for filter in &mut self.spectral_filters {
            filter.set_transfer_smoothed(transfer);
        }
    }

    #[inline]
    fn apply_spectral_transfer(&mut self, program_plane: usize, input: f32) -> f32 {
        self.spectral_filters[program_plane].process_sample(input)
    }

    fn prepare_safety_gain_ramp(
        &mut self,
        target: f32,
        block_retention: f32,
        block_size: usize,
    ) -> SourceSafetyGainRamp {
        if !self.safety_gain_initialized {
            self.applied_safety_gain = target;
            self.safety_gain_initialized = true;
        }
        let endpoint = target + (self.applied_safety_gain - target) * block_retention;
        SourceSafetyGainRamp {
            current: self.applied_safety_gain,
            endpoint,
            step: (endpoint - self.applied_safety_gain) / block_size as f32,
        }
    }
}

/// Private source preparation shared by the preserved mono route and the
/// parallel neutral route. It performs no PCM multiplication itself, so mono's
/// established arithmetic order remains unchanged.
struct SourceSafetyGainRamp {
    current: f32,
    endpoint: f32,
    step: f32,
}

impl SourceSafetyGainRamp {
    #[inline]
    fn next(&mut self, frame: usize, block_size: usize) -> f32 {
        // Advance before applying so the last sample lands exactly on the
        // block endpoint, matching the legacy mono path.
        if frame + 1 == block_size {
            self.current = self.endpoint;
        } else {
            self.current += self.step;
        }
        self.current
    }

    #[must_use]
    fn engages_source_safety(&self) -> bool {
        self.current.min(self.endpoint) < 1.0 - f32::EPSILON
    }
}

struct SpatialSourceScratch {
    calibrated_planes: [Vec<f32>; MAX_SPATIAL_PROGRAM_PLANES],
}

impl SpatialSourceScratch {
    fn new(block_size: usize) -> Self {
        Self {
            calibrated_planes: std::array::from_fn(|_| vec![0.0; block_size]),
        }
    }
}

struct SpatialScratch {
    sources: [SpatialSourceScratch; MAX_ACTIVE_SOURCES],
}

struct ProgramRightScratch {
    delayed: Vec<f32>,
    delay: FractionalDelayLine,
}

impl SpatialScratch {
    fn new(block_size: usize) -> Self {
        Self {
            sources: std::array::from_fn(|_| SpatialSourceScratch::new(block_size)),
        }
    }

    fn persistent_payload_bytes(&self) -> u64 {
        self.sources
            .iter()
            .flat_map(|source| source.calibrated_planes.iter())
            .map(Vec::capacity)
            .map(|samples| {
                u64::try_from(samples)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(core::mem::size_of::<f32>() as u64)
            })
            .fold(0_u64, u64::saturating_add)
    }
}

/// Exact heap payload reserved by the backend-neutral runtime graph.
///
/// This covers the `Vec` capacities owned by the fixed source nodes and the
/// optional neutral-route staging bank. It does not include inline struct
/// bytes, allocator metadata, publication `Arc`s, or the separately owned
/// propagation backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RuntimeGraphMemoryTelemetry {
    pub source_node_capacity: usize,
    pub propagation_delay_payload_bytes: u64,
    pub block_scratch_payload_bytes: u64,
    /// Calibrated one-or-two-plane staging owned only by the neutral route.
    pub spatial_scratch_payload_bytes: u64,
    pub total_payload_bytes: u64,
}

/// Fixed-capacity, preallocated per-source graph shell.
pub struct RuntimeGraph {
    config: EngineConfig,
    realtime_clock: RealtimeClock,
    snapshot_reader: SnapshotReader<PropagationSnapshot>,
    output_safety_reader: OutputSafetyReader,
    monitor_route_reader: Option<MonitorRouteReader>,
    sources: [SourceNode; MAX_ACTIVE_SOURCES],
    listener: Option<ListenerState>,
    backend: Option<Box<dyn BackendRenderGraph>>,
    program_backend: Option<Box<dyn BinauralProgramBackend>>,
    program_plane_counts: [usize; MAX_ACTIVE_SOURCES],
    program_right: [Option<ProgramRightScratch>; MAX_ACTIVE_SOURCES],
    spatial_backend: Option<Box<dyn SpatialBackendRenderGraph>>,
    spatial_scratch: Option<SpatialScratch>,
    spatial_program_plane_counts: Option<[usize; MAX_ACTIVE_SOURCES]>,
    spatial_generation: u64,
    spatial_discontinuity_sequence: u64,
    telemetry: Telemetry,
    snapshot_stale_after_ns: u64,
    deadline_ns: u64,
    snapshot_gain_block_retention: f32,
    output_safety_gain_block_retention: f32,
    monitor_gain_initialized: bool,
    applied_monitor_gain: f32,
    true_peak_limiter: TruePeakLimiter,
}

impl RuntimeGraph {
    pub fn new(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
    ) -> Result<Self, RenderError> {
        let (_, output_safety_reader) = OutputSafetyPublication::new(OutputSafetyConfig::default())
            .map_err(|_| RenderError::InvalidConfig)?;
        Self::new_with_output_safety(config, snapshot_reader, output_safety_reader)
    }

    pub fn new_with_output_safety(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        output_safety_reader: OutputSafetyReader,
    ) -> Result<Self, RenderError> {
        config.validate().map_err(|_| RenderError::InvalidConfig)?;
        if usize::from(config.max_active_sources) > MAX_ACTIVE_SOURCES {
            return Err(RenderError::TooManySources);
        }
        let block_size = config.block_size_frames as usize;
        let maximum_delay_samples =
            (config.sample_rate_hz as f32 * DEFAULT_MAX_DELAY_SECONDS).ceil() as usize;
        let sources = std::array::from_fn(|_| {
            SourceNode::new(block_size, maximum_delay_samples, config.sample_rate_hz)
        });
        let block_period_ns =
            u64::from(config.block_size_frames) * 1_000_000_000 / u64::from(config.sample_rate_hz);
        let deadline_ns = block_period_ns.saturating_mul(8) / 10;
        let block_seconds = config.block_size_frames as f32 / config.sample_rate_hz as f32;
        let snapshot_gain_block_retention =
            (-block_seconds / SNAPSHOT_GAIN_SLEW_TIME_SECONDS).exp();
        let output_safety_gain_block_retention =
            (-block_seconds / OUTPUT_SAFETY_GAIN_SLEW_TIME_SECONDS).exp();
        let realtime_clock = RealtimeClock::new().map_err(RenderError::ClockInitialization)?;
        Ok(Self {
            config,
            realtime_clock,
            snapshot_reader,
            output_safety_reader,
            monitor_route_reader: None,
            sources,
            listener: None,
            backend: None,
            program_backend: None,
            program_plane_counts: [1; MAX_ACTIVE_SOURCES],
            program_right: std::array::from_fn(|_| None),
            spatial_backend: None,
            spatial_scratch: None,
            spatial_program_plane_counts: None,
            spatial_generation: 0,
            spatial_discontinuity_sequence: 0,
            telemetry: Telemetry::default(),
            snapshot_stale_after_ns: DEFAULT_SNAPSHOT_STALE_NS,
            deadline_ns,
            snapshot_gain_block_retention,
            output_safety_gain_block_retention,
            monitor_gain_initialized: false,
            applied_monitor_gain: 1.0,
            true_peak_limiter: TruePeakLimiter::new(config.sample_rate_hz),
        })
    }

    pub fn new_with_backend(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        backend: Box<dyn BackendRenderGraph>,
    ) -> Result<Self, RenderError> {
        let mut graph = Self::new(config, snapshot_reader)?;
        graph.backend = Some(backend);
        Ok(graph)
    }

    pub fn new_with_backend_and_output_safety(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        output_safety_reader: OutputSafetyReader,
        backend: Box<dyn BackendRenderGraph>,
    ) -> Result<Self, RenderError> {
        let mut graph =
            Self::new_with_output_safety(config, snapshot_reader, output_safety_reader)?;
        graph.backend = Some(backend);
        Ok(graph)
    }

    pub fn set_backend_render_graph(&mut self, backend: Option<Box<dyn BackendRenderGraph>>) {
        self.backend = backend;
    }

    pub fn new_with_program_backend_and_output_safety(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        output_safety_reader: OutputSafetyReader,
        program_plane_counts: &[usize],
        backend: Box<dyn BinauralProgramBackend>,
    ) -> Result<Self, RenderError> {
        let mut graph =
            Self::new_with_output_safety(config, snapshot_reader, output_safety_reader)?;
        if program_plane_counts.len() != usize::from(config.max_active_sources) {
            return Err(RenderError::SpatialProgramShapeCountMismatch {
                configured_source_count: usize::from(config.max_active_sources),
                supplied_shape_count: program_plane_counts.len(),
            });
        }
        for (source_index, count) in program_plane_counts.iter().copied().enumerate() {
            if !(1..=MAX_SPATIAL_PROGRAM_PLANES).contains(&count) {
                return Err(RenderError::InvalidSpatialProgramPlaneCount {
                    source_index,
                    supplied_plane_count: count,
                });
            }
            graph.program_plane_counts[source_index] = count;
            if count == 2 {
                graph.program_right[source_index] = Some(ProgramRightScratch {
                    delayed: vec![0.0; graph.block_size_frames()],
                    delay: FractionalDelayLine::new(
                        graph.sources[source_index].delay.maximum_delay_samples() as usize,
                        0.0,
                        DEFAULT_DELAY_SLEW_SAMPLES_PER_SAMPLE,
                    ),
                });
            }
        }
        graph.program_backend = Some(backend);
        Ok(graph)
    }

    /// Constructs the parallel neutral route without changing the preserved
    /// legacy backend route or its buffers.
    ///
    /// `program_plane_counts` is source-index aligned, has exactly
    /// `config.max_active_sources` entries, and freezes each logical source as
    /// mono (`1`) or stereo (`2`) for the lifetime of this graph.
    pub fn new_with_spatial_backend(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        program_plane_counts: &[usize],
        backend: Box<dyn SpatialBackendRenderGraph>,
    ) -> Result<Self, RenderError> {
        let mut graph = Self::new(config, snapshot_reader)?;
        graph.install_spatial_backend(program_plane_counts, backend)?;
        Ok(graph)
    }

    /// Constructs the neutral route with caller-owned source-safety targets
    /// and the same immutable source-index-aligned program shape contract as
    /// [`Self::new_with_spatial_backend`].
    pub fn new_with_spatial_backend_and_output_safety(
        config: EngineConfig,
        snapshot_reader: SnapshotReader<PropagationSnapshot>,
        output_safety_reader: OutputSafetyReader,
        program_plane_counts: &[usize],
        backend: Box<dyn SpatialBackendRenderGraph>,
    ) -> Result<Self, RenderError> {
        let mut graph =
            Self::new_with_output_safety(config, snapshot_reader, output_safety_reader)?;
        graph.install_spatial_backend(program_plane_counts, backend)?;
        Ok(graph)
    }

    fn install_spatial_backend(
        &mut self,
        program_plane_counts: &[usize],
        backend: Box<dyn SpatialBackendRenderGraph>,
    ) -> Result<(), RenderError> {
        let configured_source_count = usize::from(self.config.max_active_sources);
        if program_plane_counts.len() != configured_source_count {
            return Err(RenderError::SpatialProgramShapeCountMismatch {
                configured_source_count,
                supplied_shape_count: program_plane_counts.len(),
            });
        }
        let mut fixed_program_plane_counts = [0; MAX_ACTIVE_SOURCES];
        for (source_index, program_plane_count) in program_plane_counts.iter().copied().enumerate()
        {
            if !(1..=MAX_SPATIAL_PROGRAM_PLANES).contains(&program_plane_count) {
                return Err(RenderError::InvalidSpatialProgramPlaneCount {
                    source_index,
                    supplied_plane_count: program_plane_count,
                });
            }
            fixed_program_plane_counts[source_index] = program_plane_count;
        }
        self.spatial_program_plane_counts = Some(fixed_program_plane_counts);
        self.spatial_scratch = Some(SpatialScratch::new(self.block_size_frames()));
        self.spatial_backend = Some(backend);
        Ok(())
    }

    /// Runs the bound neutral backend's mandatory control-thread preparation.
    ///
    /// This method performs no runtime block processing and therefore does not
    /// advance source clocks, generations, discontinuities, or callback timing.
    pub fn prepare_spatial_backend_for_realtime(
        &mut self,
    ) -> Result<(), SpatialBackendRenderError> {
        self.spatial_backend
            .as_mut()
            .ok_or(SpatialBackendRenderError::InactiveGraph)?
            .prepare_for_realtime()
    }

    /// Copies the latest propagation generation on the audio owner without
    /// advancing render cadence. A subsequent spatial render observes this
    /// same-or-newer immutable snapshot; tokened ingress uses the value to
    /// reject stale command generations before requesting provider PCM.
    #[must_use]
    pub fn observe_spatial_propagation_sequence(&mut self) -> u64 {
        self.snapshot_reader.read().sequence
    }

    /// Installs the callback endpoint for workbench raw/spatial monitoring.
    /// The route defaults to [`MonitorRoute::Spatial`] when this is not called.
    pub fn set_monitor_route_reader(&mut self, reader: MonitorRouteReader) {
        self.monitor_route_reader = Some(reader);
    }

    /// Configures the source's sole physical drive from the API-owned scene
    /// calibration and source declaration.
    pub fn set_source(
        &mut self,
        source_index: usize,
        profile: &SourceProfile,
        calibration: SceneCalibration,
    ) -> Result<SourceDrive, RenderError> {
        if source_index >= usize::from(self.config.max_active_sources) {
            return Err(RenderError::InvalidSourceIndex);
        }
        profile.validate().map_err(RenderError::Source)?;
        let drive = calibration
            .derive_source_drive(profile.reference_level, &profile.asset_analysis)
            .map_err(RenderError::Calibration)?;
        let source = &mut self.sources[source_index];
        source.drive = Some(drive);
        source.reset_smoothing();
        Ok(drive)
    }

    pub fn clear_source(&mut self, source_index: usize) -> Result<(), RenderError> {
        if source_index >= usize::from(self.config.max_active_sources) {
            return Err(RenderError::InvalidSourceIndex);
        }
        let source = self
            .sources
            .get_mut(source_index)
            .ok_or(RenderError::InvalidSourceIndex)?;
        source.drive = None;
        source.set_spectral_transfer(SpectralTransfer::NEUTRAL);
        source.reset_smoothing();
        Ok(())
    }

    /// Installs the one composed environmental transfer for a logical source.
    ///
    /// The fixed transfer is copied to every authored program plane. Runtime
    /// applies it exactly once after source calibration/safety and before the
    /// signal enters either propagation branch.
    pub fn set_source_spectral_transfer(
        &mut self,
        source_index: usize,
        transfer: SpectralTransfer,
    ) -> Result<(), RenderError> {
        if source_index >= usize::from(self.config.max_active_sources) {
            return Err(RenderError::InvalidSourceIndex);
        }
        let source = self
            .sources
            .get_mut(source_index)
            .ok_or(RenderError::InvalidSourceIndex)?;
        source.set_spectral_transfer(transfer);
        Ok(())
    }

    /// Installs a composed control-rate target with the canonical γ7
    /// 0.25 dB-per-128-frame band slew. Macro onset paths that require exact
    /// frame conditioning continue to use the immediate setter above.
    pub fn set_source_spectral_transfer_smoothed(
        &mut self,
        source_index: usize,
        transfer: SpectralTransfer,
    ) -> Result<(), RenderError> {
        if source_index >= usize::from(self.config.max_active_sources) {
            return Err(RenderError::InvalidSourceIndex);
        }
        let source = self
            .sources
            .get_mut(source_index)
            .ok_or(RenderError::InvalidSourceIndex)?;
        source.set_spectral_transfer_smoothed(transfer);
        Ok(())
    }

    /// Returns the source's composed transfer, including visible stage stems.
    pub fn source_spectral_transfer(
        &self,
        source_index: usize,
    ) -> Result<SpectralTransfer, RenderError> {
        if source_index >= usize::from(self.config.max_active_sources) {
            return Err(RenderError::InvalidSourceIndex);
        }
        self.sources
            .get(source_index)
            .map(|source| source.spectral_filters[0].transfer())
            .ok_or(RenderError::InvalidSourceIndex)
    }

    /// Stores the API-owned listener state for late-bound block-rate spatial
    /// rendering. B1's stereo graph shell does not yet apply an HRTF.
    pub fn set_listener_state(&mut self, listener: ListenerState) {
        self.listener = Some(listener);
    }

    #[must_use]
    pub const fn listener_state(&self) -> Option<ListenerState> {
        self.listener
    }

    #[must_use]
    pub const fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    /// Reports exact runtime-owned persistent buffer payload from live
    /// capacities rather than from configuration estimates.
    #[must_use]
    pub fn persistent_memory(&self) -> RuntimeGraphMemoryTelemetry {
        let propagation_delay_payload_bytes = self
            .sources
            .iter()
            .map(|source| source.delay.persistent_sample_bytes())
            .fold(0_u64, u64::saturating_add)
            .saturating_add(
                self.program_right
                    .iter()
                    .flatten()
                    .map(|right| right.delay.persistent_sample_bytes())
                    .fold(0_u64, u64::saturating_add),
            );
        let block_scratch_payload_bytes = self
            .sources
            .iter()
            .map(|source| {
                source
                    .calibrated
                    .capacity()
                    .saturating_add(source.delayed.capacity())
            })
            .map(|samples| {
                u64::try_from(samples)
                    .unwrap_or(u64::MAX)
                    .saturating_mul(core::mem::size_of::<f32>() as u64)
            })
            .fold(0_u64, u64::saturating_add)
            .saturating_add(
                self.program_right
                    .iter()
                    .flatten()
                    .map(|right| {
                        right.delayed.capacity() as u64 * core::mem::size_of::<f32>() as u64
                    })
                    .fold(0_u64, u64::saturating_add),
            );
        let spatial_scratch_payload_bytes = self
            .spatial_scratch
            .as_ref()
            .map_or(0, SpatialScratch::persistent_payload_bytes);
        RuntimeGraphMemoryTelemetry {
            source_node_capacity: self.sources.len(),
            propagation_delay_payload_bytes,
            block_scratch_payload_bytes,
            spatial_scratch_payload_bytes,
            total_payload_bytes: propagation_delay_payload_bytes
                .saturating_add(block_scratch_payload_bytes)
                .saturating_add(spatial_scratch_payload_bytes),
        }
    }

    #[must_use]
    pub const fn fault_counters(&self) -> FaultCounters {
        self.telemetry.faults
    }

    #[must_use]
    pub const fn safety_telemetry(&self) -> SafetyTelemetry {
        self.telemetry.safety
    }

    pub fn record_deadline_miss(&mut self) {
        self.telemetry.faults.deadline_miss = self.telemetry.faults.deadline_miss.saturating_add(1);
    }

    fn validate_block(&self, block: &ProcessBlock<'_>) -> Result<(), RenderError> {
        let block_size = self.block_size_frames();
        if block.output_left.len() != block_size || block.output_right.len() != block_size {
            return Err(RenderError::InvalidBlockLength);
        }
        let mut seen = [false; MAX_ACTIVE_SOURCES];
        for source in block.sources {
            if source.source_index >= usize::from(self.config.max_active_sources) {
                return Err(RenderError::InvalidSourceIndex);
            }
            if source.decoded_mono.len() != block_size {
                return Err(RenderError::InvalidBlockLength);
            }
            if seen[source.source_index] {
                return Err(RenderError::DuplicateSourceBlock);
            }
            seen[source.source_index] = true;
        }
        Ok(())
    }

    fn validate_spatial_block(
        &self,
        block: &SpatialProcessBlock<'_>,
    ) -> Result<[bool; MAX_ACTIVE_SOURCES], SpatialRenderError> {
        let block_size = self.block_size_frames();
        let presentation_samples = MAX_SPATIAL_PRESENTATION_FEEDS
            .checked_mul(block_size)
            .ok_or(SpatialRenderError::InvalidOutputBankLength)?;
        let environmental_samples = MAX_SPATIAL_ENVIRONMENT_PLANES
            .checked_mul(block_size)
            .ok_or(SpatialRenderError::InvalidOutputBankLength)?;
        if block.presentation_bank.len() != presentation_samples
            || block.environmental_bank.len() != environmental_samples
        {
            return Err(SpatialRenderError::InvalidOutputBankLength);
        }
        if block.sources.len() > usize::from(self.config.max_active_sources) {
            return Err(SpatialRenderError::TooManySources);
        }

        let mut seen = [false; MAX_ACTIVE_SOURCES];
        for source in block.sources {
            if source.source_index >= usize::from(self.config.max_active_sources) {
                return Err(SpatialRenderError::InvalidSourceIndex);
            }
            if !(1..=MAX_SPATIAL_PROGRAM_PLANES).contains(&source.program_plane_count) {
                return Err(SpatialRenderError::InvalidProgramPlaneCount);
            }
            if seen[source.source_index] {
                return Err(SpatialRenderError::DuplicateSourceBlock);
            }
            seen[source.source_index] = true;
            if let Some(configured_program_plane_counts) = self.spatial_program_plane_counts {
                let configured_plane_count = configured_program_plane_counts[source.source_index];
                if source.program_plane_count != configured_plane_count {
                    return Err(SpatialRenderError::ConfiguredProgramShapeMismatch {
                        source_index: source.source_index,
                        configured_plane_count,
                        supplied_plane_count: source.program_plane_count,
                    });
                }
            }

            for plane in &source.program_planes[..source.program_plane_count] {
                if plane.len() != block_size {
                    return Err(SpatialRenderError::InvalidBlockLength);
                }
            }
            for plane in &source.program_planes[source.program_plane_count..] {
                if !plane.is_empty() {
                    return Err(SpatialRenderError::InactiveProgramPlaneNotEmpty);
                }
            }
        }
        Ok(seen)
    }

    fn spatial_metadata_for_block(
        &self,
        block_start_frame: u64,
        validity: SpatialOutputValidity,
    ) -> SpatialOutputMetadata {
        SpatialOutputMetadata {
            sample_rate_hz: self.config.sample_rate_hz,
            block_size_frames: self.config.block_size_frames,
            block_start_frame,
            validity,
            generation: self.spatial_generation,
            discontinuity_sequence: self.spatial_discontinuity_sequence,
            world_space_unrotated: true,
            source_drive_applied: true,
            source_safety_gain_applied: true,
            monitor_gain_applied: false,
            final_hrtf_applied: false,
            output_limiter_applied: false,
            ..SpatialOutputMetadata::default()
        }
    }

    fn spatial_output_metadata_is_valid(&self, metadata: &SpatialOutputMetadata) -> bool {
        if metadata.validity != SpatialOutputValidity::Valid
            || metadata.generation < self.spatial_generation
            || metadata.active_presentation_feed_count > MAX_SPATIAL_PRESENTATION_FEEDS
            || metadata.active_environmental_plane_count
                != metadata.active_environmental_order.channel_count()
        {
            return false;
        }
        if !matches!(
            metadata.environmental_basis,
            crate::backend::SpatialEnvironmentalBasis::RightHandedEnu
                | crate::backend::SpatialEnvironmentalBasis::RightHandedXRightYUpZBack
        ) {
            return false;
        }

        let mut active_feed_count = 0;
        for (plane_index, feed) in metadata.presentation_feeds.iter().enumerate() {
            if !feed.valid {
                continue;
            }
            active_feed_count += 1;
            if feed.source_index >= usize::from(self.config.max_active_sources) {
                return false;
            }
            let Some(component_slot) = feed.component.presentation_slot() else {
                // The discrete-echo bit is reserved but is not an object plane.
                return false;
            };
            let expected_plane =
                feed.source_index * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE + component_slot;
            if plane_index != expected_plane {
                return false;
            }
            let pose_is_finite = feed.pose_enu.position.is_finite()
                && feed.pose_enu.forward.is_finite()
                && feed.pose_enu.up.is_finite();
            let placement_is_valid = match feed.placement {
                SpatialFeedPlacement::Pose => pose_is_finite,
                SpatialFeedPlacement::Direction => {
                    let direction = feed.direction_enu;
                    let length_squared = direction.east_m * direction.east_m
                        + direction.north_m * direction.north_m
                        + direction.up_m * direction.up_m;
                    pose_is_finite
                        && direction.is_finite()
                        && (length_squared - 1.0).abs()
                            <= SPATIAL_DIRECTION_LENGTH_SQUARED_TOLERANCE
                }
            };
            if !placement_is_valid {
                return false;
            }
        }
        active_feed_count == metadata.active_presentation_feed_count
    }

    fn spatial_active_output_is_finite(
        block_size: usize,
        presentation_bank: &[f32],
        environmental_bank: &[f32],
        metadata: &SpatialOutputMetadata,
    ) -> bool {
        for (plane_index, feed) in metadata.presentation_feeds.iter().enumerate() {
            if !feed.valid {
                continue;
            }
            let start = plane_index * block_size;
            if !presentation_bank[start..start + block_size]
                .iter()
                .copied()
                .all(f32::is_finite)
            {
                return false;
            }
        }
        let active_environmental_samples = metadata.active_environmental_plane_count * block_size;
        environmental_bank[..active_environmental_samples]
            .iter()
            .copied()
            .all(f32::is_finite)
    }

    fn stamp_spatial_contract_fields(
        &self,
        block_start_frame: u64,
        metadata: &mut SpatialOutputMetadata,
    ) {
        metadata.sample_rate_hz = self.config.sample_rate_hz;
        metadata.block_size_frames = self.config.block_size_frames;
        metadata.block_start_frame = block_start_frame;
        metadata.discontinuity_sequence = self.spatial_discontinuity_sequence;
        metadata.environmental_channel_order = SpatialAmbisonicChannelOrder::Acn;
        metadata.environmental_normalization = SpatialAmbisonicNormalization::N3d;
        metadata.world_space_unrotated = true;
        metadata.source_drive_applied = true;
        metadata.source_safety_gain_applied = true;
        metadata.monitor_gain_applied = false;
        metadata.final_hrtf_applied = false;
        metadata.output_limiter_applied = false;
    }

    fn normalize_inactive_spatial_outputs(
        block_size: usize,
        presentation_bank: &mut [f32],
        environmental_bank: &mut [f32],
        metadata: &mut SpatialOutputMetadata,
    ) {
        for (plane_index, feed) in metadata.presentation_feeds.iter_mut().enumerate() {
            if feed.valid {
                continue;
            }
            let start = plane_index * block_size;
            presentation_bank[start..start + block_size].fill(0.0);
            *feed = Default::default();
        }
        for plane_index in metadata.active_environmental_plane_count..MAX_SPATIAL_ENVIRONMENT_PLANES
        {
            let start = plane_index * block_size;
            environmental_bank[start..start + block_size].fill(0.0);
        }
    }

    fn publish_spatial_backend_failure(
        &mut self,
        block_start_frame: u64,
        presentation_bank: &mut [f32],
        environmental_bank: &mut [f32],
        metadata: &mut SpatialOutputMetadata,
    ) {
        presentation_bank.fill(0.0);
        environmental_bank.fill(0.0);
        self.telemetry.faults.backend_render_error =
            self.telemetry.faults.backend_render_error.saturating_add(1);
        self.spatial_discontinuity_sequence = self.spatial_discontinuity_sequence.saturating_add(1);
        *metadata = self.spatial_metadata_for_block(
            block_start_frame,
            SpatialOutputValidity::SilentDiscontinuity,
        );
    }

    /// Renders calibrated one- or two-plane source programs into the neutral
    /// pre-HRTF presentation and environmental banks.
    ///
    /// The method performs no construction, allocation, locking, formatting,
    /// clock syscall, monitor gain, final HRTF, or output limiting. Backend
    /// failures keep the callback alive by returning `Ok` with a fully zeroed
    /// silent-discontinuity block. Caller-shape and active-coverage errors are
    /// non-advancing: they leave output banks, metadata, runtime source clocks,
    /// telemetry, and the backend untouched.
    pub fn process_spatial_block(
        &mut self,
        block: SpatialProcessBlock<'_>,
    ) -> Result<(), SpatialRenderError> {
        let supplied_sources = self.validate_spatial_block(&block)?;
        if self.spatial_backend.is_none() {
            return Err(SpatialRenderError::SpatialBackendUnavailable);
        }
        if self.spatial_scratch.is_none() || self.spatial_program_plane_counts.is_none() {
            return Err(SpatialRenderError::SpatialBackendUnavailable);
        }

        // This one snapshot defines activity for the complete callback. An
        // omitted active program is a caller error, not implicit silence: do
        // not touch outputs, metadata, safety smoothing, backend clocks, or
        // fault telemetry until the active set has been proven complete.
        let snapshot = self.snapshot_reader.read();
        let configured_source_count = usize::from(self.config.max_active_sources);
        for (source_index, supplied) in supplied_sources
            .iter()
            .copied()
            .enumerate()
            .take(configured_source_count)
        {
            if !snapshot.sources[source_index].active {
                continue;
            }
            if !supplied {
                return Err(SpatialRenderError::MissingActiveProgram { source_index });
            }
            if self.sources[source_index].drive.is_none() {
                return Err(SpatialRenderError::ActiveSourceNotConfigured { source_index });
            }
        }

        // Inactive program records are optional. Reset their runtime-owned
        // clocks from the authoritative snapshot even when the caller omits
        // them, so reactivation cannot inherit a stale safety or gain ramp.
        for (source_index, source) in self
            .sources
            .iter_mut()
            .enumerate()
            .take(configured_source_count)
        {
            if !snapshot.sources[source_index].active {
                source.reset_smoothing();
            }
        }

        block.presentation_bank.fill(0.0);
        block.environmental_bank.fill(0.0);
        *block.metadata = self
            .spatial_metadata_for_block(block.block_start_frame, SpatialOutputValidity::Invalid);

        let (source_safety_targets, _monitor_gain_target) = self.output_safety_reader.read();
        if block.now_ns.saturating_sub(snapshot.simulated_at_ns) > self.snapshot_stale_after_ns {
            self.telemetry.faults.snapshot_stale =
                self.telemetry.faults.snapshot_stale.saturating_add(1);
        }

        let block_size = self.block_size_frames();
        for input in block.sources {
            let source = &self.sources[input.source_index];
            if source.drive.is_none() {
                continue;
            }
            let propagation = snapshot.sources[input.source_index];
            if propagation.active
                && (!propagation.target_delay_samples.is_finite()
                    || propagation.target_delay_samples < 0.0
                    || propagation.target_delay_samples > source.delay.maximum_delay_samples()
                    || !propagation.left_gain.is_finite()
                    || !propagation.right_gain.is_finite())
            {
                self.publish_spatial_backend_failure(
                    block.block_start_frame,
                    block.presentation_bank,
                    block.environmental_bank,
                    block.metadata,
                );
                return Ok(());
            }
        }

        let spatial_scratch = self
            .spatial_scratch
            .as_mut()
            .ok_or(SpatialRenderError::SpatialBackendUnavailable)?;
        let mut backend_source_indices = [0_usize; MAX_ACTIVE_SOURCES];
        let mut backend_program_plane_counts = [0_usize; MAX_ACTIVE_SOURCES];
        let mut backend_source_count = 0;
        for input in block.sources {
            let propagation = snapshot.sources[input.source_index];
            let source = &mut self.sources[input.source_index];
            let Some(drive) = source.drive else {
                continue;
            };
            if !propagation.active {
                continue;
            }

            let mut safety_gain = source.prepare_safety_gain_ramp(
                source_safety_targets[input.source_index],
                self.output_safety_gain_block_retention,
                block_size,
            );
            if safety_gain.engages_source_safety() {
                self.telemetry.safety.proximity_ceiling_engagements = self
                    .telemetry
                    .safety
                    .proximity_ceiling_engagements
                    .saturating_add(1);
            }

            let scratch = &mut spatial_scratch.sources[input.source_index];
            for frame in 0..block_size {
                let source_safety_gain = safety_gain.next(frame, block_size);
                // Compute the one source scalar once and apply that identical
                // value exactly once to every present program plane.
                let source_scalar = drive.linear_gain() * source_safety_gain;
                let calibrated_left = input.program_planes[0][frame] * source_scalar;
                scratch.calibrated_planes[0][frame] =
                    source.apply_spectral_transfer(0, calibrated_left);
                scratch.calibrated_planes[1][frame] = if input.program_plane_count == 2 {
                    let calibrated_right = input.program_planes[1][frame] * source_scalar;
                    source.apply_spectral_transfer(1, calibrated_right)
                } else {
                    0.0
                };
            }
            source.applied_safety_gain = safety_gain.endpoint;
            backend_source_indices[backend_source_count] = input.source_index;
            backend_program_plane_counts[backend_source_count] = input.program_plane_count;
            backend_source_count += 1;
        }

        let backend_sources: [SpatialBackendSourceBlock<'_>; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|slot| {
                if slot >= backend_source_count {
                    return SpatialBackendSourceBlock {
                        source_index: 0,
                        program_plane_count: 0,
                        program_planes: [&[], &[]],
                    };
                }
                let source_index = backend_source_indices[slot];
                let program_plane_count = backend_program_plane_counts[slot];
                SpatialBackendSourceBlock {
                    source_index,
                    program_plane_count,
                    program_planes: [
                        &spatial_scratch.sources[source_index].calibrated_planes[0],
                        if program_plane_count == 2 {
                            &spatial_scratch.sources[source_index].calibrated_planes[1]
                        } else {
                            &[]
                        },
                    ],
                }
            });

        let backend_result = self
            .spatial_backend
            .as_mut()
            .ok_or(SpatialRenderError::SpatialBackendUnavailable)?
            .render_spatial_block(SpatialPropagationRenderBlock {
                block_start_frame: block.block_start_frame,
                propagation_sequence: snapshot.sequence,
                sources: &backend_sources[..backend_source_count],
                presentation_bank: block.presentation_bank,
                environmental_bank: block.environmental_bank,
                metadata: block.metadata,
            });
        if let Err(_error) = backend_result {
            self.publish_spatial_backend_failure(
                block.block_start_frame,
                block.presentation_bank,
                block.environmental_bank,
                block.metadata,
            );
            return Ok(());
        }
        if !self.spatial_output_metadata_is_valid(block.metadata) {
            self.publish_spatial_backend_failure(
                block.block_start_frame,
                block.presentation_bank,
                block.environmental_bank,
                block.metadata,
            );
            return Ok(());
        }
        if !Self::spatial_active_output_is_finite(
            block_size,
            block.presentation_bank,
            block.environmental_bank,
            block.metadata,
        ) {
            self.publish_spatial_backend_failure(
                block.block_start_frame,
                block.presentation_bank,
                block.environmental_bank,
                block.metadata,
            );
            return Ok(());
        }

        self.spatial_generation = block.metadata.generation;
        self.stamp_spatial_contract_fields(block.block_start_frame, block.metadata);
        Self::normalize_inactive_spatial_outputs(
            block_size,
            block.presentation_bank,
            block.environmental_bank,
            block.metadata,
        );
        Ok(())
    }
}

impl BlockProcessor for RuntimeGraph {
    fn block_size_frames(&self) -> usize {
        self.config.block_size_frames as usize
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        self.validate_block(&block)?;
        let sources = std::array::from_fn::<_, MAX_ACTIVE_SOURCES, _>(|slot| {
            let source = block.sources.get(slot);
            SpatialProgramBlock {
                source_index: source.map_or(0, |source| source.source_index),
                program_plane_count: 1,
                program_planes: [source.map_or(&[][..], |source| source.decoded_mono), &[]],
            }
        });
        self.process_program_block(ProgramProcessBlock {
            now_ns: block.now_ns,
            sources: &sources[..block.sources.len()],
            output_left: block.output_left,
            output_right: block.output_right,
        })
    }

    fn process_program_block(&mut self, block: ProgramProcessBlock<'_>) -> Result<(), RenderError> {
        let block_size = self.block_size_frames();
        if block.output_left.len() != block_size || block.output_right.len() != block_size {
            return Err(RenderError::InvalidBlockLength);
        }
        let mut seen = [false; MAX_ACTIVE_SOURCES];
        for source in block.sources {
            if source.source_index >= usize::from(self.config.max_active_sources) {
                return Err(RenderError::InvalidSourceIndex);
            }
            if source.program_plane_count != self.program_plane_counts[source.source_index] {
                return Err(RenderError::InvalidSpatialProgramPlaneCount {
                    source_index: source.source_index,
                    supplied_plane_count: source.program_plane_count,
                });
            }
            if source.program_planes[..source.program_plane_count]
                .iter()
                .any(|plane| plane.len() != block_size)
                || source.program_planes[source.program_plane_count..]
                    .iter()
                    .any(|plane| !plane.is_empty())
            {
                return Err(RenderError::InvalidBlockLength);
            }
            if seen[source.source_index] {
                return Err(RenderError::DuplicateSourceBlock);
            }
            seen[source.source_index] = true;
        }
        let started = self.realtime_clock.start();
        block.output_left.fill(0.0);
        block.output_right.fill(0.0);

        let snapshot = self.snapshot_reader.read();
        let (source_safety_targets, monitor_gain_target) = self.output_safety_reader.read();
        let monitor_route = self
            .monitor_route_reader
            .as_mut()
            .map_or(MonitorRoute::Spatial, MonitorRouteReader::read);
        if block.now_ns.saturating_sub(snapshot.simulated_at_ns) > self.snapshot_stale_after_ns {
            self.telemetry.faults.snapshot_stale =
                self.telemetry.faults.snapshot_stale.saturating_add(1);
        }

        let block_size = self.block_size_frames();
        let use_simple_gain = self.backend.is_none() && self.program_backend.is_none();
        let mut backend_source_indices = [0_usize; MAX_ACTIVE_SOURCES];
        let mut backend_source_count = 0;
        for input in block.sources {
            let propagation = snapshot.sources[input.source_index];
            let source = &mut self.sources[input.source_index];
            let Some(drive) = source.drive else {
                continue;
            };
            if !propagation.active
                || !propagation.target_delay_samples.is_finite()
                || propagation.target_delay_samples < 0.0
                || propagation.target_delay_samples > source.delay.maximum_delay_samples()
                || !propagation.left_gain.is_finite()
                || !propagation.right_gain.is_finite()
            {
                if propagation.active {
                    return Err(RenderError::InvalidPropagation);
                }
                source.reset_smoothing();
                continue;
            }

            if !source.delay_initialized {
                source.applied_delay_samples = propagation.target_delay_samples;
                source.delay_initialized = true;
            }
            if !source.snapshot_gain_initialized {
                source.applied_left_gain = propagation.left_gain;
                source.applied_right_gain = propagation.right_gain;
                source.snapshot_gain_initialized = true;
            }
            let source_safety_target = source_safety_targets[input.source_index];

            let delay_step = (propagation.target_delay_samples - source.applied_delay_samples)
                / block_size as f32;
            let left_gain_target = propagation.left_gain
                + (source.applied_left_gain - propagation.left_gain)
                    * self.snapshot_gain_block_retention;
            let right_gain_target = propagation.right_gain
                + (source.applied_right_gain - propagation.right_gain)
                    * self.snapshot_gain_block_retention;
            let left_gain_step = (left_gain_target - source.applied_left_gain) / block_size as f32;
            let right_gain_step =
                (right_gain_target - source.applied_right_gain) / block_size as f32;
            let mut source_safety_gain = source.prepare_safety_gain_ramp(
                source_safety_target,
                self.output_safety_gain_block_retention,
                block_size,
            );
            let mut delay_samples = source.applied_delay_samples;
            let mut left_gain = source.applied_left_gain;
            let mut right_gain = source.applied_right_gain;
            if source_safety_gain.engages_source_safety() {
                self.telemetry.safety.proximity_ceiling_engagements = self
                    .telemetry
                    .safety
                    .proximity_ceiling_engagements
                    .saturating_add(1);
            }
            for frame in 0..block_size {
                // Advance before applying so the last sample lands exactly on
                // the block target instead of accumulating a one-sample lag.
                if frame + 1 == block_size {
                    delay_samples = propagation.target_delay_samples;
                    left_gain = left_gain_target;
                    right_gain = right_gain_target;
                } else {
                    delay_samples += delay_step;
                    left_gain += left_gain_step;
                    right_gain += right_gain_step;
                }
                let applied_source_safety_gain = source_safety_gain.next(frame, block_size);
                let calibrated = input.program_planes[0][frame]
                    * drive.linear_gain()
                    * applied_source_safety_gain;
                source.calibrated[frame] = source.apply_spectral_transfer(0, calibrated);
                source.delayed[frame] = source
                    .delay
                    .process_sample_at_delay(source.calibrated[frame], delay_samples);
                if input.program_plane_count == 2 {
                    let calibrated_right = input.program_planes[1][frame]
                        * drive.linear_gain()
                        * applied_source_safety_gain;
                    let filtered_right = source.apply_spectral_transfer(1, calibrated_right);
                    let right = self.program_right[input.source_index]
                        .as_mut()
                        .expect("stereo scratch allocated before rendering");
                    right.delayed[frame] = right
                        .delay
                        .process_sample_at_delay(filtered_right, delay_samples);
                }
                if use_simple_gain {
                    block.output_left[frame] += source.delayed[frame] * left_gain;
                    block.output_right[frame] += source.delayed[frame] * right_gain;
                }
            }
            // Assign the analytically computed endpoints to avoid float-add
            // drift and guarantee no lag accumulation across blocks.
            source.applied_delay_samples = propagation.target_delay_samples;
            source.applied_left_gain = left_gain_target;
            source.applied_right_gain = right_gain_target;
            source.applied_safety_gain = source_safety_gain.endpoint;
            backend_source_indices[backend_source_count] = input.source_index;
            backend_source_count += 1;
        }

        if let Some(backend) = self.backend.as_mut() {
            let backend_sources: [BackendSourceBlock<'_>; MAX_ACTIVE_SOURCES] =
                std::array::from_fn(|slot| {
                    let source_index = backend_source_indices[slot];
                    BackendSourceBlock {
                        source_index,
                        input_mono: if slot < backend_source_count {
                            &self.sources[source_index].delayed
                        } else {
                            &[]
                        },
                    }
                });
            let listener_orientation = self
                .listener
                .map(|listener| ListenerOrientation {
                    forward: listener.pose.forward,
                    up: listener.pose.up,
                })
                .unwrap_or(ListenerOrientation {
                    forward: fightbox_api::EnuVector3::new(0.0, 1.0, 0.0),
                    up: fightbox_api::EnuVector3::new(0.0, 0.0, 1.0),
                });
            if backend
                .render_block(PropagationRenderBlock {
                    listener_orientation,
                    sources: &backend_sources[..backend_source_count],
                    output_left: block.output_left,
                    output_right: block.output_right,
                })
                .is_err()
            {
                self.telemetry.faults.backend_render_error =
                    self.telemetry.faults.backend_render_error.saturating_add(1);
                block.output_left.fill(0.0);
                block.output_right.fill(0.0);
            }
        }

        if let Some(backend) = self.program_backend.as_mut() {
            let backend_sources: [SpatialBackendSourceBlock<'_>; MAX_ACTIVE_SOURCES] =
                std::array::from_fn(|slot| {
                    let source_index = backend_source_indices[slot];
                    SpatialBackendSourceBlock {
                        source_index,
                        program_plane_count: self.program_plane_counts[source_index],
                        program_planes: [
                            if slot < backend_source_count {
                                &self.sources[source_index].delayed
                            } else {
                                &[]
                            },
                            if slot < backend_source_count {
                                self.program_right[source_index]
                                    .as_ref()
                                    .map_or(&[][..], |right| &right.delayed)
                            } else {
                                &[]
                            },
                        ],
                    }
                });
            let listener_orientation = self
                .listener
                .map(|listener| ListenerOrientation {
                    forward: listener.pose.forward,
                    up: listener.pose.up,
                })
                .unwrap_or(ListenerOrientation {
                    forward: fightbox_api::EnuVector3::new(0.0, 1.0, 0.0),
                    up: fightbox_api::EnuVector3::new(0.0, 0.0, 1.0),
                });
            if backend
                .render_program_block(ProgramRenderBlock {
                    listener_orientation,
                    sources: &backend_sources[..backend_source_count],
                    output_left: block.output_left,
                    output_right: block.output_right,
                })
                .is_err()
            {
                self.telemetry.faults.backend_render_error =
                    self.telemetry.faults.backend_render_error.saturating_add(1);
                block.output_left.fill(0.0);
                block.output_right.fill(0.0);
            }
        }

        if let MonitorRoute::RawSource { source_index } = monitor_route {
            // Keep the backend running normally so switching back to SPATIAL
            // does not pause its source/effect state, but expose no spatial
            // tail on the raw-only monitor route.
            block.output_left.fill(0.0);
            block.output_right.fill(0.0);
            if let Some(input) = block
                .sources
                .iter()
                .find(|input| input.source_index == source_index)
            {
                for frame in 0..block_size {
                    let raw = input.program_planes[0][frame] * RAW_MONITOR_PAD_GAIN;
                    block.output_left[frame] += raw;
                    block.output_right[frame] += if input.program_plane_count == 2 {
                        input.program_planes[1][frame] * RAW_MONITOR_PAD_GAIN
                    } else {
                        raw
                    };
                }
            }
        }

        if !self.monitor_gain_initialized {
            self.applied_monitor_gain = monitor_gain_target;
            self.monitor_gain_initialized = true;
        }
        let monitor_gain_endpoint = monitor_gain_target
            + (self.applied_monitor_gain - monitor_gain_target)
                * self.output_safety_gain_block_retention;
        let monitor_gain_step =
            (monitor_gain_endpoint - self.applied_monitor_gain) / block_size as f32;
        let mut monitor_gain = self.applied_monitor_gain;
        for frame in 0..block_size {
            if frame + 1 == block_size {
                monitor_gain = monitor_gain_endpoint;
            } else {
                monitor_gain += monitor_gain_step;
            }
            block.output_left[frame] *= monitor_gain;
            block.output_right[frame] *= monitor_gain;
        }

        // NaN or infinity defeats the limiter: its peak detector skips NaN
        // and its gain turns infinity into NaN. Checked after the monitor
        // gain, which can overflow a finite sample, the block is silenced.
        if !block
            .output_left
            .iter()
            .chain(block.output_right.iter())
            .all(|sample| sample.is_finite())
        {
            block.output_left.fill(0.0);
            block.output_right.fill(0.0);
            self.telemetry.safety.non_finite_blocks =
                self.telemetry.safety.non_finite_blocks.saturating_add(1);
        }

        let mut limiter_engaged = false;
        for frame in 0..block_size {
            let pre_left = block.output_left[frame];
            let pre_right = block.output_right[frame];
            self.telemetry.safety.pre_limiter_peak = self
                .telemetry
                .safety
                .pre_limiter_peak
                .max(pre_left.abs())
                .max(pre_right.abs());
            let (post_left, post_right, engaged) =
                self.true_peak_limiter.process_stereo(pre_left, pre_right);
            limiter_engaged |= engaged;
            block.output_left[frame] = post_left;
            block.output_right[frame] = post_right;
            self.telemetry.safety.post_limiter_peak = self
                .telemetry
                .safety
                .post_limiter_peak
                .max(post_left.abs())
                .max(post_right.abs());
        }
        self.applied_monitor_gain = monitor_gain_endpoint;
        if limiter_engaged {
            self.telemetry.safety.limiter_engagements =
                self.telemetry.safety.limiter_engagements.saturating_add(1);
        }

        let duration_ns = self.realtime_clock.elapsed_ns(started);
        self.telemetry.timings.record(duration_ns);
        if duration_ns > self.deadline_ns {
            self.record_deadline_miss();
        }
        Ok(())
    }

    fn fault_counters(&self) -> FaultCounters {
        self.fault_counters()
    }

    fn safety_telemetry(&self) -> SafetyTelemetry {
        self.safety_telemetry()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackendRenderError, PropagationRenderBlock, SpatialAmbisonicOrder,
        SpatialBackendRenderError, SpatialEnvironmentalBasis, SpatialPresentationComponent,
        SpatialProgramBlock,
    };
    use crate::{
        MonitorRoutePublication, SnapshotPublication, TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES,
    };
    use fightbox_api::{
        AssetAnalysis, AssetMeasurementProvenance, EnuVector3, ExtentDescriptor, Pose,
        ReferenceLevel, SourceId,
    };
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    struct CountingAllocator;

    thread_local! {
        static TRACK_ALLOCATIONS: Cell<bool> = const { Cell::new(false) };
        static ALLOCATION_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    // SAFETY: every operation delegates directly to `System`; the thread-local
    // counter observes calls without changing their allocation semantics.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: the caller supplies the layout under `GlobalAlloc`'s
            // contract, which is forwarded unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: the pointer and layout came from the delegated allocator.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: the caller-supplied layout is forwarded unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            TRACK_ALLOCATIONS.with(|tracking| {
                if tracking.get() {
                    ALLOCATION_COUNT.with(|count| count.set(count.get() + 1));
                }
            });
            // SAFETY: all arguments are forwarded under `GlobalAlloc`'s
            // reallocation contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static GLOBAL_ALLOCATOR: CountingAllocator = CountingAllocator;

    fn count_allocations(operation: impl FnOnce()) -> usize {
        ALLOCATION_COUNT.with(|count| count.set(0));
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(true));
        operation();
        TRACK_ALLOCATIONS.with(|tracking| tracking.set(false));
        ALLOCATION_COUNT.with(Cell::get)
    }

    fn source_profile(level: ReferenceLevel) -> SourceProfile {
        SourceProfile {
            id: SourceId::new("test-source"),
            pose: Pose {
                position: EnuVector3::default(),
                forward: EnuVector3::new(0.0, 1.0, 0.0),
                up: EnuVector3::new(0.0, 0.0, 1.0),
            },
            reference_level: level,
            asset_analysis: AssetAnalysis::new(
                -20.0,
                -1.0,
                AssetMeasurementProvenance::new("runtime-test-rms+true-peak/v1").unwrap(),
            )
            .unwrap(),
            extent: ExtentDescriptor::Point,
            directivity: fightbox_api::Directivity::default(),
            max_speed_mps: 100.0,
        }
    }

    fn test_graph(block_size: u32) -> (crate::SnapshotWriter<PropagationSnapshot>, RuntimeGraph) {
        let (writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            block_size_frames: block_size,
            ..EngineConfig::default()
        };
        // Pin the monitor gain to unity: these tests assert render mechanics
        // (calibrated drive, slew bounds, limiter behavior) and must not
        // depend on the tunable DEFAULT_MONITOR_GAIN_DB monitoring default.
        let (_, output_safety_reader) = OutputSafetyPublication::new(OutputSafetyConfig {
            monitor_gain_db: 0.0,
            ..OutputSafetyConfig::default()
        })
        .unwrap();
        (
            writer,
            RuntimeGraph::new_with_output_safety(config, reader, output_safety_reader).unwrap(),
        )
    }

    #[test]
    fn offline_driver_uses_calibrated_per_source_graph_and_stereo_bus() {
        let (mut writer, mut graph) = test_graph(16);
        let drive = graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 6.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 1,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < 2,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 0.5,
            }),
        });

        let input = [0.1; 16];
        let source_blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0; 16];
        let mut right = [0.0; 16];
        let mut offline = OfflineDriver::new(graph);
        for _ in 0..3 {
            offline
                .process_block(ProcessBlock {
                    now_ns: 1,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        }

        for frame in 0..16 {
            assert!((left[frame] - drive.linear_gain() * 0.1).abs() < 1.0e-6);
            assert!((right[frame] - drive.linear_gain() * 0.05).abs() < 1.0e-6);
        }
        assert_eq!(offline.processor().telemetry().timings.len(), 3);
    }

    #[test]
    fn composed_spectral_transfer_is_applied_once_before_the_stereo_branch() {
        use fightbox_api::spectral::SpectralStage;

        let (mut writer, mut graph) = test_graph(16);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let transfer = SpectralTransfer::default()
            .with_stage(SpectralStage::Directivity, [-6.0; 8])
            .unwrap();
        graph.set_source_spectral_transfer(0, transfer).unwrap();
        assert_eq!(graph.source_spectral_transfer(0).unwrap(), transfer);
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 1,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 0.5,
            }),
        });

        let input = [0.01_f32; 16];
        let source_blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0; 16];
        let mut right = [0.0; 16];
        for _ in 0..3 {
            graph
                .process_block(ProcessBlock {
                    now_ns: 1,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        }

        let one_application = input[0] * 10.0_f32.powf(-6.0 / 20.0);
        let accidental_double_application = one_application * 10.0_f32.powf(-6.0 / 20.0);
        for frame in 0..16 {
            assert!((left[frame] - one_application).abs() < 1.0e-7);
            assert!((right[frame] - one_application * 0.5).abs() < 1.0e-7);
            assert!((left[frame] - accidental_double_application).abs() > 0.001);
        }
    }

    #[test]
    fn explicit_neutral_spectral_transfer_preserves_runtime_output_bits() {
        let (mut baseline_writer, mut baseline) = test_graph(16);
        let (mut neutral_writer, mut explicit_neutral) = test_graph(16);
        for graph in [&mut baseline, &mut explicit_neutral] {
            graph
                .set_source(
                    0,
                    &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                    SceneCalibration::default(),
                )
                .unwrap();
        }
        explicit_neutral
            .set_source_spectral_transfer(0, SpectralTransfer::NEUTRAL)
            .unwrap();
        let snapshot = PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 1,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 0.75,
                right_gain: 0.25,
            }),
        };
        baseline_writer.publish(snapshot);
        neutral_writer.publish(snapshot);

        let input = std::array::from_fn::<_, 16, _>(|frame| {
            ((frame as f32 * 0.731).sin() + (frame as f32 * 0.173).cos()) * 0.01
        });
        let blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut baseline_left = [0.0; 16];
        let mut baseline_right = [0.0; 16];
        let mut neutral_left = [0.0; 16];
        let mut neutral_right = [0.0; 16];
        for _ in 0..3 {
            baseline
                .process_block(ProcessBlock {
                    now_ns: 1,
                    sources: &blocks,
                    output_left: &mut baseline_left,
                    output_right: &mut baseline_right,
                })
                .unwrap();
            explicit_neutral
                .process_block(ProcessBlock {
                    now_ns: 1,
                    sources: &blocks,
                    output_left: &mut neutral_left,
                    output_right: &mut neutral_right,
                })
                .unwrap();
        }

        for frame in 0..16 {
            assert_eq!(
                baseline_left[frame].to_bits(),
                neutral_left[frame].to_bits()
            );
            assert_eq!(
                baseline_right[frame].to_bits(),
                neutral_right[frame].to_bits()
            );
        }
    }

    #[test]
    fn published_safety_targets_are_source_local_and_monitor_gain_slews() {
        let (mut propagation_writer, propagation_reader) =
            SnapshotPublication::new(PropagationSnapshot::default());
        let (mut safety_control, safety_reader) =
            OutputSafetyPublication::new(OutputSafetyConfig {
                // Unity start so the slew assertions are default-independent.
                monitor_gain_db: 0.0,
                ..OutputSafetyConfig::default()
            })
            .unwrap();
        let physical = source_profile(ReferenceLevel::SplAtOneMeter { db_spl: 120.0 });
        let creative = source_profile(ReferenceLevel::CreativeDb { db: 0.0 });
        safety_control.set_source(0, &physical, None).unwrap();
        safety_control.set_source(1, &creative, None).unwrap();
        safety_control
            .set_listener_position(EnuVector3::new(1.0, 0.0, 0.0))
            .unwrap();

        let config = EngineConfig {
            block_size_frames: 128,
            max_active_sources: 2,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_output_safety(config, propagation_reader, safety_reader)
                .unwrap();
        graph
            .set_source(0, &physical, SceneCalibration::default())
            .unwrap();
        graph
            .set_source(1, &creative, SceneCalibration::default())
            .unwrap();
        propagation_writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });

        let samples = [[0.01_f32; 128]; 2];
        let blocks = [
            SourceBlock {
                source_index: 0,
                decoded_mono: &samples[0],
            },
            SourceBlock {
                source_index: 1,
                decoded_mono: &samples[1],
            },
        ];
        let mut left = [0.0; 128];
        let mut right = [0.0; 128];
        graph
            .process_block(ProcessBlock {
                now_ns: 0,
                sources: &blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();

        assert!(graph.sources[0].applied_safety_gain < 1.0);
        assert_eq!(graph.sources[1].applied_safety_gain, 1.0);
        assert_eq!(graph.telemetry().safety.proximity_ceiling_engagements, 1);

        safety_control.set_monitor_gain_db(-6.0).unwrap();
        graph
            .process_block(ProcessBlock {
                now_ns: 1,
                sources: &blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        let target = 10.0_f32.powf(-6.0 / 20.0);
        assert!(graph.applied_monitor_gain > target);
        assert!(graph.applied_monitor_gain < 1.0);
    }

    #[test]
    fn final_limiter_reports_pre_and_post_peaks() {
        let (mut writer, mut graph) = test_graph(128);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let input = [2.0_f32; 128];
        let blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0; 128];
        let mut right = [0.0; 128];
        graph
            .process_block(ProcessBlock {
                now_ns: 0,
                sources: &blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();

        let telemetry = graph.safety_telemetry();
        let ceiling = 10.0_f32.powf(crate::TRUE_PEAK_LIMITER_CEILING_DBTP / 20.0);
        assert!(telemetry.limiter_engagements > 0);
        assert!(telemetry.pre_limiter_peak >= 2.0);
        assert!(telemetry.post_limiter_peak <= ceiling + 1.0e-6);
    }

    #[test]
    fn raw_monitor_stem_reaches_both_output_channels_bit_exact_at_the_fixed_pad() {
        let (mut writer, mut graph) = test_graph(128);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 12.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 0.75,
                right_gain: -0.25,
            }),
        });
        let (mut route_control, route_reader) = MonitorRoutePublication::new();
        route_control.select_raw_source(0).unwrap();
        graph.set_monitor_route_reader(route_reader);

        let input = std::array::from_fn::<_, 128, _>(|index| index as f32 * 0.001);
        let source_blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0_f32; 128];
        let mut right = [0.0_f32; 128];
        let allocations = count_allocations(|| {
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &source_blocks,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        });

        assert_eq!(allocations, 0);
        for frame in 0..TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES {
            assert_eq!(left[frame].to_bits(), 0.0_f32.to_bits());
            assert_eq!(right[frame].to_bits(), 0.0_f32.to_bits());
        }
        for frame in TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES..128 {
            let expected =
                input[frame - TRUE_PEAK_LIMITER_LOOKAHEAD_SAMPLES] * RAW_MONITOR_PAD_GAIN;
            assert_eq!(left[frame].to_bits(), expected.to_bits());
            assert_eq!(right[frame].to_bits(), expected.to_bits());
        }
    }

    #[test]
    fn hot_raw_monitor_stem_is_contained_by_the_existing_final_limiter() {
        let (_writer, mut graph) = test_graph(128);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let (mut route_control, route_reader) = MonitorRoutePublication::new();
        route_control.select_raw_source(0).unwrap();
        graph.set_monitor_route_reader(route_reader);
        let input = [100.0_f32; 128];
        let source_blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0_f32; 128];
        let mut right = [0.0_f32; 128];

        graph
            .process_block(ProcessBlock {
                now_ns: 0,
                sources: &source_blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();

        let telemetry = graph.safety_telemetry();
        let ceiling = 10.0_f32.powf(crate::TRUE_PEAK_LIMITER_CEILING_DBTP / 20.0);
        assert!(telemetry.limiter_engagements > 0);
        assert!(telemetry.pre_limiter_peak > 1.0);
        assert!(telemetry.post_limiter_peak <= ceiling + 1.0e-6);
    }

    #[test]
    fn explicit_spatial_monitor_route_is_bit_identical_to_the_existing_processed_path() {
        fn configured_graph() -> (crate::SnapshotWriter<PropagationSnapshot>, RuntimeGraph) {
            let (mut writer, mut graph) = test_graph(64);
            graph
                .set_source(
                    0,
                    &source_profile(ReferenceLevel::CreativeDb { db: -6.0 }),
                    SceneCalibration::default(),
                )
                .unwrap();
            writer.publish(PropagationSnapshot {
                sequence: 1,
                simulated_at_ns: 0,
                sources: std::array::from_fn(|index| SourcePropagation {
                    active: index == 0,
                    target_delay_samples: 7.25,
                    left_gain: 0.75,
                    right_gain: 0.25,
                }),
            });
            (writer, graph)
        }

        let (_reference_writer, mut reference) = configured_graph();
        let (_routed_writer, mut routed) = configured_graph();
        let (mut route_control, route_reader) = MonitorRoutePublication::new();
        route_control.select_spatial();
        routed.set_monitor_route_reader(route_reader);
        let input = std::array::from_fn::<_, 64, _>(|index| ((index as f32 * 0.071).sin()) * 0.01);
        let blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut reference_left = [0.0_f32; 64];
        let mut reference_right = [0.0_f32; 64];
        let mut routed_left = [0.0_f32; 64];
        let mut routed_right = [0.0_f32; 64];

        for now_ns in [0, 2_666_667, 5_333_334] {
            reference
                .process_block(ProcessBlock {
                    now_ns,
                    sources: &blocks,
                    output_left: &mut reference_left,
                    output_right: &mut reference_right,
                })
                .unwrap();
            routed
                .process_block(ProcessBlock {
                    now_ns,
                    sources: &blocks,
                    output_left: &mut routed_left,
                    output_right: &mut routed_right,
                })
                .unwrap();
            assert_eq!(
                reference_left.map(f32::to_bits),
                routed_left.map(f32::to_bits)
            );
            assert_eq!(
                reference_right.map(f32::to_bits),
                routed_right.map(f32::to_bits)
            );
        }
    }

    #[test]
    fn shaped_spectral_render_path_allocates_nothing_after_construction_and_warmup() {
        let (mut writer, mut graph) = test_graph(128);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        graph
            .set_source_spectral_transfer_smoothed(
                0,
                SpectralTransfer::default()
                    .with_stage(
                        fightbox_api::spectral::SpectralStage::Enclosure,
                        [0.0, -1.0, -2.0, -3.0, -5.0, -8.0, -12.0, -18.0],
                    )
                    .unwrap(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 12.25,
                left_gain: 0.75,
                right_gain: 0.25,
            }),
        });
        let input = [0.25; 128];
        let source_blocks = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [0.0; 128];
        let mut right = [0.0; 128];

        graph
            .process_block(ProcessBlock {
                now_ns: 1,
                sources: &source_blocks,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();

        let allocations = count_allocations(|| {
            for block_index in 0..MAX_TIMING_RECORDS {
                graph
                    .process_block(ProcessBlock {
                        now_ns: block_index as u64 * 2_666_667,
                        sources: &source_blocks,
                        output_left: &mut left,
                        output_right: &mut right,
                    })
                    .unwrap();
            }
        });
        assert_eq!(allocations, 0);
        assert_eq!(graph.telemetry().timings.len(), MAX_TIMING_RECORDS);
        assert!(graph.telemetry().faults.snapshot_stale > 0);
    }

    fn render_one_source_block(
        graph: &mut RuntimeGraph,
        input: &[f32],
        now_ns: u64,
    ) -> (Vec<f32>, Vec<f32>) {
        let sources = [SourceBlock {
            source_index: 0,
            decoded_mono: input,
        }];
        let mut left = vec![0.0; input.len()];
        let mut right = vec![0.0; input.len()];
        graph
            .process_block(ProcessBlock {
                now_ns,
                sources: &sources,
                output_left: &mut left,
                output_right: &mut right,
            })
            .unwrap();
        (left, right)
    }

    #[test]
    fn block_ramps_land_exactly_on_delay_and_slewed_gain_targets() {
        let (mut writer, mut graph) = test_graph(128);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let snapshot = |sequence, delay, gain| PropagationSnapshot {
            sequence,
            simulated_at_ns: sequence,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: delay,
                left_gain: gain,
                right_gain: gain * 0.5,
            }),
        };
        writer.publish(snapshot(1, 8.0, 1.0));
        let input = [0.25; 128];
        render_one_source_block(&mut graph, &input, 1);

        writer.publish(snapshot(2, 31.75, 0.2));
        render_one_source_block(&mut graph, &input, 2);
        let source = &graph.sources[0];
        let expected_left = 0.2 + (1.0 - 0.2) * graph.snapshot_gain_block_retention;
        let expected_right = 0.1 + (0.5 - 0.1) * graph.snapshot_gain_block_retention;

        assert_eq!(source.applied_delay_samples.to_bits(), 31.75_f32.to_bits());
        assert_eq!(
            source.delay.current_delay_samples().to_bits(),
            31.75_f32.to_bits()
        );
        assert_eq!(source.applied_left_gain.to_bits(), expected_left.to_bits());
        assert_eq!(
            source.applied_right_gain.to_bits(),
            expected_right.to_bits()
        );
    }

    #[test]
    fn snapshot_swap_on_a_steady_sine_has_a_bounded_sample_delta() {
        const BLOCK_SIZE: usize = 256;
        let (mut writer, mut graph) = test_graph(BLOCK_SIZE as u32);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let snapshot = |sequence, delay, gain| PropagationSnapshot {
            sequence,
            simulated_at_ns: sequence,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: delay,
                left_gain: gain,
                right_gain: gain,
            }),
        };
        writer.publish(snapshot(1, 24.25, 1.0));

        let radians_per_sample = std::f32::consts::TAU * 220.0 / 48_000.0;
        let mut phase_frame = 0_usize;
        let mut previous = 0.0_f32;
        let mut maximum_delta = 0.0_f32;
        for block_index in 0..12 {
            if block_index == 6 {
                writer.publish(snapshot(2, 12.25, 0.05));
            }
            let input: [f32; BLOCK_SIZE] = std::array::from_fn(|frame| {
                ((phase_frame + frame) as f32 * radians_per_sample).sin()
            });
            phase_frame += BLOCK_SIZE;
            let (left, _) = render_one_source_block(&mut graph, &input, block_index as u64);
            if block_index >= 4 {
                for sample in left {
                    maximum_delta = maximum_delta.max((sample - previous).abs());
                    previous = sample;
                }
            } else {
                previous = *left.last().unwrap();
            }
        }

        assert!(
            maximum_delta < 0.032,
            "80 ms snapshot slew allowed a {maximum_delta} sample delta"
        );
    }

    fn deterministic_render_bytes() -> Vec<u8> {
        const BLOCK_SIZE: usize = 64;
        let (mut writer, mut graph) = test_graph(BLOCK_SIZE as u32);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: -3.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let mut bytes = Vec::with_capacity(12 * BLOCK_SIZE * 2 * std::mem::size_of::<f32>());
        for block_index in 0..12_u64 {
            writer.publish(PropagationSnapshot {
                sequence: block_index + 1,
                simulated_at_ns: block_index,
                sources: std::array::from_fn(|index| SourcePropagation {
                    active: index == 0,
                    target_delay_samples: 7.125 + block_index as f32 * 0.375,
                    left_gain: if block_index < 5 { 0.9 } else { 0.23 },
                    right_gain: if block_index < 8 { 0.4 } else { 0.81 },
                }),
            });
            let input: [f32; BLOCK_SIZE] = std::array::from_fn(|frame| {
                (((block_index as usize * BLOCK_SIZE + frame) as f32) * 0.037).sin()
            });
            let (left, right) = render_one_source_block(&mut graph, &input, block_index);
            for sample in left.into_iter().chain(right) {
                bytes.extend_from_slice(&sample.to_bits().to_le_bytes());
            }
        }
        bytes
    }

    #[test]
    fn smoothing_render_is_byte_identical_on_repeated_construction() {
        assert_eq!(deterministic_render_bytes(), deterministic_render_bytes());
    }

    const DETERMINISM_CHILD_ENV: &str = "FIGHTBOX_RUNTIME_DETERMINISM_CHILD";
    const DETERMINISM_MARKER: &str = "FIGHTBOX_RENDER_BYTES=";

    #[test]
    fn deterministic_render_child_payload() {
        if std::env::var_os(DETERMINISM_CHILD_ENV).is_none() {
            return;
        }
        use std::fmt::Write as _;
        let bytes = deterministic_render_bytes();
        let mut encoded = String::with_capacity(DETERMINISM_MARKER.len() + bytes.len() * 2);
        encoded.push_str(DETERMINISM_MARKER);
        for byte in bytes {
            write!(encoded, "{byte:02x}").unwrap();
        }
        println!("{encoded}");
    }

    fn child_render_payload() -> String {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "render::tests::deterministic_render_child_payload",
                "--nocapture",
            ])
            .env(DETERMINISM_CHILD_ENV, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "determinism child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix(DETERMINISM_MARKER))
            .expect("determinism child emitted render bytes")
            .to_owned()
    }

    #[test]
    fn smoothing_render_is_byte_identical_across_processes() {
        assert_eq!(child_render_payload(), child_render_payload());
    }

    #[test]
    fn deadline_fault_placeholder_is_explicitly_recordable() {
        let (_, mut graph) = test_graph(16);
        graph.record_deadline_miss();
        assert_eq!(graph.telemetry().faults.deadline_miss, 1);
    }

    #[test]
    fn run_timing_histogram_bucket_edges_are_monotonic() {
        let mut previous = 0;
        for index in 0..RUN_TIMING_HISTOGRAM_REGULAR_BUCKETS {
            let edge = run_timing_bucket_upper_bound_ns(index);
            assert!(edge > previous);
            previous = edge;
        }
        assert_eq!(previous, RUN_TIMING_HISTOGRAM_MAX_REGULAR_NS);
    }

    #[test]
    fn run_timing_histogram_percentiles_are_conservative() {
        let samples = [1_234_u64, 12_345, 123_456, 1_234_567, 12_345_678];
        let mut histogram = RunTimingHistogram::default();
        for sample in samples {
            for _ in 0..20 {
                histogram.record(sample);
            }
        }

        assert_eq!(histogram.len(), 100);
        assert_eq!(histogram.min_ns(), Some(samples[0]));
        assert_eq!(histogram.max_ns(), Some(samples[4]));
        assert!(histogram.percentile_ns(50.0).unwrap() >= samples[2]);
        assert!(histogram.percentile_ns(95.0).unwrap() >= samples[4]);

        let mut single_bucket = RunTimingHistogram::default();
        single_bucket.record(12_345);
        assert!(single_bucket.percentile_ns(50.0).unwrap() > 12_345);
    }

    #[test]
    fn callback_timing_shared_payload_is_exactly_4096_slots_plus_cursor() {
        assert_eq!(MAX_TIMING_RECORDS, 4_096);
        let atomic_bytes = core::mem::size_of::<AtomicU64>() as u64;
        let expected = 4_096_u64
            .saturating_mul(2_u64.saturating_mul(atomic_bytes))
            .saturating_add(atomic_bytes);

        assert_eq!(expected, 65_544);
        assert_eq!(CallbackTimingPublication::shared_payload_bytes(), expected);
    }

    #[test]
    fn callback_timing_publication_drains_each_observation_once() {
        let (writer, mut reader) = CallbackTimingPublication::new();
        for duration_ns in [101, 202, 303, 404] {
            writer.record(duration_ns);
        }

        let mut observed = Vec::new();
        assert_eq!(reader.drain(|duration_ns| observed.push(duration_ns)), 4);
        assert_eq!(observed, [101, 202, 303, 404]);
        assert_eq!(reader.drain(|duration_ns| observed.push(duration_ns)), 0);
        assert_eq!(reader.dropped_observations(), 0);

        writer.record(505);
        assert_eq!(reader.drain(|duration_ns| observed.push(duration_ns)), 1);
        assert_eq!(observed, [101, 202, 303, 404, 505]);
    }

    #[test]
    fn callback_timing_drain_detects_slots_overwritten_during_observation() {
        let (writer, mut reader) = CallbackTimingPublication::new();
        for duration_ns in 1..=MAX_TIMING_RECORDS as u64 {
            writer.record(duration_ns);
        }

        let replacement_start = 10_000_u64;
        let mut observed = Vec::new();
        let delivered = reader.drain(|duration_ns| {
            observed.push(duration_ns);
            if observed.len() == 1 {
                for offset in 0..MAX_TIMING_RECORDS as u64 {
                    writer.record(replacement_start + offset);
                }
            }
        });

        // The callback above deterministically wraps the producer across every
        // unread slot. A replacement duration must never be delivered under an
        // old sequence number.
        assert_eq!(delivered, 1);
        assert_eq!(observed, [1]);
        assert_eq!(reader.dropped_observations(), MAX_TIMING_RECORDS as u64 - 1);

        assert_eq!(
            reader.drain(|duration_ns| observed.push(duration_ns)),
            MAX_TIMING_RECORDS
        );
        assert_eq!(observed.len(), MAX_TIMING_RECORDS + 1);
        assert_eq!(observed[1], replacement_start);
        assert_eq!(
            observed[MAX_TIMING_RECORDS],
            replacement_start + MAX_TIMING_RECORDS as u64 - 1
        );
    }

    #[test]
    fn callback_timing_writer_allocates_nothing_after_construction() {
        let (writer, mut reader) = CallbackTimingPublication::new();
        let allocations = count_allocations(|| {
            for duration_ns in 1..=MAX_TIMING_RECORDS as u64 * 2 {
                writer.record(duration_ns);
            }
        });
        assert_eq!(allocations, 0);

        let mut delivered = 0;
        assert_eq!(
            reader.drain(|duration_ns| {
                assert!(duration_ns > 0);
                delivered += 1;
            }),
            MAX_TIMING_RECORDS
        );
        assert_eq!(delivered, MAX_TIMING_RECORDS);
        assert_eq!(reader.dropped_observations(), MAX_TIMING_RECORDS as u64);
    }

    #[derive(Clone, Copy)]
    enum SpatialMockMode {
        Valid,
        Fail,
        PropagationSequenceMismatch,
        MalformedMetadata,
        NonFinitePresentation,
        NonFiniteEnvironment,
    }

    struct PassthroughSpatialBackend {
        mode: SpatialMockMode,
        generation: u64,
        prepared_for_realtime: bool,
        call_count: Option<Arc<AtomicU64>>,
        last_source_mask: Option<Arc<AtomicU64>>,
        last_propagation_sequence: Option<Arc<AtomicU64>>,
    }

    impl SpatialBackendRenderGraph for PassthroughSpatialBackend {
        fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
            // The deterministic test backend has no vendor workspace or lazy
            // state; explicitly opting in keeps the production contract strict.
            self.prepared_for_realtime = true;
            Ok(())
        }

        fn render_spatial_block(
            &mut self,
            block: SpatialPropagationRenderBlock<'_>,
        ) -> Result<(), SpatialBackendRenderError> {
            if !self.prepared_for_realtime {
                return Err(SpatialBackendRenderError::InactiveGraph);
            }
            if let Some(call_count) = &self.call_count {
                call_count.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(last_source_mask) = &self.last_source_mask {
                let source_mask = block
                    .sources
                    .iter()
                    .fold(0_u64, |mask, source| mask | (1_u64 << source.source_index));
                last_source_mask.store(source_mask, Ordering::Relaxed);
            }
            if let Some(last_propagation_sequence) = &self.last_propagation_sequence {
                last_propagation_sequence.store(block.propagation_sequence, Ordering::Relaxed);
            }
            // Deliberately dirty the complete banks. Runtime must retain only
            // fixed valid presentation slots and the active ACN prefix.
            block.presentation_bank.fill(91.0);
            block.environmental_bank.fill(73.0);
            if matches!(self.mode, SpatialMockMode::Fail) {
                return Err(SpatialBackendRenderError::InactiveGraph);
            }
            if matches!(self.mode, SpatialMockMode::PropagationSequenceMismatch) {
                return Err(SpatialBackendRenderError::PropagationSequenceMismatch);
            }

            let block_size = block.metadata.block_size_frames as usize;
            let mut active_feeds = 0;
            for source in block.sources {
                if !(1..=MAX_SPATIAL_PROGRAM_PLANES).contains(&source.program_plane_count) {
                    return Err(SpatialBackendRenderError::InvalidProgramPlaneCount);
                }
                for program_plane in 0..source.program_plane_count {
                    let component = match program_plane {
                        0 => SpatialPresentationComponent::DirectCenter,
                        1 => SpatialPresentationComponent::WidthPositive,
                        _ => unreachable!(),
                    };
                    let component_slot = component.presentation_slot().unwrap();
                    let output_plane = source.source_index
                        * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE
                        + component_slot;
                    let output_start = output_plane * block_size;
                    block.presentation_bank[output_start..output_start + block_size]
                        .copy_from_slice(source.program_planes[program_plane]);
                    block.metadata.presentation_feeds[output_plane] =
                        crate::backend::SpatialPresentationFeedMetadata {
                            valid: true,
                            source_index: source.source_index,
                            component,
                            placement: SpatialFeedPlacement::Pose,
                            pose_enu: Pose {
                                position: EnuVector3::new(source.source_index as f32, 0.0, 0.0),
                                forward: EnuVector3::new(0.0, 1.0, 0.0),
                                up: EnuVector3::new(0.0, 0.0, 1.0),
                            },
                            direction_enu: EnuVector3::default(),
                            latency_frames: program_plane as u32,
                        };
                    active_feeds += 1;
                }
            }

            block.metadata.validity = SpatialOutputValidity::Valid;
            block.metadata.generation = self.generation;
            block.metadata.active_presentation_feed_count =
                if matches!(self.mode, SpatialMockMode::MalformedMetadata) {
                    MAX_SPATIAL_PRESENTATION_FEEDS + 1
                } else {
                    active_feeds
                };
            block.metadata.active_environmental_order = SpatialAmbisonicOrder::One;
            block.metadata.active_environmental_plane_count = 4;
            block.metadata.environmental_latency_frames = 7;
            block.metadata.environmental_basis =
                SpatialEnvironmentalBasis::RightHandedXRightYUpZBack;
            if matches!(self.mode, SpatialMockMode::NonFinitePresentation) {
                block.presentation_bank[0] = f32::NAN;
            }
            if matches!(self.mode, SpatialMockMode::NonFiniteEnvironment) {
                block.environmental_bank[0] = f32::INFINITY;
            }
            Ok(())
        }
    }

    fn unprepared_spatial_test_graph(
        block_size: u32,
        mode: SpatialMockMode,
    ) -> (crate::SnapshotWriter<PropagationSnapshot>, RuntimeGraph) {
        let (mut writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let config = EngineConfig {
            block_size_frames: block_size,
            max_active_sources: 2,
            ..EngineConfig::default()
        };
        let graph = RuntimeGraph::new_with_spatial_backend(
            config,
            reader,
            &[1, 2],
            Box::new(PassthroughSpatialBackend {
                mode,
                generation: 11,
                prepared_for_realtime: false,
                call_count: None,
                last_source_mask: None,
                last_propagation_sequence: None,
            }),
        )
        .unwrap();
        (writer, graph)
    }

    fn prepared_spatial_test_graph(
        block_size: u32,
        mode: SpatialMockMode,
    ) -> (crate::SnapshotWriter<PropagationSnapshot>, RuntimeGraph) {
        let (writer, mut graph) = unprepared_spatial_test_graph(block_size, mode);
        graph.prepare_spatial_backend_for_realtime().unwrap();
        (writer, graph)
    }

    #[test]
    fn spatial_test_backend_fails_closed_until_explicitly_prepared() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, mut graph) =
            unprepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let input = [0.25_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);

        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        assert_eq!(
            metadata.validity,
            SpatialOutputValidity::SilentDiscontinuity
        );
        assert_eq!(metadata.discontinuity_sequence, 1);
        assert_eq!(graph.fault_counters().backend_render_error, 1);
        assert!(presentation.iter().all(|sample| *sample == 0.0));
        assert!(environment.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn successful_spatial_prepare_does_not_advance_runtime_state_or_timing() {
        let (_writer, mut graph) = unprepared_spatial_test_graph(4, SpatialMockMode::Valid);
        let memory_before = graph.persistent_memory();
        let faults_before = graph.fault_counters();
        let generation_before = graph.spatial_generation;
        let discontinuity_before = graph.spatial_discontinuity_sequence;
        let timing_observations_before = graph.telemetry().timings.len();

        graph.prepare_spatial_backend_for_realtime().unwrap();

        assert_eq!(graph.persistent_memory(), memory_before);
        assert_eq!(graph.fault_counters(), faults_before);
        assert_eq!(graph.spatial_generation, generation_before);
        assert_eq!(graph.spatial_discontinuity_sequence, discontinuity_before);
        assert_eq!(graph.telemetry().timings.len(), timing_observations_before);
    }

    fn spatial_banks(block_size: usize) -> (Vec<f32>, Vec<f32>, SpatialOutputMetadata) {
        (
            vec![0.0; MAX_SPATIAL_PRESENTATION_FEEDS * block_size],
            vec![0.0; MAX_SPATIAL_ENVIRONMENT_PLANES * block_size],
            SpatialOutputMetadata::default(),
        )
    }

    struct PreparationProbeBackend {
        calls: Arc<AtomicU64>,
        result: Result<(), SpatialBackendRenderError>,
    }

    impl SpatialBackendRenderGraph for PreparationProbeBackend {
        fn prepare_for_realtime(&mut self) -> Result<(), SpatialBackendRenderError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.result
        }

        fn render_spatial_block(
            &mut self,
            _block: SpatialPropagationRenderBlock<'_>,
        ) -> Result<(), SpatialBackendRenderError> {
            unreachable!("preparation forwarding test never renders")
        }
    }

    #[test]
    fn spatial_prepare_forwards_exact_backend_result_without_processing_a_block() {
        let (_writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let calls = Arc::new(AtomicU64::new(0));
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            EngineConfig {
                block_size_frames: 4,
                max_active_sources: 1,
                ..EngineConfig::default()
            },
            reader,
            &[1],
            Box::new(PreparationProbeBackend {
                calls: Arc::clone(&calls),
                result: Err(SpatialBackendRenderError::InvalidOutputMetadata),
            }),
        )
        .unwrap();
        let memory_before = graph.persistent_memory();
        let faults_before = graph.fault_counters();

        assert_eq!(
            graph.prepare_spatial_backend_for_realtime(),
            Err(SpatialBackendRenderError::InvalidOutputMetadata)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(graph.persistent_memory(), memory_before);
        assert_eq!(graph.fault_counters(), faults_before);

        let (_writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let mut graph = RuntimeGraph::new(
            EngineConfig {
                block_size_frames: 4,
                max_active_sources: 1,
                ..EngineConfig::default()
            },
            reader,
        )
        .unwrap();
        assert_eq!(
            graph.prepare_spatial_backend_for_realtime(),
            Err(SpatialBackendRenderError::InactiveGraph)
        );
    }

    fn presentation_plane(
        bank: &[f32],
        source_index: usize,
        component: SpatialPresentationComponent,
        block_size: usize,
    ) -> &[f32] {
        let plane_index = source_index * MAX_SPATIAL_PRESENTATION_FEEDS_PER_SOURCE
            + component.presentation_slot().unwrap();
        let start = plane_index * block_size;
        &bank[start..start + block_size]
    }

    #[test]
    fn spatial_construction_requires_one_immutable_valid_shape_per_source() {
        let build = |program_plane_counts: &[usize]| {
            let (_writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
            RuntimeGraph::new_with_spatial_backend(
                EngineConfig {
                    block_size_frames: 4,
                    max_active_sources: 2,
                    ..EngineConfig::default()
                },
                reader,
                program_plane_counts,
                Box::new(PassthroughSpatialBackend {
                    mode: SpatialMockMode::Valid,
                    generation: 1,
                    prepared_for_realtime: false,
                    call_count: None,
                    last_source_mask: None,
                    last_propagation_sequence: None,
                }),
            )
        };

        assert!(matches!(
            build(&[1]),
            Err(RenderError::SpatialProgramShapeCountMismatch {
                configured_source_count: 2,
                supplied_shape_count: 1,
            })
        ));
        assert!(matches!(
            build(&[1, 2, 1]),
            Err(RenderError::SpatialProgramShapeCountMismatch {
                configured_source_count: 2,
                supplied_shape_count: 3,
            })
        ));
        assert!(matches!(
            build(&[0, 2]),
            Err(RenderError::InvalidSpatialProgramPlaneCount {
                source_index: 0,
                supplied_plane_count: 0,
            })
        ));
        assert!(matches!(
            build(&[1, 3]),
            Err(RenderError::InvalidSpatialProgramPlaneCount {
                source_index: 1,
                supplied_plane_count: 3,
            })
        ));

        let graph = build(&[1, 2]).unwrap();
        let mut expected = [0; MAX_ACTIVE_SOURCES];
        expected[..2].copy_from_slice(&[1, 2]);
        assert_eq!(graph.spatial_program_plane_counts, Some(expected));
    }

    #[test]
    fn spatial_active_set_is_complete_atomic_and_snapshot_coherent() {
        const BLOCK_SIZE: usize = 4;
        let (mut writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let call_count = Arc::new(AtomicU64::new(0));
        let last_source_mask = Arc::new(AtomicU64::new(0));
        let last_propagation_sequence = Arc::new(AtomicU64::new(u64::MAX));
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            EngineConfig {
                block_size_frames: BLOCK_SIZE as u32,
                max_active_sources: 2,
                ..EngineConfig::default()
            },
            reader,
            &[1, 2],
            Box::new(PassthroughSpatialBackend {
                mode: SpatialMockMode::Valid,
                generation: 7,
                prepared_for_realtime: false,
                call_count: Some(Arc::clone(&call_count)),
                last_source_mask: Some(Arc::clone(&last_source_mask)),
                last_propagation_sequence: Some(Arc::clone(&last_propagation_sequence)),
            }),
        )
        .unwrap();
        let profile = source_profile(ReferenceLevel::CreativeDb { db: 0.0 });
        for source_index in 0..2 {
            graph
                .set_source(source_index, &profile, SceneCalibration::default())
                .unwrap();
        }
        graph.prepare_spatial_backend_for_realtime().unwrap();

        let zero = [0.0_f32; BLOCK_SIZE];
        let source_zero = SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&zero, &[]],
        };
        let source_one = SpatialProgramBlock {
            source_index: 1,
            program_plane_count: 2,
            program_planes: [&zero, &zero],
        };
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);

        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &[source_zero],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert_eq!(last_source_mask.load(Ordering::Relaxed), 0b01);
        assert_eq!(last_propagation_sequence.load(Ordering::Relaxed), 1);

        writer.publish(PropagationSnapshot {
            sequence: 2,
            simulated_at_ns: 1,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < 2,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        graph.sources[1].safety_gain_initialized = true;
        graph.sources[1].applied_safety_gain = 0.25;
        presentation.fill(5.0);
        environment.fill(6.0);
        metadata = SpatialOutputMetadata {
            block_start_frame: 99,
            generation: 123,
            ..SpatialOutputMetadata::default()
        };
        let rejected_metadata = metadata;
        let rejected_safety_gain = graph.sources[1].applied_safety_gain;
        let rejected_safety_initialized = graph.sources[1].safety_gain_initialized;
        let rejected_faults = graph.fault_counters();
        let rejected_generation = graph.spatial_generation;
        let rejected_discontinuity = graph.spatial_discontinuity_sequence;

        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 1,
                block_start_frame: BLOCK_SIZE as u64,
                sources: &[source_zero],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::MissingActiveProgram { source_index: 1 })
        );
        assert!(presentation.iter().all(|sample| *sample == 5.0));
        assert!(environment.iter().all(|sample| *sample == 6.0));
        assert_eq!(metadata, rejected_metadata);
        assert_eq!(call_count.load(Ordering::Relaxed), 1);
        assert_eq!(last_source_mask.load(Ordering::Relaxed), 0b01);
        assert_eq!(last_propagation_sequence.load(Ordering::Relaxed), 1);
        assert_eq!(graph.sources[1].applied_safety_gain, rejected_safety_gain);
        assert_eq!(
            graph.sources[1].safety_gain_initialized,
            rejected_safety_initialized
        );
        assert_eq!(graph.fault_counters(), rejected_faults);
        assert_eq!(graph.spatial_generation, rejected_generation);
        assert_eq!(graph.spatial_discontinuity_sequence, rejected_discontinuity);

        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 2,
                block_start_frame: BLOCK_SIZE as u64,
                sources: &[source_zero, source_one],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();
        assert_eq!(call_count.load(Ordering::Relaxed), 2);
        assert_eq!(last_source_mask.load(Ordering::Relaxed), 0b11);
        assert_eq!(last_propagation_sequence.load(Ordering::Relaxed), 2);
        assert!(graph.sources[1].applied_safety_gain > rejected_safety_gain);
        assert!(graph.sources[1].applied_safety_gain < 1.0);

        graph.sources[0].delay_initialized = true;
        graph.sources[0].snapshot_gain_initialized = true;
        graph.sources[0].safety_gain_initialized = true;
        writer.publish(PropagationSnapshot {
            sequence: 3,
            simulated_at_ns: 2,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 1,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 3,
                block_start_frame: 2 * BLOCK_SIZE as u64,
                sources: &[source_one],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();
        assert_eq!(call_count.load(Ordering::Relaxed), 3);
        assert_eq!(last_source_mask.load(Ordering::Relaxed), 0b10);
        assert_eq!(last_propagation_sequence.load(Ordering::Relaxed), 3);
        assert!(!graph.sources[0].delay_initialized);
        assert!(!graph.sources[0].snapshot_gain_initialized);
        assert!(!graph.sources[0].safety_gain_initialized);
    }

    #[test]
    fn spatial_active_source_requires_a_configured_drive_without_mutation() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        let input = [0.25_f32; BLOCK_SIZE];
        let source = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let mut presentation = vec![5.0; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environment = vec![6.0; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut metadata = SpatialOutputMetadata {
            block_start_frame: 99,
            ..SpatialOutputMetadata::default()
        };
        let original_metadata = metadata;

        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 128,
                sources: &source,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::ActiveSourceNotConfigured { source_index: 0 })
        );
        assert!(presentation.iter().all(|sample| *sample == 5.0));
        assert!(environment.iter().all(|sample| *sample == 6.0));
        assert_eq!(metadata, original_metadata);
        assert_eq!(graph.spatial_generation, 0);
        assert_eq!(graph.spatial_discontinuity_sequence, 0);
        assert_eq!(graph.fault_counters(), FaultCounters::default());
    }

    #[test]
    fn spatial_mono_and_stereo_preserve_program_order_and_one_identical_safety_scalar() {
        const BLOCK_SIZE: usize = 8;
        let (mut propagation_writer, propagation_reader) =
            SnapshotPublication::new(PropagationSnapshot::default());
        let (mut safety_control, safety_reader) =
            OutputSafetyPublication::new(OutputSafetyConfig {
                monitor_gain_db: 24.0,
                ..OutputSafetyConfig::default()
            })
            .unwrap();
        let config = EngineConfig {
            block_size_frames: BLOCK_SIZE as u32,
            max_active_sources: 2,
            ..EngineConfig::default()
        };
        let mut graph = RuntimeGraph::new_with_spatial_backend_and_output_safety(
            config,
            propagation_reader,
            safety_reader,
            &[1, 2],
            Box::new(PassthroughSpatialBackend {
                mode: SpatialMockMode::Valid,
                generation: 3,
                prepared_for_realtime: false,
                call_count: None,
                last_source_mask: None,
                last_propagation_sequence: None,
            }),
        )
        .unwrap();
        let profile = source_profile(ReferenceLevel::SplAtOneMeter { db_spl: 120.0 });
        let drive = graph
            .set_source(0, &profile, SceneCalibration::default())
            .unwrap();
        graph
            .set_source(1, &profile, SceneCalibration::default())
            .unwrap();
        propagation_writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < 2,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        graph.prepare_spatial_backend_for_realtime().unwrap();

        let mono = [0.03125_f32; BLOCK_SIZE];
        let left = std::array::from_fn::<_, BLOCK_SIZE, _>(|frame| 0.02 + frame as f32 * 0.001);
        let right = std::array::from_fn::<_, BLOCK_SIZE, _>(|frame| -0.04 - frame as f32 * 0.002);
        let sources = [
            SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&mono, &[]],
            },
            SpatialProgramBlock {
                source_index: 1,
                program_plane_count: 2,
                program_planes: [&left, &right],
            },
        ];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);

        // Warm the shared source-safety state at unity.
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();
        safety_control.set_source(0, &profile, None).unwrap();
        safety_control.set_source(1, &profile, None).unwrap();
        safety_control
            .set_listener_position(EnuVector3::new(0.01, 0.0, 0.0))
            .unwrap();

        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 1,
                block_start_frame: BLOCK_SIZE as u64,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        let mono_output = presentation_plane(
            &presentation,
            0,
            SpatialPresentationComponent::DirectCenter,
            BLOCK_SIZE,
        );
        let left_output = presentation_plane(
            &presentation,
            1,
            SpatialPresentationComponent::DirectCenter,
            BLOCK_SIZE,
        );
        let right_output = presentation_plane(
            &presentation,
            1,
            SpatialPresentationComponent::WidthPositive,
            BLOCK_SIZE,
        );
        for frame in 0..BLOCK_SIZE {
            let mono_scalar = mono_output[frame] / mono[frame];
            let left_scalar = left_output[frame] / left[frame];
            let right_scalar = right_output[frame] / right[frame];
            assert!((mono_scalar - left_scalar).abs() < 2.0e-6);
            assert!((left_scalar - right_scalar).abs() < 2.0e-6);
            assert!(left_scalar <= drive.linear_gain() + 1.0e-6);
        }
        assert!(left_output[0] / left[0] > left_output[BLOCK_SIZE - 1] / left[BLOCK_SIZE - 1]);
        assert_eq!(metadata.active_presentation_feed_count, 3);
        assert_eq!(metadata.block_start_frame, BLOCK_SIZE as u64);
        assert!(metadata.world_space_unrotated);
        assert!(metadata.source_drive_applied);
        assert!(metadata.source_safety_gain_applied);
        assert!(!metadata.monitor_gain_applied);
        assert!(!metadata.final_hrtf_applied);
        assert!(!metadata.output_limiter_applied);
    }

    #[test]
    fn spatial_inactive_fixed_slots_and_environment_tail_are_zero_and_default() {
        const BLOCK_SIZE: usize = 4;
        let (mut writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        writer.publish(PropagationSnapshot {
            sequence: 2,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 1,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        graph
            .set_source(
                1,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        let left = [0.1_f32; BLOCK_SIZE];
        let right = [-0.2_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 1,
            program_plane_count: 2,
            program_planes: [&left, &right],
        }];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 44,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        assert_eq!(metadata.validity, SpatialOutputValidity::Valid);
        assert_eq!(metadata.active_presentation_feed_count, 2);
        assert_eq!(
            metadata.active_environmental_order,
            SpatialAmbisonicOrder::One
        );
        assert_eq!(metadata.active_environmental_plane_count, 4);
        assert_eq!(
            metadata.environmental_channel_order,
            SpatialAmbisonicChannelOrder::Acn
        );
        assert_eq!(
            metadata.environmental_normalization,
            SpatialAmbisonicNormalization::N3d
        );
        assert_eq!(
            metadata.environmental_basis,
            SpatialEnvironmentalBasis::RightHandedXRightYUpZBack
        );
        for plane_index in 0..MAX_SPATIAL_PRESENTATION_FEEDS {
            let start = plane_index * BLOCK_SIZE;
            let is_active = matches!(plane_index, 3 | 4);
            if !is_active {
                assert_eq!(&presentation[start..start + BLOCK_SIZE], &[0.0; BLOCK_SIZE]);
                assert_eq!(
                    metadata.presentation_feeds[plane_index],
                    crate::backend::SpatialPresentationFeedMetadata::default()
                );
            }
        }
        for plane_index in 0..4 {
            let start = plane_index * BLOCK_SIZE;
            assert_eq!(&environment[start..start + BLOCK_SIZE], &[73.0; BLOCK_SIZE]);
        }
        for plane_index in 4..MAX_SPATIAL_ENVIRONMENT_PLANES {
            let start = plane_index * BLOCK_SIZE;
            assert_eq!(&environment[start..start + BLOCK_SIZE], &[0.0; BLOCK_SIZE]);
        }
    }

    #[test]
    fn spatial_output_omits_monitor_gain_and_final_limiter() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        let drive = graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        assert_eq!(drive.linear_gain().to_bits(), 1.0_f32.to_bits());
        let hot = [4.0_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&hot, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        assert_eq!(
            presentation_plane(
                &presentation,
                0,
                SpatialPresentationComponent::DirectCenter,
                BLOCK_SIZE,
            ),
            hot
        );
        assert!(!metadata.monitor_gain_applied);
        assert!(!metadata.final_hrtf_applied);
        assert!(!metadata.output_limiter_applied);
        assert_eq!(graph.safety_telemetry().limiter_engagements, 0);
        assert_eq!(graph.safety_telemetry().pre_limiter_peak, 0.0);
        assert_eq!(graph.safety_telemetry().post_limiter_peak, 0.0);
    }

    #[test]
    fn spatial_rejects_malformed_lengths_counts_and_inactive_plane_payloads() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        let input = [0.1_f32; BLOCK_SIZE];
        let short = [0.1_f32; BLOCK_SIZE - 1];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);

        let mut run = |source: SpatialProgramBlock<'_>| {
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &[source],
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
        };
        assert_eq!(
            run(SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 0,
                program_planes: [&[], &[]],
            }),
            Err(SpatialRenderError::InvalidProgramPlaneCount)
        );
        assert_eq!(
            run(SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 3,
                program_planes: [&input, &input],
            }),
            Err(SpatialRenderError::InvalidProgramPlaneCount)
        );
        assert_eq!(
            run(SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 2,
                program_planes: [&input, &input],
            }),
            Err(SpatialRenderError::ConfiguredProgramShapeMismatch {
                source_index: 0,
                configured_plane_count: 1,
                supplied_plane_count: 2,
            })
        );
        assert_eq!(
            run(SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&short, &[]],
            }),
            Err(SpatialRenderError::InvalidBlockLength)
        );
        assert_eq!(
            run(SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&input, &input],
            }),
            Err(SpatialRenderError::InactiveProgramPlaneNotEmpty)
        );
        drop(run);

        let duplicate_sources = [
            SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&input, &[]],
            },
            SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&input, &[]],
            },
        ];
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &duplicate_sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::DuplicateSourceBlock)
        );

        let invalid_index_sources = [SpatialProgramBlock {
            source_index: 2,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &invalid_index_sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::InvalidSourceIndex)
        );

        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let short_presentation_len = presentation.len() - 1;
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation[..short_presentation_len],
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::InvalidOutputBankLength)
        );

        let short_environment_len = environment.len() - 1;
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment[..short_environment_len],
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::InvalidOutputBankLength)
        );
    }

    #[test]
    fn spatial_backend_failure_or_malformed_metadata_is_silent_and_discontinuous() {
        const BLOCK_SIZE: usize = 4;
        for mode in [
            SpatialMockMode::Fail,
            SpatialMockMode::PropagationSequenceMismatch,
            SpatialMockMode::MalformedMetadata,
            SpatialMockMode::NonFinitePresentation,
            SpatialMockMode::NonFiniteEnvironment,
        ] {
            let (_writer, mut graph) = prepared_spatial_test_graph(BLOCK_SIZE as u32, mode);
            graph
                .set_source(
                    0,
                    &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                    SceneCalibration::default(),
                )
                .unwrap();
            let input = [0.25_f32; BLOCK_SIZE];
            let sources = [SpatialProgramBlock {
                source_index: 0,
                program_plane_count: 1,
                program_planes: [&input, &[]],
            }];
            let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);
            for (call, block_start_frame) in [0_u64, BLOCK_SIZE as u64].into_iter().enumerate() {
                presentation.fill(5.0);
                environment.fill(6.0);
                graph
                    .process_spatial_block(SpatialProcessBlock {
                        now_ns: call as u64,
                        block_start_frame,
                        sources: &sources,
                        presentation_bank: &mut presentation,
                        environmental_bank: &mut environment,
                        metadata: &mut metadata,
                    })
                    .unwrap();
                assert!(presentation.iter().all(|sample| *sample == 0.0));
                assert!(environment.iter().all(|sample| *sample == 0.0));
                assert_eq!(
                    metadata.validity,
                    SpatialOutputValidity::SilentDiscontinuity
                );
                assert_eq!(metadata.discontinuity_sequence, call as u64 + 1);
                assert_eq!(metadata.block_start_frame, block_start_frame);
                assert_eq!(metadata.active_presentation_feed_count, 0);
                assert_eq!(metadata.active_environmental_plane_count, 0);
            }
            assert_eq!(graph.fault_counters().backend_render_error, 2);
        }
    }

    #[test]
    fn spatial_direction_metadata_requires_a_finite_unit_vector() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        let mut metadata = graph.spatial_metadata_for_block(0, SpatialOutputValidity::Valid);
        metadata.active_presentation_feed_count = 1;
        let feed = &mut metadata.presentation_feeds[0];
        feed.valid = true;
        feed.source_index = 0;
        feed.component = SpatialPresentationComponent::DirectCenter;
        feed.placement = SpatialFeedPlacement::Direction;
        feed.direction_enu = EnuVector3::new(0.0, 1.0, 0.0);
        assert!(graph.spatial_output_metadata_is_valid(&metadata));

        for invalid in [
            EnuVector3::default(),
            EnuVector3::new(0.0, 0.5, 0.0),
            EnuVector3::new(f32::NAN, 1.0, 0.0),
            EnuVector3::new(f32::INFINITY, 0.0, 0.0),
        ] {
            metadata.presentation_feeds[0].direction_enu = invalid;
            assert!(
                !graph.spatial_output_metadata_is_valid(&metadata),
                "accepted invalid direction {invalid:?}"
            );
        }

        metadata.presentation_feeds[0].direction_enu = EnuVector3::new(0.0, 1.0, 0.0);
        metadata.presentation_feeds[0].pose_enu.position.east_m = f32::NAN;
        assert!(
            !graph.spatial_output_metadata_is_valid(&metadata),
            "direction placement must not carry a non-finite diagnostic pose"
        );
    }

    #[test]
    fn spatial_malformed_active_propagation_is_a_silent_advancing_discontinuity() {
        const BLOCK_SIZE: usize = 4;
        let (mut writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sequence: 2,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                target_delay_samples: if index == 0 { f32::NAN } else { 0.0 },
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });
        let input = [0.25_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 128,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        assert!(presentation.iter().all(|sample| *sample == 0.0));
        assert!(environment.iter().all(|sample| *sample == 0.0));
        assert_eq!(
            metadata.validity,
            SpatialOutputValidity::SilentDiscontinuity
        );
        assert_eq!(metadata.block_start_frame, 128);
        assert_eq!(metadata.discontinuity_sequence, 1);
        assert_eq!(graph.fault_counters().backend_render_error, 1);
    }

    #[test]
    fn spatial_unavailable_route_is_a_non_advancing_caller_error() {
        const BLOCK_SIZE: usize = 4;
        let (_writer, mut graph) = test_graph(BLOCK_SIZE as u32);
        let input = [0.25_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&input, &[]],
        }];
        let mut presentation = vec![5.0; MAX_SPATIAL_PRESENTATION_FEEDS * BLOCK_SIZE];
        let mut environment = vec![6.0; MAX_SPATIAL_ENVIRONMENT_PLANES * BLOCK_SIZE];
        let mut metadata = SpatialOutputMetadata {
            block_start_frame: 99,
            ..SpatialOutputMetadata::default()
        };
        assert_eq!(
            graph.process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 128,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            }),
            Err(SpatialRenderError::SpatialBackendUnavailable)
        );
        assert!(presentation.iter().all(|sample| *sample == 5.0));
        assert!(environment.iter().all(|sample| *sample == 6.0));
        assert_eq!(metadata.block_start_frame, 99);
        assert_eq!(graph.fault_counters().backend_render_error, 0);
    }

    #[test]
    fn spatial_callback_with_shaped_spectral_transfer_allocates_nothing() {
        const BLOCK_SIZE: usize = 16;
        let (_writer, mut graph) =
            prepared_spatial_test_graph(BLOCK_SIZE as u32, SpatialMockMode::Valid);
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        graph
            .set_source_spectral_transfer(
                0,
                SpectralTransfer::default()
                    .with_stage(
                        fightbox_api::spectral::SpectralStage::Occlusion,
                        [0.0, -1.0, -2.0, -4.0, -7.0, -11.0, -16.0, -24.0],
                    )
                    .unwrap(),
            )
            .unwrap();
        let mono = [0.1_f32; BLOCK_SIZE];
        let sources = [SpatialProgramBlock {
            source_index: 0,
            program_plane_count: 1,
            program_planes: [&mono, &[]],
        }];
        let (mut presentation, mut environment, mut metadata) = spatial_banks(BLOCK_SIZE);
        graph
            .process_spatial_block(SpatialProcessBlock {
                now_ns: 0,
                block_start_frame: 0,
                sources: &sources,
                presentation_bank: &mut presentation,
                environmental_bank: &mut environment,
                metadata: &mut metadata,
            })
            .unwrap();

        let allocations = count_allocations(|| {
            for block_index in 1..=256_u64 {
                graph
                    .process_spatial_block(SpatialProcessBlock {
                        now_ns: block_index,
                        block_start_frame: block_index * BLOCK_SIZE as u64,
                        sources: &sources,
                        presentation_bank: &mut presentation,
                        environmental_bank: &mut environment,
                        metadata: &mut metadata,
                    })
                    .unwrap();
            }
        });
        assert_eq!(allocations, 0);
    }

    struct IsolationBackend;

    impl BackendRenderGraph for IsolationBackend {
        fn render_block(
            &mut self,
            block: PropagationRenderBlock<'_>,
        ) -> Result<(), BackendRenderError> {
            for source in block.sources {
                let gain = source.source_index as f32 + 1.0;
                for frame in 0..block.output_left.len() {
                    block.output_left[frame] += source.input_mono[frame] * gain;
                    block.output_right[frame] -= source.input_mono[frame] * gain;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn backend_source_isolation_survives_muting_one_of_four_sources() {
        let (mut writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            block_size_frames: 16,
            max_active_sources: 4,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_backend(config, reader, Box::new(IsolationBackend)).unwrap();
        for source_index in 0..4 {
            graph
                .set_source(
                    source_index,
                    &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                    SceneCalibration::default(),
                )
                .unwrap();
        }
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index < 4,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });

        let inputs = [[0.001_f32; 16], [0.002; 16], [0.003; 16], [0.004; 16]];
        let mut before = [[[0.0_f32; 16]; 2]; 4];
        for source_index in 0..4 {
            let source = [SourceBlock {
                source_index,
                decoded_mono: &inputs[source_index],
            }];
            let (left, right) = before[source_index].split_at_mut(1);
            for _ in 0..3 {
                graph
                    .process_block(ProcessBlock {
                        now_ns: 0,
                        sources: &source,
                        output_left: &mut left[0],
                        output_right: &mut right[0],
                    })
                    .unwrap();
            }
        }

        graph.clear_source(2).unwrap();
        for source_index in 0..4 {
            let source = [SourceBlock {
                source_index,
                decoded_mono: &inputs[source_index],
            }];
            let mut left = [0.0_f32; 16];
            let mut right = [0.0_f32; 16];
            for _ in 0..3 {
                graph
                    .process_block(ProcessBlock {
                        now_ns: 0,
                        sources: &source,
                        output_left: &mut left,
                        output_right: &mut right,
                    })
                    .unwrap();
            }
            if source_index == 2 {
                assert_eq!(left, [0.0; 16]);
                assert_eq!(right, [0.0; 16]);
            } else {
                assert_eq!(left, before[source_index][0]);
                assert_eq!(right, before[source_index][1]);
            }
        }
    }

    #[test]
    fn maximum_capacity_admits_and_renders_sixteen_sources_offline() {
        assert_eq!(MAX_ACTIVE_SOURCES, 16);
        let (mut writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            block_size_frames: 16,
            max_active_sources: MAX_ACTIVE_SOURCES as u8,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_backend(config, reader, Box::new(IsolationBackend)).unwrap();
        for source_index in 0..MAX_ACTIVE_SOURCES {
            graph
                .set_source(
                    source_index,
                    &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                    SceneCalibration::default(),
                )
                .unwrap();
        }
        assert_eq!(
            graph.set_source(
                MAX_ACTIVE_SOURCES,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            ),
            Err(RenderError::InvalidSourceIndex)
        );
        writer.publish(PropagationSnapshot {
            sequence: 1,
            simulated_at_ns: 0,
            sources: std::array::from_fn(|_| SourcePropagation {
                active: true,
                target_delay_samples: 0.0,
                left_gain: 1.0,
                right_gain: 1.0,
            }),
        });

        let inputs = [[0.000_01_f32; 16]; MAX_ACTIVE_SOURCES];
        let sources: [SourceBlock<'_>; MAX_ACTIVE_SOURCES] =
            std::array::from_fn(|source_index| SourceBlock {
                source_index,
                decoded_mono: &inputs[source_index],
            });
        let mut left = [0.0_f32; 16];
        let mut right = [0.0_f32; 16];
        for _ in 0..3 {
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &sources,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
        }
        assert!(left.iter().chain(&right).all(|sample| sample.is_finite()));
        assert!(left.iter().any(|sample| *sample > 0.0));
        assert!(right.iter().any(|sample| *sample < 0.0));
    }

    #[test]
    fn runtime_graph_memory_is_exact_and_live_for_48k_128_mono() {
        let (_, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            sample_rate_hz: 48_000,
            block_size_frames: 128,
            max_active_sources: MAX_ACTIVE_SOURCES as u8,
            ..EngineConfig::default()
        };
        let mut graph = RuntimeGraph::new(config, reader).unwrap();
        let memory = graph.persistent_memory();
        let delay_samples_per_source = (48_000.0 * DEFAULT_MAX_DELAY_SECONDS).ceil() as u64 + 4;
        let expected_delay_bytes = delay_samples_per_source
            * core::mem::size_of::<f32>() as u64
            * MAX_ACTIVE_SOURCES as u64;
        let expected_scratch_bytes =
            2 * 128 * core::mem::size_of::<f32>() as u64 * MAX_ACTIVE_SOURCES as u64;

        assert_eq!(expected_delay_bytes, 6_144_256);
        assert_eq!(expected_scratch_bytes, 16_384);
        assert_eq!(memory.source_node_capacity, MAX_ACTIVE_SOURCES);
        assert_eq!(memory.propagation_delay_payload_bytes, expected_delay_bytes);
        assert_eq!(memory.block_scratch_payload_bytes, expected_scratch_bytes);
        assert_eq!(memory.spatial_scratch_payload_bytes, 0);
        assert_eq!(
            memory.total_payload_bytes,
            expected_delay_bytes + expected_scratch_bytes
        );
        assert_eq!(memory.total_payload_bytes, 6_160_640);

        let initial_capacity = graph.sources[0].calibrated.capacity();
        graph.sources[0].calibrated.reserve_exact(1);
        let grown_capacity = graph.sources[0].calibrated.capacity();
        assert!(grown_capacity > initial_capacity);
        let growth_bytes =
            (grown_capacity - initial_capacity) as u64 * core::mem::size_of::<f32>() as u64;
        let grown = graph.persistent_memory();
        assert_eq!(
            grown.block_scratch_payload_bytes,
            expected_scratch_bytes + growth_bytes
        );
        assert_eq!(
            grown.total_payload_bytes,
            expected_delay_bytes + expected_scratch_bytes + growth_bytes
        );
    }

    #[test]
    fn runtime_graph_memory_is_exact_and_live_for_48k_128_spatial() {
        let (_, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            sample_rate_hz: 48_000,
            block_size_frames: 128,
            max_active_sources: MAX_ACTIVE_SOURCES as u8,
            ..EngineConfig::default()
        };
        let program_plane_counts = [2; MAX_ACTIVE_SOURCES];
        let mut graph = RuntimeGraph::new_with_spatial_backend(
            config,
            reader,
            &program_plane_counts,
            Box::new(PassthroughSpatialBackend {
                mode: SpatialMockMode::Valid,
                generation: 11,
                prepared_for_realtime: false,
                call_count: None,
                last_source_mask: None,
                last_propagation_sequence: None,
            }),
        )
        .unwrap();
        let memory = graph.persistent_memory();
        let expected_delay_bytes = 6_144_256;
        let expected_block_scratch_bytes = 16_384;
        let expected_spatial_scratch_bytes = 16 * 2 * 128 * core::mem::size_of::<f32>() as u64;

        assert_eq!(expected_spatial_scratch_bytes, 16_384);
        assert_eq!(memory.source_node_capacity, MAX_ACTIVE_SOURCES);
        assert_eq!(memory.propagation_delay_payload_bytes, expected_delay_bytes);
        assert_eq!(
            memory.block_scratch_payload_bytes,
            expected_block_scratch_bytes
        );
        assert_eq!(
            memory.spatial_scratch_payload_bytes,
            expected_spatial_scratch_bytes
        );
        assert_eq!(memory.total_payload_bytes, 6_177_024);

        let spatial_scratch = graph.spatial_scratch.as_mut().unwrap();
        let plane = &mut spatial_scratch.sources[MAX_ACTIVE_SOURCES - 1].calibrated_planes[1];
        let initial_capacity = plane.capacity();
        plane.reserve_exact(1);
        let grown_capacity = plane.capacity();
        assert!(grown_capacity > initial_capacity);
        let growth_bytes =
            (grown_capacity - initial_capacity) as u64 * core::mem::size_of::<f32>() as u64;
        let grown = graph.persistent_memory();
        assert_eq!(
            grown.spatial_scratch_payload_bytes,
            expected_spatial_scratch_bytes + growth_bytes
        );
        assert_eq!(grown.total_payload_bytes, 6_177_024 + growth_bytes);
    }

    struct FailingBackend;

    impl BackendRenderGraph for FailingBackend {
        fn render_block(
            &mut self,
            _block: PropagationRenderBlock<'_>,
        ) -> Result<(), BackendRenderError> {
            Err(BackendRenderError::InactiveGraph)
        }
    }

    /// Emits NaN in its first block, a sample the monitor gain overflows in
    /// its second, then a steady signal.
    struct NonFiniteBackend {
        blocks: u32,
    }

    impl BackendRenderGraph for NonFiniteBackend {
        fn render_block(
            &mut self,
            block: PropagationRenderBlock<'_>,
        ) -> Result<(), BackendRenderError> {
            block.output_left.fill(0.25);
            block.output_right.fill(0.25);
            match self.blocks {
                0 => block.output_right[1] = f32::NAN,
                1 => block.output_left[2] = f32::MAX,
                _ => {}
            }
            self.blocks += 1;
            Ok(())
        }
    }

    #[test]
    fn a_non_finite_backend_block_is_silenced_before_the_limiter() {
        let (_writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let (_safety_control, safety_reader) = OutputSafetyPublication::new(OutputSafetyConfig {
            monitor_gain_db: 24.0,
            ..OutputSafetyConfig::default()
        })
        .unwrap();
        let config = EngineConfig {
            block_size_frames: 4,
            ..EngineConfig::default()
        };
        let mut graph = RuntimeGraph::new_with_backend_and_output_safety(
            config,
            reader,
            safety_reader,
            Box::new(NonFiniteBackend { blocks: 0 }),
        )
        .unwrap();
        let mut rendered = Vec::new();
        for _ in 0..16 {
            let mut left = [9.0; 4];
            let mut right = [9.0; 4];
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &[],
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .unwrap();
            rendered.extend(left.into_iter().chain(right));
        }
        assert!(rendered.iter().all(|sample| sample.is_finite()));
        // The limiter's history stayed clean: the later signal comes through.
        assert!(
            rendered[rendered.len() - 8..]
                .iter()
                .all(|sample| *sample > 0.0)
        );
        assert_eq!(graph.safety_telemetry().non_finite_blocks, 2);
    }

    #[test]
    fn backend_fault_keeps_callback_alive_and_silences_the_block() {
        let (mut writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            block_size_frames: 4,
            ..EngineConfig::default()
        };
        let mut graph =
            RuntimeGraph::new_with_backend(config, reader, Box::new(FailingBackend)).unwrap();
        graph
            .set_source(
                0,
                &source_profile(ReferenceLevel::CreativeDb { db: 0.0 }),
                SceneCalibration::default(),
            )
            .unwrap();
        writer.publish(PropagationSnapshot {
            sources: std::array::from_fn(|index| SourcePropagation {
                active: index == 0,
                ..SourcePropagation::default()
            }),
            ..PropagationSnapshot::default()
        });
        let input = [1.0; 4];
        let sources = [SourceBlock {
            source_index: 0,
            decoded_mono: &input,
        }];
        let mut left = [9.0; 4];
        let mut right = [9.0; 4];
        assert!(
            graph
                .process_block(ProcessBlock {
                    now_ns: 0,
                    sources: &sources,
                    output_left: &mut left,
                    output_right: &mut right,
                })
                .is_ok()
        );
        assert_eq!(left, [0.0; 4]);
        assert_eq!(right, [0.0; 4]);
        assert_eq!(graph.fault_counters().backend_render_error, 1);
    }
}
