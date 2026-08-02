//! Early disk reservation for city probe-batch artifacts.
//!
//! A city bake holds Steam Audio's serialization in memory until the SDK call
//! returns. Reserving the eventual file before that call turns a likely ENOSPC
//! into a cheap pre-compute failure while the surrounding `AtomicDir` keeps the
//! final directory invisible until every artifact is complete.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::error::{CliError, Result};

const RESERVATION_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const APFS_CAVEAT: &str = "filesystem-reported availability can still be optimistic on APFS because purgeable space may disappear";

/// A physically written placeholder for the eventual `probe-batch.bin`.
#[derive(Debug)]
pub(crate) struct BakeReservation {
    file: File,
    path: PathBuf,
    reserved_bytes: u64,
    reported_available_bytes: u64,
}

impl BakeReservation {
    /// Check the destination filesystem and claim `required_bytes` before bake
    /// compute begins. Zeroes are written, rather than relying on a sparse
    /// `set_len`, so an ENOSPC is observed here on filesystems such as APFS.
    pub(crate) fn create(path: &Path, output: &Path, required_bytes: u64) -> Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            CliError::new(format!(
                "city bake reservation path {} has no parent",
                path.display()
            ))
        })?;
        let available = available_bytes(parent).map_err(|error| {
            CliError::new(format!(
                "cannot check available disk space for city bake output {} before compute: {error}; {APFS_CAVEAT}",
                output.display()
            ))
        })?;
        if required_bytes > available {
            return Err(CliError::new(format!(
                "insufficient disk space for city bake output {} before compute: estimated artifact needs {}, but the filesystem reports only {} available; {APFS_CAVEAT}",
                output.display(),
                format_bytes(required_bytes),
                format_bytes(available)
            )));
        }

        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|error| reservation_error(output, required_bytes, available, error))?;
        if let Err(error) = write_reservation(&mut file, required_bytes) {
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(reservation_error(output, required_bytes, available, error));
        }

        Ok(Self {
            file,
            path: path.to_owned(),
            reserved_bytes: required_bytes,
            reported_available_bytes: available,
        })
    }

    #[must_use]
    pub(crate) fn reported_available_bytes(&self) -> u64 {
        self.reported_available_bytes
    }

    /// Replace the zero reservation with the exact SDK serialization and
    /// truncate its unused tail. The file is already inside the hidden atomic
    /// directory, so no second temporary copy (and no second allocation) is
    /// needed.
    pub(crate) fn finish(mut self, bytes: &[u8]) -> Result<()> {
        let actual_bytes = u64::try_from(bytes.len())
            .map_err(|_| CliError::new("city probe batch exceeds the supported file size"))?;
        if actual_bytes > self.reserved_bytes {
            return Err(CliError::new(format!(
                "city probe batch is {} but only {} was reserved before compute; the conservative artifact estimator was exceeded",
                format_bytes(actual_bytes),
                format_bytes(self.reserved_bytes)
            )));
        }
        self.file.seek(SeekFrom::Start(0)).map_err(|error| {
            CliError::new(format!(
                "cannot rewind reserved city probe batch {}: {error}",
                self.path.display()
            ))
        })?;
        self.file.write_all(bytes).map_err(|error| {
            CliError::new(format!(
                "cannot fill reserved city probe batch {}: {error}",
                self.path.display()
            ))
        })?;
        self.file.set_len(actual_bytes).map_err(|error| {
            CliError::new(format!(
                "cannot truncate reserved city probe batch {} to its final size: {error}",
                self.path.display()
            ))
        })?;
        self.file.sync_all().map_err(|error| {
            CliError::new(format!(
                "cannot flush completed city probe batch {}: {error}",
                self.path.display()
            ))
        })
    }
}

fn write_reservation(file: &mut File, required_bytes: u64) -> std::io::Result<()> {
    let zeroes = vec![0_u8; RESERVATION_CHUNK_BYTES];
    let mut remaining = required_bytes;
    while remaining > 0 {
        let count = usize::try_from(remaining.min(RESERVATION_CHUNK_BYTES as u64))
            .expect("reservation chunk size always fits usize");
        file.write_all(&zeroes[..count])?;
        remaining -= count as u64;
    }
    file.sync_all()
}

fn reservation_error(
    output: &Path,
    required_bytes: u64,
    available: u64,
    error: std::io::Error,
) -> CliError {
    CliError::new(format!(
        "cannot reserve {} for city bake output {} before compute (filesystem reported {} available): {error}; {APFS_CAVEAT}",
        format_bytes(required_bytes),
        output.display(),
        format_bytes(available)
    ))
}

#[must_use]
pub(crate) fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(all(target_vendor = "apple", target_pointer_width = "64"))]
#[repr(C)]
struct StatVfs {
    f_bsize: std::ffi::c_ulong,
    f_frsize: std::ffi::c_ulong,
    // Darwin's fsblkcnt_t/fsfilcnt_t stay 32-bit on 64-bit hosts.
    f_blocks: u32,
    f_bfree: u32,
    f_bavail: u32,
    f_files: u32,
    f_ffree: u32,
    f_favail: u32,
    f_fsid: std::ffi::c_ulong,
    f_flag: std::ffi::c_ulong,
    f_namemax: std::ffi::c_ulong,
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
#[repr(C)]
struct StatVfs {
    f_bsize: std::ffi::c_ulong,
    f_frsize: std::ffi::c_ulong,
    f_blocks: u64,
    f_bfree: u64,
    f_bavail: u64,
    f_files: u64,
    f_ffree: u64,
    f_favail: u64,
    f_fsid: std::ffi::c_ulong,
    f_flag: std::ffi::c_ulong,
    f_namemax: std::ffi::c_ulong,
    reserved: [std::ffi::c_int; 6],
}

#[cfg(any(
    all(target_vendor = "apple", target_pointer_width = "64"),
    all(target_os = "linux", target_pointer_width = "64")
))]
unsafe extern "C" {
    fn statvfs(path: *const std::ffi::c_char, buffer: *mut StatVfs) -> std::ffi::c_int;
}

#[cfg(any(
    all(target_vendor = "apple", target_pointer_width = "64"),
    all(target_os = "linux", target_pointer_width = "64")
))]
fn available_bytes(path: &Path) -> std::io::Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "filesystem path contains an interior NUL byte",
        )
    })?;
    let mut storage = std::mem::MaybeUninit::<StatVfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and alive for the call. `StatVfs`
    // matches the target's public statvfs ABI, and the OS initializes it fully
    // when returning success.
    if unsafe { statvfs(path.as_ptr(), storage.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: successful statvfs initialized every field.
    let storage = unsafe { storage.assume_init() };
    (storage.f_bavail as u64)
        .checked_mul(storage.f_frsize as u64)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "available filesystem byte count overflowed u64",
            )
        })
}

#[cfg(windows)]
fn available_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_bytes: *mut u64,
            total_free_bytes: *mut u64,
        ) -> i32;
    }

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0_u64;
    // SAFETY: `wide` is NUL-terminated and all output pointers are valid for
    // the duration of the call.
    if unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(available)
}

#[cfg(not(any(
    all(target_vendor = "apple", target_pointer_width = "64"),
    all(target_os = "linux", target_pointer_width = "64"),
    windows
)))]
fn available_bytes(_path: &Path) -> std::io::Result<u64> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "available-space queries are unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn scratch(label: &str) -> PathBuf {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp/lane-bake-robustness")
        } else {
            std::env::temp_dir().join("lane-bake-robustness")
        };
        std::fs::create_dir_all(&base).unwrap();
        let path = base.join(format!(
            "reservation-{label}-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn reservation_is_truncated_to_the_exact_final_bytes() {
        let root = scratch("truncate");
        let path = root.join("probe-batch.bin");
        let output = root.join("city.baked");
        let reservation = BakeReservation::create(&path, &output, 64 * 1024).unwrap();
        assert_eq!(path.metadata().unwrap().len(), 64 * 1024);

        let expected = b"exact probe batch bytes";
        reservation.finish(expected).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn impossible_reservation_fails_before_creating_a_file() {
        let root = scratch("insufficient");
        let path = root.join("probe-batch.bin");
        let output = root.join("city.baked");
        let error = BakeReservation::create(&path, &output, u64::MAX).unwrap_err();
        assert!(error.message().contains("insufficient disk space"));
        assert!(error.message().contains("optimistic on APFS"));
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(root);
    }
}
