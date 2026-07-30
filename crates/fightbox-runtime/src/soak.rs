//! Bounded offline/live soak reporting.

use crate::{
    BlockProcessor, FaultCounters, ProcessBlock, RunTimingHistogram, SourceBlock, TimingHistory,
};
use std::time::Instant;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TimingPercentiles {
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    pub p99_9_ms: f64,
}

impl TimingPercentiles {
    #[must_use]
    pub fn from_history(history: &TimingHistory) -> Self {
        let milliseconds =
            |percentile| history.percentile_ns(percentile).unwrap_or(0) as f64 / 1_000_000.0;
        Self {
            p50_ms: milliseconds(50.0),
            p95_ms: milliseconds(95.0),
            p99_ms: milliseconds(99.0),
            p99_9_ms: milliseconds(99.9),
        }
    }

    #[must_use]
    pub fn from_histogram(histogram: &RunTimingHistogram) -> Self {
        let milliseconds =
            |percentile| histogram.percentile_ns(percentile).unwrap_or(0) as f64 / 1_000_000.0;
        Self {
            p50_ms: milliseconds(50.0),
            p95_ms: milliseconds(95.0),
            p99_ms: milliseconds(99.0),
            p99_9_ms: milliseconds(99.9),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SoakReport {
    pub rendered_blocks: u64,
    pub window_callback_timings: TimingPercentiles,
    pub run_callback_timings: TimingPercentiles,
    pub deadline_misses: u64,
    pub faults: FaultCounters,
}

/// Runs the exact block processor used by live output for the requested
/// virtual duration. The loop is intentionally unpaced for fast CI soaks.
pub fn run_offline_soak<P: BlockProcessor>(
    processor: &mut P,
    sample_rate_hz: u32,
    seconds: u64,
    sources: &[SourceBlock<'_>],
) -> Result<SoakReport, crate::RenderError> {
    let block_size = processor.block_size_frames();
    let total_frames = seconds.saturating_mul(u64::from(sample_rate_hz));
    let blocks = total_frames.div_ceil(block_size as u64);
    let mut left = vec![0.0; block_size];
    let mut right = vec![0.0; block_size];
    let mut timings = TimingHistory::default();
    let mut run_timings = RunTimingHistogram::default();
    let block_ns = (block_size as u64)
        .saturating_mul(1_000_000_000)
        .checked_div(u64::from(sample_rate_hz))
        .unwrap_or(0);
    let mut callback_deadline_misses = 0_u64;

    for block_index in 0..blocks {
        let started = Instant::now();
        processor.process_block(ProcessBlock {
            now_ns: block_index.saturating_mul(block_ns),
            sources,
            output_left: &mut left,
            output_right: &mut right,
        })?;
        let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        timings.record(elapsed);
        run_timings.record(elapsed);
        if elapsed > block_ns.saturating_mul(8) / 10 {
            callback_deadline_misses = callback_deadline_misses.saturating_add(1);
        }
    }

    let faults = processor.fault_counters();
    Ok(SoakReport {
        rendered_blocks: blocks,
        window_callback_timings: TimingPercentiles::from_history(&timings),
        run_callback_timings: TimingPercentiles::from_histogram(&run_timings),
        deadline_misses: callback_deadline_misses.max(faults.deadline_miss),
        faults,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderError;

    struct SilentProcessor {
        blocks: u64,
    }

    impl BlockProcessor for SilentProcessor {
        fn block_size_frames(&self) -> usize {
            16
        }

        fn process_block(&mut self, block: ProcessBlock<'_>) -> Result<(), RenderError> {
            block.output_left.fill(0.0);
            block.output_right.fill(0.0);
            self.blocks += 1;
            Ok(())
        }
    }

    #[test]
    fn offline_soak_reports_virtual_duration_without_pacing() {
        let mut processor = SilentProcessor { blocks: 0 };
        let report = run_offline_soak(&mut processor, 64, 2, &[]).unwrap();
        assert_eq!(report.rendered_blocks, 8);
        assert_eq!(processor.blocks, 8);
        assert!(report.window_callback_timings.p99_9_ms >= 0.0);
        assert!(report.run_callback_timings.p99_9_ms >= 0.0);
    }

    #[test]
    fn run_wide_percentile_detects_slow_samples_evicted_from_window() {
        let fast_ns = 100_000;
        let slow_ns = 3_000_000;
        let mut window = TimingHistory::default();
        let mut run = RunTimingHistogram::default();

        for _ in 0..30 {
            window.record(slow_ns);
            run.record(slow_ns);
        }
        for _ in 0..10_000 {
            window.record(fast_ns);
            run.record(fast_ns);
        }

        let window = TimingPercentiles::from_history(&window);
        let run = TimingPercentiles::from_histogram(&run);
        assert_eq!(window.p99_9_ms, fast_ns as f64 / 1_000_000.0);
        assert!(run.p99_9_ms >= slow_ns as f64 / 1_000_000.0);
    }
}
