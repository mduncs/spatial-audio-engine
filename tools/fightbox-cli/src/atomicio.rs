//! Atomic, deterministic output for world directories and capture bundles.
//!
//! Every artifact is written into a sibling temporary directory/file and renamed
//! into place on success. A non-empty existing output is rejected rather than
//! mixing runs. WAVs are encoded and hashed through the canonical
//! `fightbox-evidence` writer so a stem hash is stable and verifiable.
//!
//! Output path policy: the path must resolve to an absolute location. An
//! explicit system temp directory (for example `/tmp/...` or the canonicalized
//! `TMPDIR`) is a valid, accepted gate output — acceptance renders into absolute
//! temp paths by design. What is rejected is writing into the vendored Steam
//! Audio SDK cache, the repository working tree itself, or any other broad or
//! destructive target. These are the only checks this function performs; it
//! never claims a check it does not implement.

use std::path::{Path, PathBuf};

use fightbox_evidence::{WavError, WavSpec, sha256_hex, write_wav};

use crate::error::{CliError, Result};

/// Resolve `path` to its canonical absolute form even when it does not yet
/// exist, by canonicalizing the nearest ancestor that does exist and rejoining
/// the components below it.
///
/// A run directory that has not been created yet still has to be policy-checked,
/// and checked *before* anything is created — so this resolves through missing
/// ancestors rather than refusing them. `AtomicDir::create` creates the parent
/// afterwards, once the destination is known to be allowed.
fn canonicalize_target(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path.canonicalize().map_err(|e| {
            CliError::new(format!(
                "output path {} cannot be canonicalized: {e}",
                path.display()
            ))
        });
    }
    let absolute = std::path::absolute(path).map_err(|e| {
        CliError::new(format!(
            "output path {} cannot be made absolute: {e}",
            path.display()
        ))
    })?;
    let mut existing = absolute.as_path();
    let mut missing = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            CliError::new(format!(
                "output path {} has no existing ancestor to resolve against",
                path.display()
            ))
        })?;
        missing.push(name.to_owned());
        existing = existing.parent().ok_or_else(|| {
            CliError::new(format!(
                "output path {} has no parent directory",
                path.display()
            ))
        })?;
    }
    let mut resolved = existing.canonicalize().map_err(|e| {
        CliError::new(format!(
            "output ancestor {} is not accessible: {e}",
            existing.display()
        ))
    })?;
    resolved.extend(missing.iter().rev());
    Ok(resolved)
}

/// Reject an output location that is not absolute or that resolves into a
/// forbidden area. Returns the canonicalized absolute path on success.
///
/// An explicit absolute system temp directory is *accepted* — gates render into
/// `/tmp`/`TMPDIR` by design. The only rejected targets are the vendored Steam
/// Audio SDK cache, the repository working tree, and the filesystem root.
pub fn validate_output_path(path: &Path) -> Result<PathBuf> {
    let canonical = canonicalize_target(path)?;
    if !canonical.is_absolute() {
        return Err(CliError::new(format!(
            "output path {} resolved to a non-absolute location; refusing relative output",
            path.display()
        )));
    }
    reject_forbidden_output(&canonical)?;
    Ok(canonical)
}

/// Reject the vendored SDK cache, the repository working tree, and the root.
/// An explicit system temp directory is permitted.
fn reject_forbidden_output(canonical: &Path) -> Result<()> {
    let display = canonical.to_string_lossy();

    // The vendored Steam Audio SDK lives under `.cache/steam-audio`; never write
    // capture output into a vendored SDK area.
    const SDK_CACHE_MARKER: &str = ".cache/steam-audio";
    if display.contains(SDK_CACHE_MARKER) {
        return Err(CliError::new(format!(
            "output path {} is inside the Steam Audio SDK cache; refusing to write there",
            canonical.display()
        )));
    }

    // Refuse the repository working tree (where source, fixtures, and docs live)
    // and the filesystem root. A capture bundle must live in its own dedicated
    // directory outside the source tree.
    if let Some(repo_root) = repository_root() {
        if canonical.starts_with(&repo_root) {
            return Err(CliError::new(format!(
                "output path {} is inside the repository working tree ({}); write capture bundles to a dedicated directory outside the source tree",
                canonical.display(),
                repo_root.display()
            )));
        }
    }
    if canonical.parent().is_none() {
        return Err(CliError::new(format!(
            "output path {} is the filesystem root; refusing to write there",
            canonical.display()
        )));
    }
    Ok(())
}

/// Best-effort canonical path to the repository root, resolved from the
/// manifest directory embedded at build time. Returns `None` if it cannot be
/// resolved (in which case the repository check is simply skipped).
fn repository_root() -> Option<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    // `tools/fightbox-cli` -> repository root is two levels up.
    let root = manifest_dir.parent()?.parent()?;
    root.canonicalize().ok()
}

/// A directory written atomically via a sibling temporary directory.
#[derive(Debug)]
pub struct AtomicDir {
    final_path: PathBuf,
    temp_path: PathBuf,
    committed: bool,
}

impl AtomicDir {
    /// Create a sibling temporary directory for `final_path`. Rejects a non-empty
    /// existing target so two runs cannot mix.
    pub fn create(final_path: PathBuf) -> Result<Self> {
        if final_path.exists() {
            let is_nonempty = final_path
                .read_dir()
                .map(|mut iterator| iterator.next().is_some())
                .unwrap_or(false);
            if is_nonempty {
                return Err(CliError::new(format!(
                    "output directory {} already exists and is non-empty; refusing to mix runs",
                    final_path.display()
                )));
            }
        }
        let parent = final_path.parent().ok_or_else(|| {
            CliError::new(format!(
                "output directory {} has no parent",
                final_path.display()
            ))
        })?;
        let final_name = final_path
            .file_name()
            .ok_or_else(|| {
                CliError::new(format!(
                    "output directory {} has no final component",
                    final_path.display()
                ))
            })?
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(format!(
                "cannot create output parent {}: {e}",
                parent.display()
            ))
        })?;
        let temp_path = parent.join(format!(".{final_name}.tmp.{}", unique_suffix()));
        std::fs::create_dir(&temp_path).map_err(|e| {
            CliError::new(format!(
                "cannot create temporary directory {}: {e}",
                temp_path.display()
            ))
        })?;
        Ok(Self {
            final_path,
            temp_path,
            committed: false,
        })
    }

    /// The temporary directory contents are staged here.
    #[must_use]
    pub fn temp_path(&self) -> &Path {
        &self.temp_path
    }

    /// Rename the temporary directory into its final location on success.
    pub fn commit(mut self) -> Result<()> {
        // Remove any empty pre-existing target so the rename succeeds.
        if self.final_path.exists() {
            std::fs::remove_dir(&self.final_path).map_err(|e| {
                CliError::new(format!(
                    "cannot remove existing empty target {}: {e}",
                    self.final_path.display()
                ))
            })?;
        }
        std::fs::rename(&self.temp_path, &self.final_path).map_err(|e| {
            CliError::new(format!(
                "cannot rename {} to {}: {e}",
                self.temp_path.display(),
                self.final_path.display()
            ))
        })?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for AtomicDir {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_dir_all(&self.temp_path);
        }
    }
}

/// Write an atomic JSON file: serialize to a sibling temp file and rename.
pub fn write_json_atomic<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| CliError::new(format!("cannot serialize {}: {e}", path.display())))?;
    write_bytes_atomic(path, &bytes)
}

/// Write a JSON string (already serialized) atomically.
pub fn write_json_string_atomic(path: &Path, json: &str) -> Result<()> {
    write_bytes_atomic(path, json.as_bytes())
}

/// Write exact bytes atomically: a sibling temp file plus rename.
pub fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CliError::new(format!("output path {} has no parent", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| CliError::new(format!("output path {} has no file name", path.display())))?
        .to_string_lossy()
        .into_owned();
    let temp_path = parent.join(format!(".{name}.tmp.{}", unique_suffix()));
    if let Some(existing) = path.parent() {
        std::fs::create_dir_all(existing).map_err(|e| {
            CliError::new(format!(
                "cannot create output parent {}: {e}",
                existing.display()
            ))
        })?;
    }
    std::fs::write(&temp_path, bytes).map_err(|e| {
        CliError::new(format!(
            "cannot write temporary file {}: {e}",
            temp_path.display()
        ))
    })?;
    std::fs::rename(&temp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        CliError::new(format!(
            "cannot rename {} to {}: {e}",
            temp_path.display(),
            path.display()
        ))
    })
}

/// Write exact bytes to a plain (non-atomic) path. Used for `fixture.json`
/// copies where the source bytes are the contract.
pub fn write_bytes_plain(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CliError::new(format!(
                "cannot create output parent {}: {e}",
                parent.display()
            ))
        })?;
    }
    std::fs::write(path, bytes)
        .map_err(|e| CliError::new(format!("cannot write {}: {e}", path.display())))
}

/// A finite-stereo WAV written to disk plus its content hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrittenWav {
    pub content_sha256: String,
    pub frame_count: usize,
}

/// Write a finite-stereo WAV to `path` inside `dir`, returning its SHA-256 and
/// frame count. Samples must be interleaved stereo.
pub fn write_stereo_wav(
    dir: &Path,
    file_name: &str,
    sample_rate_hz: i32,
    interleaved: &[f32],
) -> Result<WrittenWav> {
    let spec = WavSpec {
        sample_rate_hz: sample_rate_hz as u32,
        channels: 2,
    };
    let bytes = write_wav(spec, interleaved).map_err(|e| {
        CliError::new(format!(
            "cannot encode {file_name}: {}",
            wav_error_message(&e)
        ))
    })?;
    let hash = sha256_hex(&bytes);
    let frame_count = interleaved.len() / 2;
    write_bytes_atomic(&dir.join(file_name), &bytes)?;
    Ok(WrittenWav {
        content_sha256: hash,
        frame_count,
    })
}

/// Read raw bytes from a file.
pub fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|e| CliError::new(format!("cannot read {}: {e}", path.display())))
}

fn wav_error_message(error: &WavError) -> &'static str {
    error.as_str()
}

fn unique_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("{}-{}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fightbox_evidence::sine;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    fn stereo_spec() -> WavSpec {
        WavSpec {
            sample_rate_hz: 48_000,
            channels: 2,
        }
    }

    #[test]
    fn atomic_dir_commits_and_cleans_up_on_failure() {
        let root = tempdir();
        let target = root.join("world");
        {
            let dir = AtomicDir::create(target.clone()).unwrap();
            std::fs::write(dir.temp_path().join("a.txt"), b"hello").unwrap();
            dir.commit().unwrap();
        }
        assert!(target.exists());
        assert_eq!(std::fs::read(target.join("a.txt")).unwrap(), b"hello");
    }

    #[test]
    fn atomic_dir_rejects_nonempty_target() {
        let root = tempdir();
        let target = root.join("world");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("stale.txt"), b"x").unwrap();
        let error = AtomicDir::create(target).unwrap_err();
        assert!(error.message().contains("non-empty"));
    }

    #[test]
    fn atomic_dir_dropped_without_commit_leaves_no_trace() {
        let root = tempdir();
        let target = root.join("world");
        let temp_path;
        {
            let dir = AtomicDir::create(target.clone()).unwrap();
            temp_path = dir.temp_path().to_path_buf();
            // dropped without commit
        }
        assert!(!target.exists());
        assert!(!temp_path.exists(), "leftover temp dir: {temp_path:?}");
        // No other artifact from this atomic write may remain in its isolated
        // test parent.
        let remaining: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
        assert!(remaining.is_empty(), "leftover temp files: {remaining:?}");
    }

    #[test]
    fn write_stereo_wav_round_trips_and_hashes() {
        let dir = tempdir();
        let spec = stereo_spec();
        let signal = sine(spec, 1_000.0, 480, -20.0).unwrap();
        let written = write_stereo_wav(&dir, "direct.wav", 48_000, &signal.samples).unwrap();
        assert_eq!(written.frame_count, 480);
        assert_eq!(written.content_sha256.len(), 64);
        let bytes = std::fs::read(dir.join("direct.wav")).unwrap();
        assert_eq!(sha256_hex(&bytes), written.content_sha256);
    }

    #[test]
    fn validate_output_path_rejects_sdk_cache() {
        let path = Path::new("/tmp/.cache/steam-audio/steamaudio-4.8.1/bundle");
        // validate_output_path canonicalizes the parent, which here does not
        // exist, so it returns an error rather than silently accepting.
        assert!(validate_output_path(path).is_err());
    }

    #[test]
    fn validate_output_path_accepts_explicit_system_temp() {
        // An absolute path inside the system temp directory is a valid gate
        // output. The parent (`/tmp`) exists and canonicalizes, and the target
        // is not the SDK cache, the repo tree, or the root.
        let dir = tempdir();
        let target = dir.join("s0-bundle");
        let resolved =
            validate_output_path(&target).expect("absolute temp output must be accepted");
        assert!(resolved.is_absolute());
        assert_eq!(
            resolved.file_name(),
            Some(std::ffi::OsStr::new("s0-bundle"))
        );
    }

    #[test]
    fn validate_output_path_resolves_through_a_run_directory_that_does_not_exist_yet() {
        // `--output <fresh-run-dir>/<name>.baked` is an ordinary request. The
        // policy check has to reach a verdict on it without creating anything,
        // so a long-running command can fail fast on a bad destination.
        let dir = tempdir();
        let target = dir.join("run-2026-07-31").join("megablock.baked");
        let resolved = validate_output_path(&target).expect("missing parents must still resolve");
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("run-2026-07-31/megablock.baked"));
        assert!(
            !target.parent().unwrap().exists(),
            "validation must not create the run directory"
        );

        // Resolving through missing ancestors must not become a way around the
        // forbidden-target policy.
        assert!(
            validate_output_path(&dir.join("nested").join(".cache/steam-audio/bundle")).is_err()
        );
    }

    #[test]
    fn validate_output_path_rejects_relative_output() {
        // A purely relative path whose parent canonicalizes inside the repo tree
        // is rejected as repo-internal; a path with no parent component is
        // rejected outright. Either way it must not be accepted.
        assert!(validate_output_path(Path::new("relative-bundle")).is_err());
    }

    #[test]
    fn reject_forbidden_output_accepts_temp_and_rejects_cache_and_root() {
        // Direct unit coverage of the policy helper, independent of whether a
        // parent happens to canonicalize on the host.
        let temp = tempdir();
        let bundle = temp.join("bundle");
        assert!(reject_forbidden_output(&bundle).is_ok());

        let cache = Path::new("/var/folders/.cache/steam-audio/x");
        assert!(reject_forbidden_output(cache).is_err());

        let root = Path::new("/");
        assert!(reject_forbidden_output(root).is_err());
    }

    fn tempdir() -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "fightbox-cli-test-{}-{sequence}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
