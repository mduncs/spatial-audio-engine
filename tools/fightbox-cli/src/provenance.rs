//! Run provenance: engine identity, SDK dylib checksum, and host facts.
//!
//! Authority note §ν requires every capture to record engine commit and build
//! profile, Steam Audio version and binary checksum, platform, CPU class, sample
//! rate, block size, HRTF identity, requested/delivered quality, durations,
//! streaming cadence, and explicit claims/non-claims.
//!
//! Legacy capture schemas retain their original static engine-identity field.
//! New acceptance protocols must carry a committed base plus a bundled source
//! snapshot and authority executable rather than treating that legacy field as
//! current source truth.

use std::path::{Path, PathBuf};

use fightbox_steam_audio::{STEAM_AUDIO_UPSTREAM_COMMIT, STEAM_AUDIO_VERSION, sha256_hex};

/// The honest engine identity for this unborn-branch repository.
pub const ENGINE_IDENTITY: &str = "unborn-main";

/// Explicit non-claim: a base commit alone does not reproduce dirty source.
pub const UNCOMMITTED_SOURCE_NONCLAIM: &str = "Engine commit alone does not reproduce uncommitted source; use the capture's frozen source and binary authority when provided.";

/// Explicit non-claim: no delivered-ear SPL without an output-device transfer.
pub const NO_DELIVERED_EAR_SPL_NONCLAIM: &str =
    "No delivered-ear-SPL claim without a measured output-device/headphone transfer.";

/// Explicit non-claim: the remaining Phase A gates are not yet run. Lifetime /
/// leak tooling has run successfully on this Mac; the open gates are the
/// performance/km-sweep decision and the human S3 listening judgment, not an
/// unqualified "sanitizer suite not implemented" claim.
pub const REMAINING_PHASE_A_GATES_NONCLAIM: &str = "Remaining Phase A gates (performance/km sweep and the human S3 listening judgment) are not implemented in this slice; lifetime/leak tooling has already run.";

/// SDK dylib provenance, located from `STEAM_AUDIO_SDK_DIR` on a linked run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdkBinary {
    pub version: &'static str,
    pub upstream_commit: &'static str,
    pub dylib_path: Option<PathBuf>,
    /// SHA-256 of the actual dylib bytes when `STEAM_AUDIO_SDK_DIR` resolved it.
    pub dylib_checksum_sha256: Option<String>,
}

impl SdkBinary {
    /// Locate the SDK library relative to `STEAM_AUDIO_SDK_DIR` and hash it.
    /// Returns the pinned version/commit even when the dylib is not found, so a
    /// non-linked build still records honest provenance.
    pub fn detect() -> Self {
        Self::detect_from(std::env::var_os("STEAM_AUDIO_SDK_DIR").as_deref())
    }

    /// Path-parameterized core of [`Self::detect`]. Accepts the SDK directory
    /// explicitly so tests and callers can drive detection without mutating the
    /// process-global `STEAM_AUDIO_SDK_DIR` (which would race other tests).
    pub fn detect_from(sdk_dir: Option<&std::ffi::OsStr>) -> Self {
        let version = STEAM_AUDIO_VERSION;
        let upstream_commit = STEAM_AUDIO_UPSTREAM_COMMIT;
        let Some(sdk_dir) = sdk_dir else {
            return Self {
                version,
                upstream_commit,
                dylib_path: None,
                dylib_checksum_sha256: None,
            };
        };
        let dylib = find_dylib(Path::new(&sdk_dir));
        let (dylib_path, dylib_checksum_sha256) = match dylib {
            Some(path) => {
                let checksum = std::fs::read(&path).ok().map(|bytes| sha256_hex(&bytes));
                (Some(path), checksum)
            }
            None => (None, None),
        };
        Self {
            version,
            upstream_commit,
            dylib_path,
            dylib_checksum_sha256,
        }
    }

    /// True only when both the resolved dylib path and its SHA-256 checksum were
    /// established. Artifact-producing linked commands must reject capture when
    /// this is false: a build with the SDK env present but an invocation without
    /// it produces a null path/hash, which is not valid provenance.
    #[must_use]
    pub fn dylib_established(&self) -> bool {
        self.dylib_path.is_some() && self.dylib_checksum_sha256.is_some()
    }

    /// Reject an artifact-producing capture unless the actual SDK dylib path and
    /// checksum were established from `STEAM_AUDIO_SDK_DIR` at runtime. A linked
    /// build invoked without the env produces a null path/hash — that capture is
    /// invalid provenance and must not be written. Shared by s0/s3-bake/s3-render.
    pub fn require_established(&self) -> crate::error::Result<()> {
        if self.dylib_established() {
            Ok(())
        } else {
            Err(crate::error::CliError::new(
                "artifact-producing capture requires the Steam Audio dylib path and checksum to be established; set STEAM_AUDIO_SDK_DIR to the verified SDK at runtime",
            ))
        }
    }
}

/// Search for the target library within an SDK root. Mirrors the backend build
/// script's target selection for macOS/Linux.
fn find_dylib(sdk_root: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const TARGET_DIR: &str = "osx";
    #[cfg(target_os = "linux")]
    const TARGET_DIR: &str = "linux-x64";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = sdk_root;
        None
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        const LIB_NAME: &str = if cfg!(target_os = "macos") {
            "libphonon.dylib"
        } else {
            "libphonon.so"
        };
        let mut found = Vec::new();
        collect_named(sdk_root, LIB_NAME, 5, &mut found);
        found
            .into_iter()
            .find(|path| path.components().any(|part| part.as_os_str() == TARGET_DIR))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn collect_named(root: &Path, name: &str, remaining_depth: usize, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.file_name().is_some_and(|n| n == name) {
            found.push(path);
        } else if remaining_depth > 0 && path.is_dir() {
            collect_named(&path, name, remaining_depth - 1, found);
        }
    }
}

/// The host platform identity (best-effort; never claims a specific board).
pub fn platform() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "darwin"
    }
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unknown"
    }
}

/// The host CPU class (best-effort).
pub fn cpu_class() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        "unknown"
    }
}

/// HRTF identity used for the Phase A binaural render.
pub const HRTF_IDENTITY: &str = "steam-audio-default";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_identity_is_explicit_and_dirty_source_nonclaim_requires_authority() {
        assert_eq!(ENGINE_IDENTITY, "unborn-main");
        assert!(UNCOMMITTED_SOURCE_NONCLAIM.contains("frozen source"));
        assert!(UNCOMMITTED_SOURCE_NONCLAIM.contains("binary authority"));
    }

    #[test]
    fn sdk_binary_records_pinned_version_without_dir() {
        // Drive detection through the path-parameterized core so no
        // process-global environment mutation is involved (which would race
        // other tests). With no SDK directory, the pinned version is recorded
        // and the dylib is not established.
        let sdk = SdkBinary::detect_from(None);
        assert_eq!(sdk.version, "4.8.1");
        assert_eq!(sdk.upstream_commit, "0da1825");
        assert!(sdk.dylib_checksum_sha256.is_none());
        assert!(sdk.dylib_path.is_none());
        assert!(!sdk.dylib_established());
    }

    #[cfg(all(target_os = "macos", feature = "linked-sdk"))]
    #[test]
    fn sdk_binary_establishes_dylib_from_verified_sdk() {
        // The linked build contract already requires this absolute path. Read
        // it without mutating process-global state so isolated worktrees use
        // the canonical checkout's ignored SDK cache just like the build.
        let sdk_dir = std::env::var_os("STEAM_AUDIO_SDK_DIR")
            .expect("linked test requires STEAM_AUDIO_SDK_DIR");
        let sdk = SdkBinary::detect_from(Some(sdk_dir.as_os_str()));
        assert!(
            sdk.dylib_established(),
            "dylib must be established under verified SDK"
        );
        let dylib = sdk
            .dylib_path
            .expect("dylib path must resolve under verified SDK");
        assert!(
            dylib.is_file(),
            "dylib path must point at a real file: {}",
            dylib.display()
        );
        let checksum = sdk
            .dylib_checksum_sha256
            .expect("dylib checksum must be computed under verified SDK");
        assert_eq!(checksum.len(), 64);
    }

    #[test]
    fn sdk_binary_not_established_for_missing_dir() {
        // A directory that does not contain the dylib must not claim it.
        let sdk = SdkBinary::detect_from(Some(std::ffi::OsStr::new("/nonexistent-sdk-dir")));
        assert!(!sdk.dylib_established());
        assert!(sdk.dylib_checksum_sha256.is_none());
    }
}
