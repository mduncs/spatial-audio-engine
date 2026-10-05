//! Session-owned monotonic timing for realtime callback measurements.
//!
//! On Apple platforms, [`RealtimeClock::new`] captures and validates the Mach
//! timebase, then reads `mach_absolute_time()` once to resolve and warm the
//! counter before the clock can enter a callback. [`RealtimeClock::start`] and
//! [`RealtimeClock::elapsed_ns`] subsequently call only that counter and use
//! integer arithmetic. There is no global clock or lazy initialization.
//!
//! Rust 1.91.1 implements Apple `std::time::Instant` with
//! `clock_gettime(CLOCK_UPTIME_RAW)`. Rust documents that clock as having the
//! same sleep behavior and value domain as converted `mach_absolute_time()`:
//! <https://github.com/rust-lang/rust/blob/1.91.1/library/std/src/sys/pal/unix/time.rs#L237-L258>.
//! Non-Apple targets retain `Instant` as the portable fallback.
//!
//! Apple identifies direct `mach_absolute_time()` use as a required-reason
//! API. An app embedding this crate must add the System Boot Time category
//! (`NSPrivacyAccessedAPICategorySystemBootTime`) and elapsed-time reason
//! `35F9.1` to its `PrivacyInfo.xcprivacy`. Only the derived callback duration,
//! never the raw counter or a derived absolute boot time, may leave the device:
//! <https://developer.apple.com/documentation/kernel/1462446-mach_absolute_time>
//! and <https://developer.apple.com/documentation/bundleresources/app-privacy-configuration/nsprivacyaccessedapitypes/nsprivacyaccessedapitype>.

use core::fmt;
#[cfg(any(target_vendor = "apple", test))]
use core::num::NonZeroU32;

#[cfg(not(target_vendor = "apple"))]
use std::time::Instant;

/// Failure to initialize a realtime counter on the control thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealtimeClockError {
    /// The operating system rejected the counter-scale query.
    CounterScaleQueryFailed { status: i32 },
    /// The operating system returned a scale that cannot represent time.
    InvalidCounterScale { numerator: u32, denominator: u32 },
}

impl fmt::Display for RealtimeClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::CounterScaleQueryFailed { status } => {
                write!(
                    formatter,
                    "realtime counter scale query failed with status {status}"
                )
            }
            Self::InvalidCounterScale {
                numerator,
                denominator,
            } => write!(
                formatter,
                "realtime counter returned invalid scale {numerator}/{denominator}"
            ),
        }
    }
}

impl std::error::Error for RealtimeClockError {}

/// A preinitialized monotonic clock owned by one runtime or output session.
///
/// Construct this on the control thread and move it into callback-owned state.
/// Construction is the only operation that may query platform clock metadata.
#[derive(Debug)]
pub struct RealtimeClock {
    #[cfg(target_vendor = "apple")]
    scale: CounterScale,
}

/// An opaque callback start marker produced by [`RealtimeClock::start`].
///
/// A marker must be consumed by the same clock instance that produced it.
#[derive(Clone, Copy, Debug)]
pub struct RealtimeTimestamp {
    #[cfg(target_vendor = "apple")]
    ticks: u64,
    #[cfg(not(target_vendor = "apple"))]
    instant: Instant,
}

impl RealtimeClock {
    /// Initializes a session clock outside the realtime callback.
    ///
    /// On Apple this queries and validates `mach_timebase_info`, then performs
    /// one priming counter read. Other targets have no fallible metadata query.
    pub fn new() -> Result<Self, RealtimeClockError> {
        #[cfg(target_vendor = "apple")]
        {
            let scale = apple::counter_scale()?;

            // Resolve the symbol and any platform-side first-use work before
            // the clock is moved into realtime callback state.
            let _primed_ticks = apple::absolute_time();
            Ok(Self { scale })
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            Ok(Self {})
        }
    }

    /// Captures the beginning of a measured callback interval.
    ///
    /// After [`Self::new`], this method neither allocates nor initializes
    /// process-global state.
    #[must_use]
    #[inline(always)]
    pub fn start(&self) -> RealtimeTimestamp {
        #[cfg(target_vendor = "apple")]
        {
            RealtimeTimestamp {
                ticks: apple::absolute_time(),
            }
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            RealtimeTimestamp {
                instant: Instant::now(),
            }
        }
    }

    /// Returns elapsed nanoseconds, saturating at [`u64::MAX`].
    ///
    /// Apple counter subtraction is wrapping so an interval crossing the
    /// counter's single wrap remains correct. A callback cannot span more than
    /// one full `u64` counter period. Conversion avoids `u128` division and its
    /// possible compiler runtime helper.
    ///
    /// The following executable allocation check runs in its own doctest crate
    /// so it can install an observing allocator without competing with the
    /// runtime crate's unit-test allocator.
    ///
    /// ```
    /// use fightbox_runtime::RealtimeClock;
    /// use std::alloc::{GlobalAlloc, Layout, System};
    /// use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    ///
    /// struct CountingAllocator;
    /// static TRACKING: AtomicBool = AtomicBool::new(false);
    /// static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
    ///
    /// unsafe impl GlobalAlloc for CountingAllocator {
    ///     unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
    ///         if TRACKING.load(Ordering::Relaxed) {
    ///             ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ///         }
    ///         unsafe { System.alloc(layout) }
    ///     }
    ///
    ///     unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
    ///         unsafe { System.dealloc(pointer, layout) }
    ///     }
    ///
    ///     unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
    ///         if TRACKING.load(Ordering::Relaxed) {
    ///             ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ///         }
    ///         unsafe { System.alloc_zeroed(layout) }
    ///     }
    ///
    ///     unsafe fn realloc(
    ///         &self,
    ///         pointer: *mut u8,
    ///         layout: Layout,
    ///         new_size: usize,
    ///     ) -> *mut u8 {
    ///         if TRACKING.load(Ordering::Relaxed) {
    ///             ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    ///         }
    ///         unsafe { System.realloc(pointer, layout, new_size) }
    ///     }
    /// }
    ///
    /// #[global_allocator]
    /// static ALLOCATOR: CountingAllocator = CountingAllocator;
    ///
    /// let clock = RealtimeClock::new().expect("platform clock must initialize");
    /// let _ = clock.elapsed_ns(clock.start());
    /// ALLOCATIONS.store(0, Ordering::Relaxed);
    /// TRACKING.store(true, Ordering::Relaxed);
    /// let mut observed = 0;
    /// for _ in 0..1_024 {
    ///     let started = clock.start();
    ///     observed ^= clock.elapsed_ns(started);
    /// }
    /// TRACKING.store(false, Ordering::Relaxed);
    /// std::hint::black_box(observed);
    /// assert_eq!(ALLOCATIONS.load(Ordering::Relaxed), 0);
    /// ```
    #[must_use]
    #[inline(always)]
    pub fn elapsed_ns(&self, started: RealtimeTimestamp) -> u64 {
        #[cfg(target_vendor = "apple")]
        {
            let elapsed_ticks = apple::absolute_time().wrapping_sub(started.ticks);
            self.scale.ticks_to_ns(elapsed_ticks)
        }

        #[cfg(not(target_vendor = "apple"))]
        {
            saturating_u128_to_u64(started.instant.elapsed().as_nanos())
        }
    }
}

#[cfg(any(target_vendor = "apple", test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CounterScale {
    numerator: NonZeroU32,
    denominator: NonZeroU32,
}

#[cfg(any(target_vendor = "apple", test))]
impl CounterScale {
    fn new(numerator: u32, denominator: u32) -> Result<Self, RealtimeClockError> {
        let Some(numerator_nonzero) = NonZeroU32::new(numerator) else {
            return Err(RealtimeClockError::InvalidCounterScale {
                numerator,
                denominator,
            });
        };
        let Some(denominator_nonzero) = NonZeroU32::new(denominator) else {
            return Err(RealtimeClockError::InvalidCounterScale {
                numerator,
                denominator,
            });
        };
        Ok(Self {
            numerator: numerator_nonzero,
            denominator: denominator_nonzero,
        })
    }

    #[inline(always)]
    fn ticks_to_ns(self, ticks: u64) -> u64 {
        let numerator = u64::from(self.numerator.get());
        let denominator = u64::from(self.denominator.get());
        if numerator == denominator {
            return ticks;
        }
        let whole = ticks / denominator;
        let remainder = ticks % denominator;

        whole
            .saturating_mul(numerator)
            .saturating_add(remainder * numerator / denominator)
    }
}

#[cfg(not(target_vendor = "apple"))]
#[inline(always)]
fn saturating_u128_to_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(target_vendor = "apple")]
mod apple {
    use super::{CounterScale, RealtimeClockError};

    const KERN_SUCCESS: i32 = 0;

    #[repr(C)]
    struct MachTimebaseInfo {
        numerator: u32,
        denominator: u32,
    }

    #[link(name = "System")]
    unsafe extern "C" {
        fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
        fn mach_absolute_time() -> u64;
    }

    pub(super) fn counter_scale() -> Result<CounterScale, RealtimeClockError> {
        let mut info = MachTimebaseInfo {
            numerator: 0,
            denominator: 0,
        };
        // SAFETY: `info` is a valid, aligned out-pointer matching Apple's
        // `mach_timebase_info_data_t` layout and remains alive for the call.
        let status = unsafe { mach_timebase_info(&mut info) };
        if status != KERN_SUCCESS {
            return Err(RealtimeClockError::CounterScaleQueryFailed { status });
        }
        CounterScale::new(info.numerator, info.denominator)
    }

    #[must_use]
    #[inline(always)]
    pub(super) fn absolute_time() -> u64 {
        // SAFETY: this takes no arguments and returns Apple's monotonic host
        // counter. Construction already performed the metadata query and one
        // priming read outside the callback.
        unsafe { mach_absolute_time() }
    }

    #[cfg(test)]
    pub(super) fn linked_symbol_addresses() -> (usize, usize) {
        (mach_timebase_info as usize, mach_absolute_time as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}

    #[test]
    fn clock_and_timestamp_can_move_to_callback_ownership() {
        assert_send::<RealtimeClock>();
        assert_sync::<RealtimeClock>();
        assert_send::<RealtimeTimestamp>();
        assert_sync::<RealtimeTimestamp>();
        assert!(!core::mem::needs_drop::<RealtimeClock>());
        assert!(!core::mem::needs_drop::<RealtimeTimestamp>());
    }

    #[test]
    fn counter_conversion_is_exact_without_overflow() {
        let scale = CounterScale::new(125, 3).unwrap();
        assert_eq!(scale.ticks_to_ns(0), 0);
        assert_eq!(scale.ticks_to_ns(3), 125);
        assert_eq!(scale.ticks_to_ns(9), 375);

        let unity = CounterScale::new(u32::MAX, u32::MAX).unwrap();
        assert_eq!(unity.ticks_to_ns(u64::MAX), u64::MAX);

        let reducing = CounterScale::new(3, 125).unwrap();
        let expected = (u128::from(u64::MAX) * 3 / 125) as u64;
        assert_eq!(reducing.ticks_to_ns(u64::MAX), expected);
    }

    #[test]
    fn counter_conversion_saturates_large_scales() {
        let scale = CounterScale::new(u32::MAX, 1).unwrap();
        assert_eq!(scale.ticks_to_ns(u64::MAX), u64::MAX);
        assert_eq!(scale.ticks_to_ns(u64::MAX - 1), u64::MAX);
    }

    #[test]
    fn counter_delta_survives_one_wrap() {
        let started = u64::MAX - 5;
        let finished = 3_u64;
        let elapsed_ticks = finished.wrapping_sub(started);
        assert_eq!(elapsed_ticks, 9);
        assert_eq!(
            CounterScale::new(1, 1).unwrap().ticks_to_ns(elapsed_ticks),
            9
        );
    }

    #[test]
    fn invalid_counter_scales_are_rejected_before_callback_use() {
        assert_eq!(
            CounterScale::new(0, 1),
            Err(RealtimeClockError::InvalidCounterScale {
                numerator: 0,
                denominator: 1,
            })
        );
        assert_eq!(
            CounterScale::new(1, 0),
            Err(RealtimeClockError::InvalidCounterScale {
                numerator: 1,
                denominator: 0,
            })
        );
    }

    #[test]
    fn elapsed_time_advances_monotonically() {
        let clock = RealtimeClock::new().unwrap();
        let started = clock.start();
        std::thread::sleep(Duration::from_millis(2));
        let first = clock.elapsed_ns(started);
        let second = clock.elapsed_ns(started);
        assert!(first > 0);
        assert!(second >= first);
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn apple_mach_symbols_link_and_timebase_is_available() {
        let (timebase_symbol, counter_symbol) = apple::linked_symbol_addresses();
        assert_ne!(timebase_symbol, 0);
        assert_ne!(counter_symbol, 0);
        assert!(apple::counter_scale().is_ok());
        let first = apple::absolute_time();
        let second = apple::absolute_time();
        assert!(second >= first);
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn duration_conversion_saturates() {
        assert_eq!(saturating_u128_to_u64(Duration::MAX.as_nanos()), u64::MAX);
    }
}
