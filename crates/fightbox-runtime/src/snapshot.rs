//! A bounded SPSC triple buffer for immutable snapshots.

use std::cell::UnsafeCell;
use std::hint::spin_loop;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const SLOT_COUNT: usize = 3;
const INDEX_BITS: usize = 2;
const INDEX_MASK: usize = (1 << INDEX_BITS) - 1;
const MAX_READ_ATTEMPTS: usize = 3;

const fn pack(published: usize, reading: usize) -> usize {
    published | (reading << INDEX_BITS)
}

const fn published(state: usize) -> usize {
    state & INDEX_MASK
}

const fn reading(state: usize) -> usize {
    (state >> INDEX_BITS) & INDEX_MASK
}

struct Shared<T: Copy> {
    slots: [UnsafeCell<T>; SLOT_COUNT],
    state: AtomicUsize,
}

// SAFETY: access to each slot is governed by `state`. There is exactly one
// writer and one reader. The writer only mutates a slot that is neither
// published nor marked as being read. The reader only copies the slot it first
// marks as being read with a successful compare-exchange.
unsafe impl<T: Copy + Send> Sync for Shared<T> {}

/// Factory for a single-writer, single-reader immutable snapshot channel.
pub struct SnapshotPublication;

impl SnapshotPublication {
    /// Allocates the three slots once and returns unique writer and reader ends.
    #[must_use]
    pub fn new<T: Copy + Send>(initial: T) -> (SnapshotWriter<T>, SnapshotReader<T>) {
        let shared = Arc::new(Shared {
            slots: std::array::from_fn(|_| UnsafeCell::new(initial)),
            state: AtomicUsize::new(pack(0, 0)),
        });
        (
            SnapshotWriter {
                shared: Arc::clone(&shared),
            },
            SnapshotReader {
                shared,
                last: initial,
            },
        )
    }
}

/// Unique producer end of a snapshot publication channel.
pub struct SnapshotWriter<T: Copy + Send> {
    shared: Arc<Shared<T>>,
}

impl<T: Copy + Send> SnapshotWriter<T> {
    /// Publishes a complete value. This may retry if it races the reader's
    /// index transition, but it never allocates.
    pub fn publish(&mut self, value: T) {
        let mut state = self.shared.state.load(Ordering::Acquire);
        loop {
            let published_slot = published(state);
            let reading_slot = reading(state);
            let write_slot = (0..SLOT_COUNT)
                .find(|slot| *slot != published_slot && *slot != reading_slot)
                .expect("three slots always leave one writer slot");

            // SAFETY: `write_slot` was observed as neither published nor read.
            // If the reader changes state concurrently, it can only move to
            // `published_slot`, never to `write_slot`.
            unsafe {
                *self.shared.slots[write_slot].get() = value;
            }

            let next = pack(write_slot, reading_slot);
            match self.shared.state.compare_exchange(
                state,
                next,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(observed) => {
                    state = observed;
                    spin_loop();
                }
            }
        }
    }
}

/// Unique consumer end of a snapshot publication channel.
pub struct SnapshotReader<T: Copy + Send> {
    shared: Arc<Shared<T>>,
    last: T,
}

impl<T: Copy + Send> SnapshotReader<T> {
    /// Returns one fully published snapshot.
    ///
    /// The operation performs at most three compare-exchanges. If a producer
    /// wins every race, the reader returns its retained last complete value.
    /// That bounded fallback makes this wait-free for a real-time callback.
    pub fn read(&mut self) -> T {
        for _ in 0..MAX_READ_ATTEMPTS {
            let state = self.shared.state.load(Ordering::Acquire);
            let published_slot = published(state);
            if published_slot == reading(state) {
                return self.last;
            }
            let next = pack(published_slot, published_slot);
            if self
                .shared
                .state
                .compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                // SAFETY: the successful transition marks this slot as being
                // read. The writer excludes it until a later reader transition.
                self.last = unsafe { *self.shared.slots[published_slot].get() };
                return self.last;
            }
            spin_loop();
        }
        self.last
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    const WORDS: usize = 64;

    #[derive(Clone, Copy)]
    struct StressSnapshot {
        generation: u64,
        payload: [u64; WORDS],
    }

    impl StressSnapshot {
        fn generation(generation: u64) -> Self {
            Self {
                generation,
                payload: [generation; WORDS],
            }
        }

        fn is_complete(self) -> bool {
            self.payload.iter().all(|word| *word == self.generation)
        }
    }

    #[test]
    fn concurrent_reader_never_observes_a_torn_generation() {
        let (mut writer, mut reader) = SnapshotPublication::new(StressSnapshot::generation(0));
        let done = Arc::new(AtomicBool::new(false));
        let writer_done = Arc::clone(&done);
        let writer_thread = thread::spawn(move || {
            for generation in 1..=250_000 {
                writer.publish(StressSnapshot::generation(generation));
            }
            writer_done.store(true, Ordering::Release);
        });

        let mut reads = 0;
        while !done.load(Ordering::Acquire) || reads < 250_000 {
            assert!(reader.read().is_complete());
            reads += 1;
        }
        writer_thread.join().unwrap();
    }
}
