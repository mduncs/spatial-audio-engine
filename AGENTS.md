# spatial-audio-engine

The Steam Audio SDK is acquired only through `scripts/acquire-steam-audio.sh` into
the ignored `.cache/steam-audio/` directory. Do not vendor SDK archives, binaries,
or a source checkout. The default Rust build deliberately works without the SDK;
SDK linking is an explicit `linked-sdk` feature and requires
`STEAM_AUDIO_SDK_DIR` to name the verified extracted SDK — always as an ABSOLUTE
path (a relative value is resolved from the build-script crate dir and falsely
reports a missing `phonon.h`).

Every *binary* crate built with `linked-sdk` needs its own `build.rs` emitting the
`-Wl,-rpath` link arg from `DEP_FIGHTBOX_STEAM_AUDIO_LIBRARY_DIR` (copy
`tools/fightbox-cli/build.rs`): `cargo:rustc-link-arg` from the backend crate does
not propagate to dependent binaries, and a missing rpath aborts at dyld
(`libphonon.dylib not found`) only at run time.

All direct Steam Audio C API calls belong in `crates/fightbox-steam-audio`. Domain
coordinates are right-handed local ENU (`x` east, `y` north, `z` up); the backend
mapping is exactly Steam `(x, y, z) = (ENU.x, ENU.z, -ENU.y)`.

Crate dependency direction (since 2026-07-29): `fightbox-runtime` depends only on
`fightbox-api`; `fightbox-steam-audio` depends on both and implements the frozen seam
traits in `crates/fightbox-runtime/src/backend.rs`. The capability/status facade lives
in `fightbox-steam-audio::status`. Live audio output is the runtime `live-output`
feature (cpal); default builds are device-free.

Capture bundles must be written outside the repository tree (the CLI enforces this);
the canonical evidence area is `~/fightbox-runs/`. A full `phase-a sweep` hashes the
live `crates/` + `tools/` tree at start and at self-verify, so it requires a quiescent
source tree — run it from a frozen snapshot copy when agents are editing.

Execution state of record: `EXECUTION.md` (proof contracts, lane ownership, gates).
