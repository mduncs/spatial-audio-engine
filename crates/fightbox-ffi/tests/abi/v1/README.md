# Fightbox C ABI v1 freeze

`fightbox_v1.h` is an immutable copy of the public C surface immediately before
the Wave 17 versioned ABI window. It is test source truth and must not be
regenerated. The check script pins its SHA-256 so changing the fixture cannot
silently redefine what “v1 compatible” means.

`legacy_client.c` compiles as an old caller. It pins the v1 enum values, C
layouts, source signatures, imported symbol names, and the invalid-null behavior
of every old entry point. `current_header_stub.c` must first observe those same
declarations in the current generated header, then implements them. Compiling,
linking, and running the pair therefore rejects changes that would break an
already-compiled/source-compatible v1 client while permitting additive V2
declarations.

From the repository root:

```sh
bash scripts/check-fightbox-ffi-v1-abi.sh
```

When a real `fightbox-ffi` static library is available, pass it after `--` with
the verified Steam Audio library directory. This complete macOS recipe also
works from an isolated Git worktree. The same frozen client is then linked
against the Rust implementation and exercises create, listener/source update,
one 128-frame mono-to-interleaved-stereo render, and destroy using the
checked-in Chicago control package:

```sh
canonical_checkout="$(dirname "$(git rev-parse --git-common-dir)")"
export STEAM_AUDIO_SDK_DIR="$canonical_checkout/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
cargo build -p fightbox-ffi --release
sdk_lib="$STEAM_AUDIO_SDK_DIR/lib/osx"
bash scripts/check-fightbox-ffi-v1-abi.sh -- \
  target/release/libfightbox_ffi.a \
  -L"$sdk_lib" -lphonon -Wl,-rpath,"$sdk_lib"
```

The default check deliberately requires neither cbindgen nor the Steam Audio
SDK. It proves C source/layout/symbol compatibility, not the Rust archive's
export table; the optional real-library mode closes that final link gap.
