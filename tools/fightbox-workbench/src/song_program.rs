//! A latest-song handoff. Decoding and retired-buffer destruction stay on the
//! control thread; the audio thread only adopts a completed bank.

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const BANK_COUNT: usize = 3;
const INDEX_BITS: usize = 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;

pub(crate) struct SongBuffer {
    pub loaded_generation: u64,
    pub mono: Vec<f32>,
    pub stereo: Option<Vec<[f32; 2]>>,
}

struct Shared {
    banks: [UnsafeCell<Option<SongBuffer>>; BANK_COUNT],
    state: AtomicUsize,
}

// SAFETY: there is one writer and one reader. The writer only changes a bank
// that is neither published nor marked reading; the reader marks ownership in
// the same atomic state before borrowing a bank and never changes its storage.
unsafe impl Sync for Shared {}

fn pack(published: usize, reading: usize) -> usize {
    published | (reading << INDEX_BITS)
}

fn published(state: usize) -> usize {
    state & INDEX_MASK
}

fn reading(state: usize) -> usize {
    (state >> INDEX_BITS) & INDEX_MASK
}

/// Retain this owner until the audio output (and its reader) is destroyed, so
/// the final shared buffers are also freed on the control thread.
pub(crate) struct SongWriter {
    shared: Arc<Shared>,
}

pub(crate) struct SongReader {
    shared: Arc<Shared>,
    reading_slot: usize,
}

pub(crate) fn song_channel() -> (SongWriter, SongReader) {
    let shared = Arc::new(Shared {
        banks: std::array::from_fn(|_| UnsafeCell::new(None)),
        state: AtomicUsize::new(pack(0, 0)),
    });
    (
        SongWriter {
            shared: Arc::clone(&shared),
        },
        SongReader {
            shared,
            reading_slot: 0,
        },
    )
}

impl SongWriter {
    /// Releases retired programs during a control-thread tick, including the
    /// old reading bank after the callback has adopted its replacement.
    pub(crate) fn reclaim(&mut self) {
        let state = self.shared.state.load(Ordering::Acquire);
        for slot in 0..BANK_COUNT {
            if slot != published(state) && slot != reading(state) {
                // SAFETY: the only reader can move to the published bank, so
                // every bank selected here stays free until this writer acts.
                unsafe { *self.shared.banks[slot].get() = None };
            }
        }
    }

    /// Moves a fully decoded, preallocated program into the latest-song slot.
    /// Any superseded program in the free bank is dropped here, never by the
    /// callback. No reader acknowledgement is needed before returning.
    pub(crate) fn publish(&mut self, buffer: SongBuffer) {
        self.publish_bank(Some(buffer));
    }

    pub(crate) fn clear(&mut self) {
        self.publish_bank(None);
    }

    fn publish_bank(&mut self, buffer: Option<SongBuffer>) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        let write_slot = (0..BANK_COUNT)
            .find(|slot| *slot != published(state) && *slot != reading(state))
            .expect("three song banks leave one control-thread slot");
        // SAFETY: this bank is neither the published bank the reader may adopt
        // nor the bank it currently reads. Until this writer publishes, the
        // reader can only change ownership to the existing published bank.
        unsafe { *self.shared.banks[write_slot].get() = buffer };
        loop {
            match self.shared.state.compare_exchange(
                state,
                pack(write_slot, reading(state)),
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                // Only the reader can have changed state, and the write bank
                // remains free. Retrying is confined to the control thread.
                Err(observed) => state = observed,
            }
        }
    }
}

impl SongReader {
    /// Adopts at most once per block. A racing publication is deferred until
    /// the next block rather than spinning on the audio thread.
    pub(crate) fn adopt_latest(&mut self) -> bool {
        let state = self.shared.state.load(Ordering::Acquire);
        let published_slot = published(state);
        if published_slot == self.reading_slot {
            return false;
        }
        if self
            .shared
            .state
            .compare_exchange(
                state,
                pack(published_slot, published_slot),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        self.reading_slot = published_slot;
        true
    }

    pub(crate) fn buffer(&self) -> Option<&SongBuffer> {
        // SAFETY: this slot stays marked reading until a later adoption. A
        // borrow of self prevents that adoption while this reference is live.
        unsafe { &*self.shared.banks[self.reading_slot].get() }.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballistic_crack::tests::count_allocator_calls;

    fn program(value: f32) -> SongBuffer {
        SongBuffer {
            loaded_generation: 0,
            mono: vec![value; 8],
            stereo: Some(vec![[value, -value]; 8]),
        }
    }

    #[test]
    fn latest_song_wins_without_touching_the_reading_bank() {
        let (mut writer, mut reader) = song_channel();
        assert!(reader.buffer().is_none());
        assert!(!reader.adopt_latest());
        writer.publish(program(1.0));
        assert!(reader.adopt_latest());
        assert!(!reader.adopt_latest());

        let retained = reader.buffer().unwrap();
        let retained_samples = retained.mono.as_ptr();
        for value in 2..=20 {
            writer.publish(program(value as f32));
            assert_eq!(retained.mono, [1.0; 8]);
            assert_eq!(retained.stereo.as_ref().unwrap(), &[[1.0, -1.0]; 8]);
        }
        assert_eq!(reader.buffer().unwrap().mono.as_ptr(), retained_samples);
        assert!(reader.adopt_latest());
        assert_eq!(reader.buffer().unwrap().mono, [20.0; 8]);
        assert!(!reader.adopt_latest());
    }

    #[test]
    fn audio_adoption_does_not_allocate_or_free_programs() {
        let (mut writer, mut reader) = song_channel();
        for value in 1..=8 {
            writer.publish(program(value as f32));
            let calls = count_allocator_calls(|| {
                assert!(reader.adopt_latest());
                let buffer = reader.buffer().unwrap();
                assert_eq!(buffer.mono[0], value as f32);
                assert_eq!(
                    buffer.stereo.as_ref().unwrap()[0],
                    [value as f32, -(value as f32)]
                );
                assert!(!reader.adopt_latest());
            });
            assert_eq!(calls, (0, 0));
        }
        let next = program(9.0);
        let (_, frees) = count_allocator_calls(|| writer.publish(next));
        assert_eq!(
            frees, 2,
            "the control thread frees the retired mono and stereo vectors"
        );
        assert_eq!(count_allocator_calls(|| writer.reclaim()), (0, 0));
        assert_eq!(reader.buffer().unwrap().mono, [8.0; 8]);
        assert_eq!(
            count_allocator_calls(|| assert!(reader.adopt_latest())),
            (0, 0)
        );
        assert_eq!(count_allocator_calls(|| writer.reclaim()), (0, 2));
        assert_eq!(reader.buffer().unwrap().mono, [9.0; 8]);
    }
}
