//! Bake memory guard. Steam Audio's path bake holds per-pair path state while
//! it searches, so its peak footprint grows with the probe pairs inside the
//! path range (every pair once the range spans the scene), not with the
//! serialized artifact. The preflight refuses a bake whose estimate passes the
//! cap; a watchdog ends a running bake whose real footprint passes it. The
//! estimate is topology dependent and only a first filter: the watchdog is the
//! real bound.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{CliError, Result};

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Footprint before the path search (package, rasters, Steam scene), rounded up.
const BASE_BYTES: u64 = 512 << 20;
/// Measured at 4 bake threads and a 1500 m path range (2026-10-01): the Loop at
/// 150 m peaked near 145 B/pair (76.7 M pairs, 11.1 GB), the Ravenswood
/// neighbourhood at 278 B/pair (100.4 M pairs, 27.9 GB), and the Loop at 250 m
/// passed 355 B/pair (55.4 M pairs, 18.3 GiB) before the watchdog ended it.
/// Per-pair cost follows street topology, so this keeps the worst seen.
const BYTES_PER_PROBE_PAIR: u64 = 400;
/// The default cap leaves half of host RAM to everything else.
const DEFAULT_HOST_FRACTION: f64 = 0.5;
/// Exit status of a bake the watchdog ended (BSD `EX_TEMPFAIL`).
pub(crate) const OVER_CAP_EXIT: i32 = 75;

#[must_use]
pub(crate) fn peak_estimate_bytes(probe_pairs: u64) -> u64 {
    BASE_BYTES.saturating_add(probe_pairs.saturating_mul(BYTES_PER_PROBE_PAIR))
}

/// Host RAM and the cap a bake must fit under.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct MemoryBudget {
    host_bytes: Option<u64>,
    cap_bytes: Option<u64>,
    explicit: bool,
}

impl MemoryBudget {
    /// `max_gib` (`--max-bake-memory-gb`, GiB) replaces the half-of-RAM default.
    #[must_use]
    pub(crate) fn new(host_bytes: Option<u64>, max_gib: Option<f64>) -> Self {
        let cap_bytes = match max_gib {
            Some(gib) => Some((gib * GIB) as u64),
            None => host_bytes.map(|host| (host as f64 * DEFAULT_HOST_FRACTION) as u64),
        };
        Self {
            host_bytes,
            cap_bytes,
            explicit: max_gib.is_some(),
        }
    }

    #[must_use]
    pub(crate) fn for_host(max_gib: Option<f64>) -> Self {
        Self::new(host_memory_bytes(), max_gib)
    }

    #[must_use]
    pub(crate) fn cap_bytes(&self) -> Option<u64> {
        self.cap_bytes
    }

    #[must_use]
    pub(crate) fn describe(&self, probe_pairs: u64) -> String {
        let gib = |bytes: u64| format!("{:.1} GiB", bytes as f64 / GIB);
        format!(
            "peak memory estimate {} for {probe_pairs} probe pairs (host RAM {}, cap {}{})",
            gib(peak_estimate_bytes(probe_pairs)),
            self.host_bytes.map_or("unknown".into(), gib),
            self.cap_bytes.map_or("none".into(), gib),
            if self.explicit {
                " from --max-bake-memory-gb"
            } else {
                ", half of RAM"
            },
        )
    }

    /// Refuses a bake whose estimate passes the cap, before any compute.
    pub(crate) fn check(&self, probe_pairs: u64) -> Result<()> {
        let Some(cap) = self.cap_bytes else {
            return Err(CliError::new(
                "cannot read host RAM to cap the bake's memory; pass --max-bake-memory-gb <GiB>",
            ));
        };
        if peak_estimate_bytes(probe_pairs) > cap {
            return Err(CliError::new(format!(
                "bake refused: {}. Path bake memory grows with probe pairs; shrink the area, add --corridor, or raise the cap with --max-bake-memory-gb <GiB> on a host with that much free RAM",
                self.describe(probe_pairs)
            )));
        }
        Ok(())
    }
}

/// A bake's memory cap plus where its run stages files and holds a lock, so
/// the watchdog can clean up if it must exit.
pub(crate) struct BakeGuard {
    pub(crate) budget: MemoryBudget,
    pub(crate) staging_roots: Vec<PathBuf>,
    pub(crate) lock: Option<PathBuf>,
}

impl BakeGuard {
    /// Half of host RAM; the bake's own output directory is its only staging.
    #[must_use]
    pub(crate) fn host_default() -> Self {
        Self {
            budget: MemoryBudget::for_host(None),
            staging_roots: Vec::new(),
            lock: None,
        }
    }
}

/// Watches the process footprint during a bake. The first poll over the cap
/// removes this process's staged directories and lock and exits at once.
/// Steam's cancel is not used: cancelling during the path search crashed the
/// process (SIGSEGV on Linux, 2026-10-01), and a crash can write a core dump
/// the size of the footprint.
pub(crate) struct Watchdog {
    cap_bytes: u64,
    /// Directories whose `.<name>.tmp.<pid>-*` entries belong to this run.
    staging_roots: Vec<PathBuf>,
    lock: Option<PathBuf>,
    peak_bytes: AtomicU64,
}

impl Watchdog {
    #[must_use]
    pub(crate) fn new(cap_bytes: u64, staging_roots: Vec<PathBuf>, lock: Option<PathBuf>) -> Self {
        Self {
            cap_bytes,
            staging_roots,
            lock,
            peak_bytes: AtomicU64::new(0),
        }
    }

    /// Polled by the bake's watcher thread. Never returns `true`: over the cap
    /// the process exits from here instead of asking Steam to cancel.
    pub(crate) fn should_cancel(&self) -> bool {
        if let Some(footprint) = phys_footprint_bytes() {
            if self.record(footprint) {
                self.abandon(footprint);
            }
        }
        false
    }

    /// Records one footprint sample; `true` once it passes the cap.
    fn record(&self, footprint: u64) -> bool {
        self.peak_bytes.fetch_max(footprint, Ordering::Relaxed);
        footprint > self.cap_bytes
    }

    #[must_use]
    pub(crate) fn peak_bytes(&self) -> u64 {
        self.peak_bytes.load(Ordering::Relaxed)
    }

    /// Cleans up as the failure path would and exits without unwinding or
    /// running exit handlers, since Steam's bake threads are still running.
    fn abandon(&self, footprint: u64) -> ! {
        eprintln!(
            "fightbox: bake memory footprint {:.1} GiB passed the {:.1} GiB cap; removing staged files and exiting (status {OVER_CAP_EXIT}). Nothing was written. Shrink the area, use --graded or --corridor, or bake on a host with more free RAM.",
            footprint as f64 / GIB,
            self.cap_bytes as f64 / GIB
        );
        let marker = format!(".tmp.{}-", std::process::id());
        for root in &self.staging_roots {
            for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') && name.contains(&marker) {
                    let path = entry.path();
                    let _ = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
                }
            }
        }
        if let Some(lock) = &self.lock {
            let _ = std::fs::remove_file(lock);
        }
        // SAFETY: `_exit` ends the process immediately; nothing runs after it.
        unsafe { libc::_exit(OVER_CAP_EXIT) }
    }
}

/// Stops a crashing bake from writing a core dump. Under WSL a pipe
/// `core_pattern` ignores `ulimit -c` and copies the whole footprint to the
/// Windows system drive (18.5 GB on 2026-10-01).
pub(crate) fn forbid_crash_dumps() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE takes a plain integer flag.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

/// Physical footprint of this process, including compressed and swapped pages.
#[cfg(target_os = "macos")]
#[must_use]
pub(crate) fn phys_footprint_bytes() -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
    // SAFETY: RUSAGE_INFO_V2 writes at most one `rusage_info_v2` into `info`.
    let status = unsafe {
        libc::proc_pid_rusage(
            libc::getpid(),
            libc::RUSAGE_INFO_V2,
            info.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    // SAFETY: a zero status means the kernel filled the record.
    (status == 0).then(|| unsafe { info.assume_init() }.ri_phys_footprint)
}

/// Resident plus swapped memory, the closest Linux analogue of the footprint.
#[cfg(target_os = "linux")]
#[must_use]
pub(crate) fn phys_footprint_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let kib = |key: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(key))
            .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
    };
    Some((kib("VmRSS:")? + kib("VmSwap:").unwrap_or(0)) * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[must_use]
pub(crate) fn phys_footprint_bytes() -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn host_memory_bytes() -> Option<u64> {
    let mut bytes = 0_u64;
    let mut size = std::mem::size_of::<u64>();
    // SAFETY: `hw.memsize` is a u64 and `size` names exactly that buffer.
    let status = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&raw mut bytes).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (status == 0 && bytes > 0).then_some(bytes)
}

/// `MemTotal`, which under WSL is the VM's configured limit.
#[cfg(target_os = "linux")]
fn host_memory_bytes() -> Option<u64> {
    std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kib| kib * 1024)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn host_memory_bytes() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_covers_the_measured_bake_and_the_cap_refuses_it_on_a_16_gib_mac() {
        let pairs = |probes: u64| probes * (probes - 1) / 2;
        // Every measured bake stays covered: the neighbourhood at 27.9 GB and
        // the Loop at 250 m, which passed 18.3 GiB.
        assert!(peak_estimate_bytes(pairs(14_168)) >= 27_902_873_592);
        assert!(peak_estimate_bytes(55_400_000) as f64 > 18.3 * GIB);
        // The killed 20,562-probe bake was already past 35 GB.
        assert!(peak_estimate_bytes(pairs(20_562)) > 35_000_000_000);

        let mac = MemoryBudget::new(Some(16 << 30), None);
        assert_eq!(mac.cap_bytes(), Some(8 << 30));
        assert!(mac.check(pairs(4_000)).is_ok());
        let refused = mac.check(pairs(14_168)).unwrap_err().to_string();
        assert!(
            refused.contains("--max-bake-memory-gb") && refused.contains("cap 8.0 GiB"),
            "{refused}"
        );
        // An explicit cap replaces the default either way.
        assert!(
            MemoryBudget::new(Some(16 << 30), Some(48.0))
                .check(pairs(14_168))
                .is_ok()
        );
        assert!(
            MemoryBudget::new(Some(16 << 30), Some(0.25))
                .check(pairs(100))
                .is_err()
        );
        // Without a known host size only an explicit cap allows a bake.
        assert!(MemoryBudget::new(None, None).check(pairs(100)).is_err());
        assert!(MemoryBudget::new(None, Some(1.0)).check(pairs(100)).is_ok());

        // The footprint is readable, and a sample over the cap trips the watchdog.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(phys_footprint_bytes().is_some_and(|bytes| bytes > 1 << 20));
        let watchdog = Watchdog::new(1 << 30, Vec::new(), None);
        assert!(!watchdog.record(1 << 29));
        assert!(watchdog.record((1 << 30) + 1) && watchdog.peak_bytes() == (1 << 30) + 1);
    }
}
