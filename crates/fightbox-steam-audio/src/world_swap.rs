//! Bounded, allocation-free ownership handoff for prepared render generations.
//!
//! The channel is single-producer/single-consumer and has one preallocated
//! slot. It deliberately moves whole owned generations instead of publishing
//! borrowed SDK handles. The audio consumer never waits: a contested or empty
//! slot is simply retried at the next block boundary.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

const EMPTY: u8 = 0;
const WRITING: u8 = 1;
const FULL: u8 = 2;
const READING: u8 = 3;

struct Shared<T> {
    value: UnsafeCell<MaybeUninit<T>>,
    state: AtomicU8,
}

// SAFETY: the unique producer and consumer ends are the only routes to the
// slot. `state` grants exclusive write/read access, and release/acquire
// ordering publishes a completely initialized value.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        if *self.state.get_mut() == FULL {
            // SAFETY: FULL means the producer initialized the slot and no
            // consumer has moved the value out.
            unsafe {
                self.value.get_mut().assume_init_drop();
            }
        }
    }
}

pub(crate) struct Producer<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Producer<T> {
    pub(crate) fn try_push(&mut self, value: T) -> Result<(), T> {
        if self
            .shared
            .state
            .compare_exchange(EMPTY, WRITING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(value);
        }
        // SAFETY: the successful transition from EMPTY grants this producer
        // exclusive access until it publishes FULL.
        unsafe {
            (*self.shared.value.get()).write(value);
        }
        self.shared.state.store(FULL, Ordering::Release);
        Ok(())
    }
}

pub(crate) struct Consumer<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Consumer<T> {
    pub(crate) fn try_pop(&mut self) -> Option<T> {
        self.shared
            .state
            .compare_exchange(FULL, READING, Ordering::Acquire, Ordering::Relaxed)
            .ok()?;
        // SAFETY: the successful transition from FULL grants this consumer
        // exclusive access to the initialized value.
        let value = unsafe { (*self.shared.value.get()).assume_init_read() };
        self.shared.state.store(EMPTY, Ordering::Release);
        Some(value)
    }
}

pub(crate) fn channel<T>() -> (Producer<T>, Consumer<T>) {
    let shared = Arc::new(Shared {
        value: UnsafeCell::new(MaybeUninit::uninit()),
        state: AtomicU8::new(EMPTY),
    });
    (
        Producer {
            shared: Arc::clone(&shared),
        },
        Consumer { shared },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Tracked {
        generation: u64,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn moved_generation_is_not_dropped_until_the_consumer_releases_it() {
        let drops = Arc::new(AtomicUsize::new(0));
        let (mut producer, mut consumer) = channel();
        producer
            .try_push(Tracked {
                generation: 7,
                drops: Arc::clone(&drops),
            })
            .ok()
            .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        let in_flight = consumer.try_pop().unwrap();
        assert_eq!(in_flight.generation, 7);
        drop(producer);
        drop(consumer);
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        drop(in_flight);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn full_slot_rejects_a_new_generation_without_losing_either_value() {
        let (mut producer, mut consumer) = channel();
        producer.try_push(11_u64).unwrap();
        assert_eq!(producer.try_push(12), Err(12));
        assert_eq!(consumer.try_pop(), Some(11));
        assert_eq!(consumer.try_pop(), None);
    }
}
