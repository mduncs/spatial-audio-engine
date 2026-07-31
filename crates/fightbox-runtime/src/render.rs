//! Unified offline/live block processor and per-source runtime graph shell.

use crate::backend::{
    BackendRenderGraph, BackendSourceBlock, ListenerOrientation, PropagationRenderBlock,
};
use crate::safety::{
    OutputSafetyPublication, OutputSafetyReader, SafetyTelemetry, TruePeakLimiter,
};
use crate::{FractionalDelayLine, SnapshotReader};
use fightbox_api::{
    CalibrationError, EngineConfig, ListenerState, OutputSafetyConfig, SceneCalibration,
    SourceDrive, SourceError, SourceProfile,
};
use std::time::Instant;

pub const MAX_ACTIVE_SOURCES: usize = 8;
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

/// The sole render entry point shared by offline and future device wrappers.
pub trait BlockProcessor {
    fn block_size_frames(&self) -> usize;
    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError>;

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
    TooManySources,
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
    fn new(block_size: usize, maximum_delay_samples: usize) -> Self {
        Self {
            drive: None,
            delay: FractionalDelayLine::new(
                maximum_delay_samples,
                0.0,
                DEFAULT_DELAY_SLEW_SAMPLES_PER_SAMPLE,
            ),
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
    }
}

/// Fixed-capacity, preallocated per-source graph shell.
pub struct RuntimeGraph {
    config: EngineConfig,
    snapshot_reader: SnapshotReader<PropagationSnapshot>,
    output_safety_reader: OutputSafetyReader,
    sources: [SourceNode; MAX_ACTIVE_SOURCES],
    listener: Option<ListenerState>,
    backend: Option<Box<dyn BackendRenderGraph>>,
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
        let sources = std::array::from_fn(|_| SourceNode::new(block_size, maximum_delay_samples));
        let block_period_ns =
            u64::from(config.block_size_frames) * 1_000_000_000 / u64::from(config.sample_rate_hz);
        let deadline_ns = block_period_ns.saturating_mul(8) / 10;
        let block_seconds = config.block_size_frames as f32 / config.sample_rate_hz as f32;
        let snapshot_gain_block_retention =
            (-block_seconds / SNAPSHOT_GAIN_SLEW_TIME_SECONDS).exp();
        let output_safety_gain_block_retention =
            (-block_seconds / OUTPUT_SAFETY_GAIN_SLEW_TIME_SECONDS).exp();
        Ok(Self {
            config,
            snapshot_reader,
            output_safety_reader,
            sources,
            listener: None,
            backend: None,
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
        source.reset_smoothing();
        Ok(())
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
}

impl BlockProcessor for RuntimeGraph {
    fn block_size_frames(&self) -> usize {
        self.config.block_size_frames as usize
    }

    fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
        self.validate_block(&block)?;
        let started = Instant::now();
        block.output_left.fill(0.0);
        block.output_right.fill(0.0);

        let snapshot = self.snapshot_reader.read();
        let (source_safety_targets, monitor_gain_target) = self.output_safety_reader.read();
        if block.now_ns.saturating_sub(snapshot.simulated_at_ns) > self.snapshot_stale_after_ns {
            self.telemetry.faults.snapshot_stale =
                self.telemetry.faults.snapshot_stale.saturating_add(1);
        }

        let block_size = self.block_size_frames();
        let use_simple_gain = self.backend.is_none();
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
            if !source.safety_gain_initialized {
                source.applied_safety_gain = source_safety_target;
                source.safety_gain_initialized = true;
            }

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
            let source_safety_gain_target = source_safety_target
                + (source.applied_safety_gain - source_safety_target)
                    * self.output_safety_gain_block_retention;
            let source_safety_gain_step =
                (source_safety_gain_target - source.applied_safety_gain) / block_size as f32;
            let mut delay_samples = source.applied_delay_samples;
            let mut left_gain = source.applied_left_gain;
            let mut right_gain = source.applied_right_gain;
            let mut source_safety_gain = source.applied_safety_gain;
            if source_safety_gain.min(source_safety_gain_target) < 1.0 - f32::EPSILON {
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
                    source_safety_gain = source_safety_gain_target;
                } else {
                    delay_samples += delay_step;
                    left_gain += left_gain_step;
                    right_gain += right_gain_step;
                    source_safety_gain += source_safety_gain_step;
                }
                source.calibrated[frame] =
                    input.decoded_mono[frame] * drive.linear_gain() * source_safety_gain;
                source.delayed[frame] = source
                    .delay
                    .process_sample_at_delay(source.calibrated[frame], delay_samples);
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
            source.applied_safety_gain = source_safety_gain_target;
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
        let mut limiter_engaged = false;
        for frame in 0..block_size {
            if frame + 1 == block_size {
                monitor_gain = monitor_gain_endpoint;
            } else {
                monitor_gain += monitor_gain_step;
            }
            let pre_left = block.output_left[frame] * monitor_gain;
            let pre_right = block.output_right[frame] * monitor_gain;
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

        let duration_ns = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
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
    use crate::SnapshotPublication;
    use crate::backend::{BackendRenderError, PropagationRenderBlock};
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
            max_speed_mps: 100.0,
        }
    }

    fn test_graph(block_size: u32) -> (crate::SnapshotWriter<PropagationSnapshot>, RuntimeGraph) {
        let (writer, reader) = SnapshotPublication::new(PropagationSnapshot::default());
        let config = EngineConfig {
            block_size_frames: block_size,
            ..EngineConfig::default()
        };
        (writer, RuntimeGraph::new(config, reader).unwrap())
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
                active: index == 0,
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
    fn published_safety_targets_are_source_local_and_monitor_gain_slews() {
        let (mut propagation_writer, propagation_reader) =
            SnapshotPublication::new(PropagationSnapshot::default());
        let (mut safety_control, safety_reader) =
            OutputSafetyPublication::new(OutputSafetyConfig::default()).unwrap();
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
                active: index < 2,
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
    fn render_path_allocates_nothing_after_construction_and_warmup() {
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

    struct FailingBackend;

    impl BackendRenderGraph for FailingBackend {
        fn render_block(
            &mut self,
            _block: PropagationRenderBlock<'_>,
        ) -> Result<(), BackendRenderError> {
            Err(BackendRenderError::InactiveGraph)
        }
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
