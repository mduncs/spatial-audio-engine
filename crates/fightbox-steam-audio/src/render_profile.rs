//! Opt-in, allocation-free timing for one retained render graph.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);
static TOTALS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
const STAGES: usize = 6;
const BUCKET_WIDTH_NS: u64 = 8_000;
const BUCKETS: usize = 1_025;
const FIRST_BLOCKS: usize = 32;
static HISTOGRAMS: [[AtomicU64; BUCKETS]; STAGES] =
    [const { [const { AtomicU64::new(0) }; BUCKETS] }; STAGES];
static INITIAL_BLOCKS: [[AtomicU64; STAGES]; FIRST_BLOCKS] =
    [const { [const { AtomicU64::new(0) }; STAGES] }; FIRST_BLOCKS];
static RECORDED_BLOCKS: AtomicU64 = AtomicU64::new(0);
static CPU_TOTAL_NS: AtomicU64 = AtomicU64::new(0);
static CPU_HISTOGRAM: [AtomicU64; BUCKETS] = [const { AtomicU64::new(0) }; BUCKETS];
static INITIAL_CPU_NS: [AtomicU64; FIRST_BLOCKS] = [const { AtomicU64::new(0) }; FIRST_BLOCKS];
static QUALITY_HISTOGRAMS: [[AtomicU64; BUCKETS]; 4] =
    [const { [const { AtomicU64::new(0) }; BUCKETS] }; 4];
static QUALITY_DEADLINES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
const REFLECTION_PARTS: usize = 3;
const RUNGS: usize = 16;
const SLOW_REFLECTION_BLOCKS: usize = 128;
static REFLECTION_TOTALS: [AtomicU64; REFLECTION_PARTS] = [const { AtomicU64::new(0) }; REFLECTION_PARTS];
static REFLECTION_EVENTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static REFLECTION_HISTOGRAMS: [[[AtomicU64; BUCKETS]; 4]; RUNGS] =
    [const { [const { [const { AtomicU64::new(0) }; BUCKETS] }; 4] }; RUNGS];
static ADOPTION_HISTOGRAMS: [[AtomicU64; BUCKETS]; 2] =
    [const { [const { AtomicU64::new(0) }; BUCKETS] }; 2];
static SLOW_REFLECTION_ROWS: [[AtomicU64; 10]; SLOW_REFLECTION_BLOCKS] =
    [const { [const { AtomicU64::new(0) }; 10] }; SLOW_REFLECTION_BLOCKS];
static SLOW_REFLECTION_COUNT: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod thread_cpu {
    unsafe extern "C" {
        fn clock_gettime_nsec_np(clock_id: i32) -> u64;
    }

    pub(super) fn now_ns() -> u64 {
        // CLOCK_THREAD_CPUTIME_ID from the macOS SDK's time.h.
        unsafe { clock_gettime_nsec_np(16) }
    }
}

fn thread_cpu_ns() -> Option<u64> {
    #[cfg(target_os = "macos")]
    { Some(thread_cpu::now_ns()) }
    #[cfg(not(target_os = "macos"))]
    { None }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RenderProfileTotals {
    pub total_ns: u64,
    pub preparation_ns: u64,
    pub direct_ns: u64,
    pub path_ns: u64,
    pub echo_ns: u64,
    pub reflection_ns: u64,
    pub block_count: u64,
    pub source_count: u64,
    pub echo_tap_count: u64,
}

/// Enable only while running one render graph. Disabled by default.
pub fn enable_render_profiling(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Control-side snapshot; the final bucket contains every duration >= 8.192 ms.
pub struct RenderProfileHistograms {
    pub stage_names: [&'static str; STAGES],
    pub bucket_width_ns: u64,
    pub counts: [Vec<u64>; STAGES],
    pub first_blocks_ns: Vec<[u64; STAGES]>,
    pub thread_cpu_available: bool,
    pub thread_cpu_total_ns: u64,
    pub thread_cpu_counts: Vec<u64>,
    pub first_blocks_thread_cpu_ns: Vec<u64>,
    pub quality_counts: [Vec<u64>; 4],
    pub quality_deadline_misses: [u64; 4],
    pub reflection_counts_by_rung: Vec<[Vec<u64>; 4]>,
    pub reflection_adoption_counts: [Vec<u64>; 2],
    pub slow_reflection_blocks: Vec<[u64; 10]>,
}

/// Reset only while rendering is stopped. Cumulative offline totals are kept.
pub fn reset_render_profile_histograms() {
    for histogram in &HISTOGRAMS {
        for count in histogram {
            count.store(0, Ordering::Relaxed);
        }
    }
    for block in &INITIAL_BLOCKS {
        for duration in block {
            duration.store(0, Ordering::Relaxed);
        }
    }
    CPU_TOTAL_NS.store(0, Ordering::Relaxed);
    for count in &CPU_HISTOGRAM {
        count.store(0, Ordering::Relaxed);
    }
    for duration in &INITIAL_CPU_NS {
        duration.store(0, Ordering::Relaxed);
    }
    RECORDED_BLOCKS.store(0, Ordering::Relaxed);
    for histogram in &QUALITY_HISTOGRAMS {
        for count in histogram { count.store(0, Ordering::Relaxed); }
    }
    for count in &QUALITY_DEADLINES { count.store(0, Ordering::Relaxed); }
    for rung in &REFLECTION_HISTOGRAMS {
        for histogram in rung {
            for count in histogram { count.store(0, Ordering::Relaxed); }
        }
    }
    for histogram in &ADOPTION_HISTOGRAMS {
        for count in histogram { count.store(0, Ordering::Relaxed); }
    }
    for count in REFLECTION_TOTALS.iter().chain(&REFLECTION_EVENTS) { count.store(0, Ordering::Relaxed); }
    for row in &SLOW_REFLECTION_ROWS {
        for value in row { value.store(0, Ordering::Relaxed); }
    }
    SLOW_REFLECTION_COUNT.store(0, Ordering::Relaxed);
}

/// Allocates only on control, after rendering has stopped.
pub fn render_profile_histograms() -> RenderProfileHistograms {
    RenderProfileHistograms {
        stage_names: ["total", "preparation", "direct", "path", "echo", "reflection"],
        bucket_width_ns: BUCKET_WIDTH_NS,
        counts: std::array::from_fn(|stage| HISTOGRAMS[stage].iter()
            .map(|count| count.load(Ordering::Relaxed)).collect()),
        first_blocks_ns: (0..RECORDED_BLOCKS.load(Ordering::Relaxed).min(FIRST_BLOCKS as u64) as usize)
            .map(|block| std::array::from_fn(|stage| INITIAL_BLOCKS[block][stage].load(Ordering::Relaxed)))
            .collect(),
        thread_cpu_available: cfg!(target_os = "macos"),
        thread_cpu_total_ns: CPU_TOTAL_NS.load(Ordering::Relaxed),
        thread_cpu_counts: CPU_HISTOGRAM.iter().map(|count| count.load(Ordering::Relaxed)).collect(),
        first_blocks_thread_cpu_ns: (0..RECORDED_BLOCKS.load(Ordering::Relaxed).min(FIRST_BLOCKS as u64) as usize)
            .map(|block| INITIAL_CPU_NS[block].load(Ordering::Relaxed)).collect(),
        quality_counts: std::array::from_fn(|level| QUALITY_HISTOGRAMS[level].iter()
            .map(|count| count.load(Ordering::Relaxed)).collect()),
        quality_deadline_misses: std::array::from_fn(|level| QUALITY_DEADLINES[level].load(Ordering::Relaxed)),
        reflection_counts_by_rung: REFLECTION_HISTOGRAMS.iter().map(|rung|
            std::array::from_fn(|part| rung[part].iter().map(|count| count.load(Ordering::Relaxed)).collect())).collect(),
        reflection_adoption_counts: std::array::from_fn(|kind| ADOPTION_HISTOGRAMS[kind].iter()
            .map(|count| count.load(Ordering::Relaxed)).collect()),
        slow_reflection_blocks: (0..SLOW_REFLECTION_COUNT.load(Ordering::Relaxed).min(SLOW_REFLECTION_BLOCKS as u64) as usize)
            .map(|row| std::array::from_fn(|column| SLOW_REFLECTION_ROWS[row][column].load(Ordering::Relaxed))).collect(),
    }
}

/// Cumulative timings; subtract two snapshots around a block for its breakdown.
pub fn render_profile_totals() -> RenderProfileTotals {
    let t: [u64; 10] = std::array::from_fn(|i| TOTALS[i].load(Ordering::Relaxed));
    RenderProfileTotals {
        total_ns: t[0], preparation_ns: t[1], direct_ns: t[2], path_ns: t[3],
        echo_ns: t[4], reflection_ns: t[5], block_count: t[6],
        source_count: t[7], echo_tap_count: t[8],
    }
}

pub(crate) fn count(index: usize) {
    if ENABLED.load(Ordering::Relaxed) {
        TOTALS[index].fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) struct Timer {
    started: Instant,
    index: usize,
    stage_before: [u64; STAGES - 1],
    cpu_started: Option<u64>,
    quality: Option<(usize, u64, usize)>,
    reflection_before: [u64; REFLECTION_PARTS],
    reflection_events_before: [u64; 3],
}

impl Timer {
    pub(crate) fn set_quality(&mut self, level: crate::ReflectionQualityLevel, period_ns: u64, rung: u16) {
        let index = match level {
            crate::ReflectionQualityLevel::Full => 0,
            crate::ReflectionQualityLevel::Reduced => 1,
            crate::ReflectionQualityLevel::Minimum => 2,
            crate::ReflectionQualityLevel::Intermediate => 3,
        };
        self.quality = Some((index, period_ns, usize::from(rung).min(RUNGS - 1)));
    }
}

pub(crate) fn timer(index: usize) -> Option<Timer> {
    ENABLED.load(Ordering::Relaxed).then(|| Timer {
        started: Instant::now(), index,
        stage_before: if index == 0 {
            std::array::from_fn(|stage| TOTALS[stage + 1].load(Ordering::Relaxed))
        } else { [0; STAGES - 1] },
        cpu_started: if index == 0 { thread_cpu_ns() } else { None },
        quality: None,
        reflection_before: if index == 0 { std::array::from_fn(|part| REFLECTION_TOTALS[part].load(Ordering::Relaxed)) } else { [0; REFLECTION_PARTS] },
        reflection_events_before: if index == 0 { std::array::from_fn(|event| REFLECTION_EVENTS[event].load(Ordering::Relaxed)) } else { [0; 3] },
    })
}

impl Drop for Timer {
    fn drop(&mut self) {
        let cpu_elapsed = self.cpu_started.and_then(|started| thread_cpu_ns()
            .map(|finished| finished.saturating_sub(started)));
        let elapsed = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        TOTALS[self.index].fetch_add(elapsed, Ordering::Relaxed);
        if self.index == 0 {
            if let Some((level, period_ns, _)) = self.quality {
                let bucket = (elapsed / BUCKET_WIDTH_NS).min((BUCKETS - 1) as u64) as usize;
                QUALITY_HISTOGRAMS[level][bucket].fetch_add(1, Ordering::Relaxed);
                if elapsed >= period_ns { QUALITY_DEADLINES[level].fetch_add(1, Ordering::Relaxed); }
            }
            let durations: [u64; STAGES] = std::array::from_fn(|stage| {
                if stage == 0 { elapsed } else {
                    TOTALS[stage].load(Ordering::Relaxed).saturating_sub(self.stage_before[stage - 1])
                }
            });
            let block = RECORDED_BLOCKS.fetch_add(1, Ordering::Relaxed);
            if let Some((_, _, rung)) = self.quality {
                let parts: [u64; REFLECTION_PARTS] = std::array::from_fn(|part|
                    REFLECTION_TOTALS[part].load(Ordering::Relaxed).saturating_sub(self.reflection_before[part]));
                let events: [u64; 3] = std::array::from_fn(|event|
                    REFLECTION_EVENTS[event].load(Ordering::Relaxed).saturating_sub(self.reflection_events_before[event]));
                for (part, duration) in [durations[5], parts[0], parts[1], parts[2]].into_iter().enumerate() {
                    let bucket = (duration / BUCKET_WIDTH_NS).min((BUCKETS - 1) as u64) as usize;
                    REFLECTION_HISTOGRAMS[rung][part][bucket].fetch_add(1, Ordering::Relaxed);
                }
                let bucket = (durations[5] / BUCKET_WIDTH_NS).min((BUCKETS - 1) as u64) as usize;
                ADOPTION_HISTOGRAMS[usize::from(events[1] != 0)][bucket].fetch_add(1, Ordering::Relaxed);
                if durations[5] >= 750_000 {
                    let row = SLOW_REFLECTION_COUNT.fetch_add(1, Ordering::Relaxed) as usize;
                    if row < SLOW_REFLECTION_BLOCKS {
                        for (column, value) in [block, rung as u64, events[0], events[1], events[2], durations[5], parts[0], parts[1], parts[2], cpu_elapsed.unwrap_or(0)].into_iter().enumerate() {
                            SLOW_REFLECTION_ROWS[row][column].store(value, Ordering::Relaxed);
                        }
                    }
                }
            }
            if let Some(duration) = cpu_elapsed {
                CPU_TOTAL_NS.fetch_add(duration, Ordering::Relaxed);
                let bucket = (duration / BUCKET_WIDTH_NS).min((BUCKETS - 1) as u64) as usize;
                CPU_HISTOGRAM[bucket].fetch_add(1, Ordering::Relaxed);
                if block < FIRST_BLOCKS as u64 {
                    INITIAL_CPU_NS[block as usize].store(duration, Ordering::Relaxed);
                }
            }
            for (stage, duration) in durations.into_iter().enumerate() {
                let bucket = (duration / BUCKET_WIDTH_NS).min((BUCKETS - 1) as u64) as usize;
                HISTOGRAMS[stage][bucket].fetch_add(1, Ordering::Relaxed);
                if block < FIRST_BLOCKS as u64 {
                    INITIAL_BLOCKS[block as usize][stage].store(duration, Ordering::Relaxed);
                }
            }
        }
    }
}

pub(crate) fn reflection_apply_event(adoption: bool, held: bool) {
    if ENABLED.load(Ordering::Relaxed) {
        REFLECTION_EVENTS[0].fetch_add(1, Ordering::Relaxed);
        REFLECTION_EVENTS[1].fetch_add(u64::from(adoption), Ordering::Relaxed);
        REFLECTION_EVENTS[2].fetch_add(u64::from(held), Ordering::Relaxed);
    }
}

pub(crate) struct ReflectionTimer { started: Instant, part: usize }

pub(crate) fn reflection_timer(part: usize) -> Option<ReflectionTimer> {
    ENABLED.load(Ordering::Relaxed).then(|| ReflectionTimer { started: Instant::now(), part })
}

impl Drop for ReflectionTimer {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        REFLECTION_TOTALS[self.part].fetch_add(elapsed, Ordering::Relaxed);
    }
}
