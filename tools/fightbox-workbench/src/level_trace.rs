//! Actual output PCM trail. Single callback writer / single UI reader only.
//! No callback allocation, locks, waiting, filesystem work, or unsafe code.
//! Pose is sample-and-held per callback, not interpolated or source telemetry.
use serde::Serialize;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct TraceIdentity {
    pub generation: u64,
    /// Caller increments on any mix, stage, gain, enable/retrigger or route change.
    pub epoch: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct BlockContext {
    pub recording: bool,
    pub identity: TraceIdentity,
    pub listener_position_m: [f32; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum WindowEnd {
    Complete,
    IdentityChanged,
    RecordingStopped,
    Flushed,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct LevelSample {
    pub sequence: u64,
    /// Stream-local audio frames, half-open interval. Clock includes recording-off time.
    pub start_frame: u64,
    pub end_frame: u64,
    pub identity: TraceIdentity,
    pub listener_start_m: [f32; 3],
    pub listener_end_m: [f32; 3],
    pub pose_valid: bool,
    pub segment_start: bool,
    pub end_reason: WindowEnd,
    pub exact_silence: bool,
    pub nonfinite_frames: u64,
    /// None for invalid PCM; exact silence has Some(0), not an invented dB floor.
    pub rms_linear: Option<f64>,
    pub peak_linear: Option<f64>,
    /// Known missing windows before this one, based on callback sequence numbers.
    pub gap_before: u64,
}
impl LevelSample {
    pub fn rms_dbfs(&self) -> Option<f64> {
        self.rms_linear
            .filter(|x| *x > 0.0)
            .map(|x| 20.0 * x.log10())
    }
    pub fn peak_dbfs(&self) -> Option<f64> {
        self.peak_linear
            .filter(|x| *x > 0.0)
            .map(|x| 20.0 * x.log10())
    }
    /// Necessary continuity only: source phase and acoustic causality are unavailable.
    pub fn can_connect_from(&self, previous: &Self) -> bool {
        !self.segment_start
            && self.gap_before == 0
            && self.pose_valid
            && previous.pose_valid
            && self.nonfinite_frames == 0
            && previous.nonfinite_frames == 0
            && self.identity == previous.identity
            && self.start_frame == previous.end_frame
            && self.sequence == previous.sequence.saturating_add(1)
    }
}

// Ready release/acquire transfers a slot. Consumer releases it only after copying
// every atomic word. Neither endpoint is Clone; a full producer never overwrites.
// Atomics avoid UnsafeCell/unsafe Sync used by the private, audio-specific capture ring.
struct Slot {
    ready: AtomicBool,
    words: [AtomicU64; 15],
}
struct Shared {
    slots: Box<[Slot]>,
    dropped: AtomicU64,
    recording: AtomicBool,
    unavailable_output_frames: AtomicU64,
}

struct Accumulator {
    start: u64,
    frames: u64,
    identity: TraceIdentity,
    first: [f32; 3],
    last: [f32; 3],
    pose_valid: bool,
    segment_start: bool,
    sum: f64,
    peak: f64,
    nonfinite: u64,
    silence: bool,
}

pub struct LevelTraceWriter {
    shared: Arc<Shared>,
    write_index: usize,
    window_frames: u64,
    clock: u64,
    sequence: u64,
    acc: Option<Accumulator>,
    active_identity: Option<TraceIdentity>,
    next_segment: bool,
}
pub struct LevelTraceReader {
    shared: Arc<Shared>,
    read_index: usize,
    sample_rate_hz: u32,
    window_frames: u64,
    retained: VecDeque<LevelSample>,
    history_capacity: usize,
    next_sequence: u64,
    evicted: u64,
}

/// Allocates only at setup. Queue/history bounds are explicit; zero is rejected.
/// The writer must be observed on every callback, including while recording is off.
/// One channel per stream generation; flush old writer before disposing it.
pub fn channel(
    sample_rate_hz: u32,
    queue_capacity: usize,
    history_capacity: usize,
) -> Result<(LevelTraceWriter, LevelTraceReader), &'static str> {
    if sample_rate_hz == 0 || queue_capacity == 0 || history_capacity == 0 {
        return Err("level trace requires nonzero rate and capacities");
    }
    let window_frames = (u64::from(sample_rate_hz) + 5) / 10;
    let shared = Arc::new(Shared {
        slots: (0..queue_capacity)
            .map(|_| Slot {
                ready: AtomicBool::new(false),
                words: std::array::from_fn(|_| AtomicU64::new(0)),
            })
            .collect(),
        dropped: AtomicU64::new(0),
        recording: AtomicBool::new(false),
        unavailable_output_frames: AtomicU64::new(0),
    });
    Ok((
        LevelTraceWriter {
            shared: shared.clone(),
            write_index: 0,
            window_frames: window_frames.max(1),
            clock: 0,
            sequence: 0,
            acc: None,
            active_identity: None,
            next_segment: true,
        },
        LevelTraceReader {
            shared,
            read_index: 0,
            sample_rate_hz,
            window_frames: window_frames.max(1),
            retained: VecDeque::with_capacity(history_capacity),
            history_capacity,
            next_sequence: 0,
            evicted: 0,
        },
    ))
}

impl LevelTraceWriter {
    /// Block pose applies unchanged to all samples in this callback. Unequal channel
    /// lengths are an invalid PCM window, not silently shortened stereo evidence.
    pub fn observe(&mut self, left: &[f32], right: &[f32], context: BlockContext) {
        if !context.recording {
            self.finish(WindowEnd::RecordingStopped);
            self.active_identity = None;
            self.shared.recording.store(false, Ordering::Release);
            self.next_segment = true;
            self.clock = self
                .clock
                .saturating_add(left.len().max(right.len()) as u64);
            return;
        }
        self.shared.recording.store(true, Ordering::Release);
        if self.active_identity != Some(context.identity) {
            self.finish(WindowEnd::IdentityChanged);
            self.active_identity = Some(context.identity);
            self.next_segment = true;
        }
        for i in 0..left.len().max(right.len()) {
            let acc = self.acc.get_or_insert_with(|| Accumulator {
                start: self.clock,
                frames: 0,
                identity: context.identity,
                first: context.listener_position_m,
                last: context.listener_position_m,
                pose_valid: true,
                segment_start: self.next_segment,
                sum: 0.0,
                peak: 0.0,
                nonfinite: 0,
                silence: true,
            });
            self.next_segment = false;
            acc.last = context.listener_position_m;
            acc.pose_valid &= context.listener_position_m.iter().all(|x| x.is_finite());
            match (left.get(i), right.get(i)) {
                (Some(&l), Some(&r)) if l.is_finite() && r.is_finite() => {
                    let (l, r) = (f64::from(l), f64::from(r));
                    acc.sum += l * l + r * r;
                    acc.peak = acc.peak.max(l.abs()).max(r.abs());
                    acc.silence &= l == 0.0 && r == 0.0;
                }
                _ => {
                    acc.nonfinite += 1;
                    acc.silence = false;
                }
            }
            acc.frames += 1;
            self.clock = self.clock.saturating_add(1);
            if acc.frames == self.window_frames {
                self.finish(WindowEnd::Complete);
            }
        }
    }
    /// Call before stream disposal; does not reset frame clock or queued/history data.
    pub fn flush(&mut self) {
        self.finish(WindowEnd::Flushed);
        self.shared.recording.store(false, Ordering::Release);
        self.active_identity = None;
        self.next_segment = true;
    }
    /// Faulted output is unavailable, never fabricated silence. Preserve the clock.
    pub fn skip_frames(&mut self, frames: u64) {
        self.finish(WindowEnd::Flushed);
        self.clock = self.clock.saturating_add(frames);
        self.shared
            .unavailable_output_frames
            .fetch_add(frames, Ordering::Relaxed);
        self.active_identity = None;
        self.next_segment = true;
    }
    fn finish(&mut self, reason: WindowEnd) {
        let Some(a) = self.acc.take() else {
            return;
        };
        let reason_code = match reason {
            WindowEnd::Complete => 0,
            WindowEnd::IdentityChanged => 1,
            WindowEnd::RecordingStopped => 2,
            WindowEnd::Flushed => 3,
        };
        let flags = reason_code
            | ((a.segment_start as u64) << 2)
            | ((a.pose_valid as u64) << 3)
            | ((a.silence as u64) << 4);
        let mut words = [
            self.sequence,
            a.start,
            self.clock,
            a.identity.generation,
            a.identity.epoch,
            flags,
            a.nonfinite,
            a.sum.to_bits(),
            a.peak.to_bits(),
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        for i in 0..3 {
            words[9 + i] = u64::from(a.first[i].to_bits());
            words[12 + i] = u64::from(a.last[i].to_bits());
        }
        self.sequence = self.sequence.saturating_add(1);
        let slot = &self.shared.slots[self.write_index];
        if slot.ready.load(Ordering::Acquire) {
            self.shared.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        for (dst, word) in slot.words.iter().zip(words) {
            dst.store(word, Ordering::Relaxed);
        }
        slot.ready.store(true, Ordering::Release);
        self.write_index = (self.write_index + 1) % self.shared.slots.len();
    }
}

impl LevelTraceReader {
    /// UI thread only. Keeps newest bounded history; queue gaps and history eviction
    /// are different counters. Draining does not reset the recording or identities.
    pub fn drain(&mut self) -> usize {
        let mut count = 0;
        // Bounded work even if the producer runs concurrently.
        for _ in 0..self.shared.slots.len() {
            let slot = &self.shared.slots[self.read_index];
            if !slot.ready.load(Ordering::Acquire) {
                break;
            }
            let w = std::array::from_fn::<_, 15, _>(|i| slot.words[i].load(Ordering::Relaxed));
            slot.ready.store(false, Ordering::Release);
            self.read_index = (self.read_index + 1) % self.shared.slots.len();
            let frames = w[2] - w[1];
            let sample = LevelSample {
                sequence: w[0],
                start_frame: w[1],
                end_frame: w[2],
                identity: TraceIdentity {
                    generation: w[3],
                    epoch: w[4],
                },
                listener_start_m: std::array::from_fn(|i| f32::from_bits(w[9 + i] as u32)),
                listener_end_m: std::array::from_fn(|i| f32::from_bits(w[12 + i] as u32)),
                pose_valid: w[5] & 8 != 0,
                segment_start: w[5] & 4 != 0,
                end_reason: match w[5] & 3 {
                    0 => WindowEnd::Complete,
                    1 => WindowEnd::IdentityChanged,
                    2 => WindowEnd::RecordingStopped,
                    _ => WindowEnd::Flushed,
                },
                exact_silence: w[5] & 16 != 0,
                nonfinite_frames: w[6],
                rms_linear: (w[6] == 0)
                    .then(|| (f64::from_bits(w[7]) / (frames as f64 * 2.0)).sqrt()),
                peak_linear: (w[6] == 0).then(|| f64::from_bits(w[8])),
                gap_before: w[0].saturating_sub(self.next_sequence),
            };
            self.next_sequence = w[0].saturating_add(1);
            if self.retained.len() == self.history_capacity {
                self.retained.pop_front();
                self.evicted += 1;
            }
            self.retained.push_back(sample);
            count += 1;
        }
        count
    }
    pub fn samples(&self) -> &VecDeque<LevelSample> {
        &self.retained
    }
    pub fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }
    pub fn dropped_windows(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }
    pub fn unavailable_output_frames(&self) -> u64 {
        self.shared
            .unavailable_output_frames
            .load(Ordering::Relaxed)
    }
    pub fn evicted_windows(&self) -> u64 {
        self.evicted
    }
    /// Callback acknowledgement, not UI intent.
    pub fn is_recording(&self) -> bool {
        self.shared.recording.load(Ordering::Acquire)
    }
    /// UI must first stop recording and drain its final callback. Refuses active Clear.
    pub fn clear_history(&mut self, recording: bool) -> Result<(), &'static str> {
        if recording || self.is_recording() {
            return Err("stop level recording before clearing history");
        }
        self.drain();
        self.retained.clear();
        self.evicted = 0;
        Ok(())
    }
    /// UI-thread export of drained history; caller attaches scene/mix epoch ledger.
    pub fn export_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version":"fightbox.output-level-trace.v1",
            "signal":"actual final stereo output PCM at caller tap; post original limiter",
            "sample_rate_hz":self.sample_rate_hz,"window_frames":self.window_frames,
            "pose_semantics":"sample-and-held callback listener pose; start/end observed, not interpolated",
            "source_phase":null,"source_phase_note":"unavailable; epoch boundaries are not phase alignment",
            "unavailable_output_frames":self.unavailable_output_frames(),
            "dropped_windows_total":self.dropped_windows(),"evicted_history_windows":self.evicted,
            "samples":self.retained,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ctx(recording: bool, epoch: u64, x: f32) -> BlockContext {
        BlockContext {
            recording,
            identity: TraceIdentity {
                generation: 7,
                epoch,
            },
            listener_position_m: [x, 2.0, 1.5],
        }
    }
    #[test]
    fn stereo_energy_exact_windows_and_held_pose_across_callbacks() {
        let (mut w, mut r) = channel(100, 8, 8).unwrap();
        w.observe(&[0.3; 6], &[0.4; 6], ctx(true, 0, 1.0));
        w.observe(&[0.3; 9], &[0.4; 9], ctx(true, 0, 4.0));
        w.observe(&[], &[], ctx(false, 0, 4.0));
        assert_eq!(r.drain(), 2);
        let a = &r.samples()[0];
        let b = &r.samples()[1];
        assert_eq!(
            (a.start_frame, a.end_frame, b.start_frame, b.end_frame),
            (0, 10, 10, 15)
        );
        assert!((a.rms_linear.unwrap() - (0.125_f64).sqrt()).abs() < 1e-7);
        assert!((a.peak_linear.unwrap() - 0.4).abs() < 1e-7);
        assert_eq!((a.listener_start_m[0], a.listener_end_m[0]), (1.0, 4.0));
        assert_eq!(b.end_reason, WindowEnd::RecordingStopped);
        assert!(b.can_connect_from(a));
        assert!(!r.is_recording());
    }
    #[test]
    fn epoch_generation_restart_and_recording_gaps_break_comparisons() {
        let (mut w, mut r) = channel(100, 16, 16).unwrap();
        w.observe(&[1.0; 4], &[1.0; 4], ctx(true, 0, 0.0));
        w.observe(&[0.5; 3], &[0.5; 3], ctx(true, 1, 0.0));
        w.observe(&[0.0; 2], &[0.0; 2], ctx(false, 1, 0.0));
        w.observe(&[0.5; 2], &[0.5; 2], ctx(true, 1, 0.0));
        let mut new_generation = ctx(true, 1, 0.0);
        new_generation.identity.generation = 8;
        w.observe(&[0.5; 2], &[0.5; 2], new_generation);
        w.flush();
        r.drain();
        assert_eq!(r.samples().len(), 4);
        assert_eq!(r.samples()[0].end_reason, WindowEnd::IdentityChanged);
        assert_eq!(r.samples()[2].start_frame, 9);
        for i in 1..4 {
            assert!(!r.samples()[i].can_connect_from(&r.samples()[i - 1]));
        }
        assert_eq!(r.samples()[3].identity.generation, 8);
    }
    #[test]
    fn bounded_queue_counts_gaps_without_overwriting_and_bounded_history_evicts() {
        let (mut w, mut r) = channel(100, 1, 2).unwrap();
        w.observe(&[0.25; 30], &[0.25; 30], ctx(true, 0, 0.0));
        assert_eq!(r.dropped_windows(), 2);
        r.drain();
        assert_eq!(r.samples()[0].sequence, 0);
        w.observe(&[0.25; 10], &[0.25; 10], ctx(true, 0, 0.0));
        r.drain();
        assert_eq!(r.samples()[1].sequence, 3);
        assert_eq!(r.samples()[1].gap_before, 2);
        assert!(!r.samples()[1].can_connect_from(&r.samples()[0]));
        w.observe(&[0.25; 10], &[0.25; 10], ctx(true, 0, 0.0));
        r.drain();
        assert_eq!(r.samples().len(), 2);
        assert_eq!(r.evicted_windows(), 1);
        assert!(r.clear_history(false).is_err()); // Callback has not acknowledged stop.
        w.flush();
        assert!(r.clear_history(false).is_ok());
        assert!(r.samples().is_empty());
        assert_eq!(r.dropped_windows(), 2); // Lifetime losses are not erased by Clear.
    }
    #[test]
    fn silence_nonfinite_and_malformed_stereo_are_not_fabricated_levels() {
        let (mut w, mut r) = channel(100, 8, 8).unwrap();
        w.observe(&[0.0; 10], &[0.0; 10], ctx(true, 0, 0.0));
        w.observe(&[f32::NAN; 10], &[0.1; 10], ctx(true, 0, 0.0));
        w.observe(&[0.1; 10], &[0.1; 9], ctx(true, 0, 0.0));
        r.drain();
        assert!(r.samples()[0].exact_silence);
        assert_eq!(r.samples()[0].rms_linear, Some(0.0));
        assert_eq!(r.samples()[0].rms_dbfs(), None);
        assert_eq!(r.samples()[1].nonfinite_frames, 10);
        assert_eq!(r.samples()[1].rms_linear, None);
        assert_eq!(r.samples()[2].nonfinite_frames, 1);
        assert_eq!(r.samples()[2].peak_linear, None);
        let exported: serde_json::Value = serde_json::from_str(&r.export_json().unwrap()).unwrap();
        assert!(exported["source_phase"].is_null());
        assert!(exported["samples"][1]["rms_linear"].is_null());
    }
    #[test]
    fn concurrent_single_producer_consumer_preserves_sequences_and_values() {
        let (mut w, mut r) = channel(100, 8, 2048).unwrap();
        let producer = std::thread::spawn(move || {
            for _ in 0..1000 {
                w.observe(&[0.25; 10], &[-0.25; 10], ctx(true, 9, 5.0));
            }
            w.flush();
        });
        while !producer.is_finished() {
            r.drain();
            std::thread::yield_now();
        }
        producer.join().unwrap();
        r.drain();
        assert_eq!(r.samples().len() as u64 + r.dropped_windows(), 1000);
        let mut previous = None;
        for sample in r.samples() {
            assert_eq!(sample.rms_linear, Some(0.25));
            assert_eq!(sample.peak_linear, Some(0.25));
            assert_eq!(sample.start_frame, sample.sequence * 10);
            if let Some(p) = previous {
                assert!(sample.sequence > p);
            }
            previous = Some(sample.sequence);
        }
    }
}

#[cfg(test)]
mod fault_tests {
    use super::*;
    #[test]
    fn unavailable_output_advances_time_without_fabricated_pcm() {
        let (mut w, mut r) = channel(100, 8, 8).unwrap();
        let c = BlockContext {
            recording: true,
            identity: TraceIdentity::default(),
            listener_position_m: [0.0; 3],
        };
        w.observe(&[0.25; 4], &[0.25; 4], c);
        w.skip_frames(7);
        w.observe(&[0.25; 10], &[0.25; 10], c);
        r.drain();
        assert_eq!(r.unavailable_output_frames(), 7);
        assert_eq!(
            (r.samples()[0].start_frame, r.samples()[0].end_frame),
            (0, 4)
        );
        assert_eq!(
            (r.samples()[1].start_frame, r.samples()[1].end_frame),
            (11, 21)
        );
        assert!(!r.samples()[1].can_connect_from(&r.samples()[0]));
        assert_eq!(r.samples()[1].rms_linear, Some(0.25));
    }
}
