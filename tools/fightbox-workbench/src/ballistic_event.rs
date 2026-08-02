//! Allocation-free audio-thread adoption of trigger-time ballistic stems.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use fightbox_api::AssetAnalysis;
use fightbox_evidence::{WavSpec, analyze_decoded_asset};

const BANK_COUNT: usize = 3;
const INDEX_BITS: usize = 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;
const ARTILLERY_PROGRAM_SECONDS: f64 = 2.0;
const ARTILLERY_FADE_SECONDS: f64 = 0.05;
const ONSET_FRACTION_OF_PEAK: f32 = 1.0e-3;

fn pack(published: usize, reading: usize) -> usize {
    published | (reading << INDEX_BITS)
}

fn published(state: usize) -> usize {
    state & INDEX_MASK
}

fn reading(state: usize) -> usize {
    (state >> INDEX_BITS) & INDEX_MASK
}

struct StemBank {
    generation: u64,
    crack_active: bool,
    crack: Vec<f32>,
    blast: Vec<f32>,
}

struct Shared {
    banks: [UnsafeCell<StemBank>; BANK_COUNT],
    state: AtomicUsize,
    completed_generation: AtomicU64,
}

// SAFETY: the single writer mutates only a bank that is neither published nor
// marked reading. The single audio reader changes its marked bank through the
// atomic state before reading and never mutates sample storage.
unsafe impl Sync for Shared {}

pub(crate) struct EventStemWriter {
    shared: Arc<Shared>,
}

pub(crate) struct EventStemReader {
    shared: Arc<Shared>,
    reading_slot: usize,
    generation: u64,
    cursor: usize,
}

pub(crate) fn event_stem_channel(program_frames: usize) -> (EventStemWriter, EventStemReader) {
    let shared = Arc::new(Shared {
        banks: std::array::from_fn(|_| {
            UnsafeCell::new(StemBank {
                generation: 0,
                crack_active: false,
                crack: vec![0.0; program_frames],
                blast: vec![0.0; program_frames],
            })
        }),
        state: AtomicUsize::new(pack(0, 0)),
        completed_generation: AtomicU64::new(0),
    });
    (
        EventStemWriter {
            shared: Arc::clone(&shared),
        },
        EventStemReader {
            shared,
            reading_slot: 0,
            generation: 0,
            cursor: 0,
        },
    )
}

impl EventStemWriter {
    pub(crate) fn publish(
        &mut self,
        generation: u64,
        crack_active: bool,
        crack: &[f32],
        blast: &[f32],
    ) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        loop {
            let published_slot = published(state);
            let reading_slot = reading(state);
            let write_slot = (0..BANK_COUNT)
                .find(|slot| *slot != published_slot && *slot != reading_slot)
                .expect("three event banks leave one control-thread slot");
            // SAFETY: this bank is neither the published bank a reader may
            // adopt nor the bank currently marked as being read.
            let bank = unsafe { &mut *self.shared.banks[write_slot].get() };
            assert_eq!(bank.crack.len(), crack.len());
            assert_eq!(bank.blast.len(), blast.len());
            bank.crack.copy_from_slice(crack);
            bank.blast.copy_from_slice(blast);
            bank.generation = generation;
            bank.crack_active = crack_active;

            let next = pack(write_slot, reading_slot);
            match self.shared.state.compare_exchange(
                state,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => state = observed,
            }
        }
    }

    pub(crate) fn completed_generation(&self) -> u64 {
        self.shared.completed_generation.load(Ordering::Acquire)
    }
}

impl EventStemReader {
    /// Adopts a complete pair once per audio block. Both roles therefore start
    /// from cursor zero in the same callback, with no activation scheduler.
    pub(crate) fn begin_block(&mut self) {
        let state = self.shared.state.load(Ordering::Acquire);
        let published_slot = published(state);
        if published_slot == self.reading_slot {
            return;
        }
        let next = pack(published_slot, published_slot);
        if self
            .shared
            .state
            .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.reading_slot = published_slot;
            // SAFETY: the successful state transition marks this bank as read,
            // excluding it from the writer until a later adoption.
            let bank = unsafe { &*self.shared.banks[published_slot].get() };
            self.generation = bank.generation;
            self.cursor = 0;
        }
    }

    pub(crate) fn fill_crack(&self, output: &mut [f32]) {
        let bank = self.bank();
        if !bank.crack_active {
            output.fill(0.0);
            return;
        }
        fill_from_cursor(&bank.crack, self.cursor, output);
    }

    pub(crate) fn fill_blast(&self, output: &mut [f32]) {
        fill_from_cursor(&self.bank().blast, self.cursor, output);
    }

    pub(crate) fn end_block(&mut self, frames: usize) {
        if self.generation == 0 {
            return;
        }
        self.cursor = self.cursor.saturating_add(frames);
        if self.cursor >= self.bank().blast.len() {
            self.shared
                .completed_generation
                .store(self.generation, Ordering::Release);
        }
    }

    fn bank(&self) -> &StemBank {
        // SAFETY: `reading_slot` is marked in `state` for the lifetime of this
        // reader epoch, so the writer cannot select it.
        unsafe { &*self.shared.banks[self.reading_slot].get() }
    }
}

fn fill_from_cursor(signal: &[f32], cursor: usize, output: &mut [f32]) {
    output.fill(0.0);
    let available = signal.len().saturating_sub(cursor).min(output.len());
    if available > 0 {
        output[..available].copy_from_slice(&signal[cursor..cursor + available]);
    }
}

/// Rotates the isolated artillery onset to the requested embedded-silence
/// frame, crops the signed two-second program, and applies only its 50 ms tail
/// fade. It never applies a level gain.
pub(crate) fn generate_blast_stem(
    asset: &[f32],
    sample_rate_hz: u32,
    program_frames: usize,
    embedded_leading_frames: usize,
) -> Result<(Vec<f32>, AssetAnalysis), String> {
    if asset.is_empty() || sample_rate_hz == 0 {
        return Err("ballistic blast asset is empty or has an invalid sample rate".into());
    }
    let peak = asset
        .iter()
        .map(|sample| sample.abs())
        .fold(0.0_f32, f32::max);
    if peak == 0.0 {
        return Err("ballistic blast asset is silent".into());
    }
    // This is the signed strip's isolated probe: a threshold relative to the
    // asset's own peak, rather than a naive first-nonzero scan that mistakes
    // decoder residue for the physical onset.
    let onset = asset
        .iter()
        .position(|sample| sample.abs() >= peak * ONSET_FRACTION_OF_PEAK)
        .ok_or("ballistic blast asset has no isolated onset")?;
    let audible_frames = (ARTILLERY_PROGRAM_SECONDS * f64::from(sample_rate_hz)) as usize;
    if embedded_leading_frames.saturating_add(audible_frames) > program_frames {
        return Err("ballistic blast program does not fit the event stem".into());
    }
    let mut stem = vec![0.0; program_frames];
    let target =
        &mut stem[embedded_leading_frames..embedded_leading_frames.saturating_add(audible_frames)];
    for (frame, sample) in target.iter_mut().enumerate() {
        *sample = asset.get(onset + frame).copied().unwrap_or(0.0);
    }
    let fade_frames = (ARTILLERY_FADE_SECONDS * f64::from(sample_rate_hz)) as usize;
    let fade_start = target.len().saturating_sub(fade_frames);
    for (offset, sample) in target[fade_start..].iter_mut().enumerate() {
        let phase = std::f32::consts::FRAC_PI_2 * offset as f32 / fade_frames.max(1) as f32;
        *sample *= phase.cos().powi(2);
    }
    // Calibrate against the exact audible event program, not the source WAV's
    // twelve-second padded-loop RMS. Embedded silence is transport timing and
    // is deliberately outside this power measurement.
    let analysis = analyze_decoded_asset(
        WavSpec {
            sample_rate_hz,
            channels: 1,
        },
        target,
    )
    .map_err(|error| format!("cannot analyze ballistic blast stem: {}", error.as_str()))?
    .into_parts()
    .0;
    Ok((stem, analysis))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_adoption_restarts_both_roles_at_one_audio_boundary() {
        let (mut writer, mut reader) = event_stem_channel(8);
        writer.publish(1, true, &[1.0; 8], &[2.0; 8]);
        reader.begin_block();
        let mut crack = [0.0; 4];
        let mut blast = [0.0; 4];
        reader.fill_crack(&mut crack);
        reader.fill_blast(&mut blast);
        assert_eq!(crack, [1.0; 4]);
        assert_eq!(blast, [2.0; 4]);
        reader.end_block(4);
        assert_eq!(writer.completed_generation(), 0);
        reader.end_block(4);
        assert_eq!(writer.completed_generation(), 1);
    }

    #[test]
    fn out_of_cone_bank_is_blast_only() {
        let (mut writer, mut reader) = event_stem_channel(4);
        writer.publish(7, false, &[1.0; 4], &[2.0; 4]);
        reader.begin_block();
        let mut crack = [9.0; 4];
        let mut blast = [9.0; 4];
        reader.fill_crack(&mut crack);
        reader.fill_blast(&mut blast);
        assert_eq!(crack, [0.0; 4]);
        assert_eq!(blast, [2.0; 4]);
    }

    #[test]
    fn blast_alignment_ignores_numerical_residue_before_the_probe_onset() {
        let mut asset = vec![1.0e-8; 200_000];
        asset[1_000] = 1.0;
        let (stem, analysis) = generate_blast_stem(&asset, 48_000, 144_000, 0).unwrap();
        assert_eq!(stem[0].to_bits(), 1.0_f32.to_bits());
        assert!(stem[..1].iter().all(|sample| *sample != 1.0e-8));
        assert!(analysis.program_rms_dbfs.is_finite());
        assert!(analysis.true_peak_dbtp.is_finite());
    }

    #[test]
    fn real_artillery_calibration_measures_the_cropped_event_program() {
        let asset = crate::asset::load_asset("artillery-impact").unwrap();
        let (_, analysis) = generate_blast_stem(&asset.samples, 48_000, 144_000, 0).unwrap();

        assert!(analysis.program_rms_dbfs > asset.analysis.program_rms_dbfs);
        eprintln!(
            "ballistic_blast_calibration original_loop_rms={:.4}dBFS event_program_rms={:.4}dBFS event_true_peak={:.4}dBTP",
            asset.analysis.program_rms_dbfs, analysis.program_rms_dbfs, analysis.true_peak_dbtp,
        );
    }
}
