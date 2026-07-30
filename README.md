# Spatial Audio Engine V3

This is the new Fightbox implementation selected by
[`V3-AUTHORITY-NOTE.md`](../spatial-fightbox/V3-AUTHORITY-NOTE.md): a Rust-owned engine boundary
around Steam Audio 4.8.1, built for controlled evidence before city-scale product work.

The first implementation slice is working. It includes:

- an SDK-neutral domain API;
- a contained Steam Audio FFI/RAII crate;
- verified, idempotent 4.8.1 SDK acquisition;
- target-aware dynamic linking and a real `IPLContext` lifecycle test;
- honest runtime and evidence types;
- a machine-readable status CLI; and
- strict S0 free-field and S3 building-corner fixture definitions.

S0 and S3 have **not** run. No path bake, serialized probe reload, rendered stems, or audible
around-corner claim exists yet. The next milestone is the full S3 gate in
[`EXECUTION.md`](EXECUTION.md).

## Verify the portable build

```sh
rustup run stable cargo fmt --check
rustup run stable cargo check --workspace --all-targets
rustup run stable cargo test --workspace
rustup run stable cargo run -p fightbox-cli -- status
```

## Acquire and verify Steam Audio

```sh
scripts/acquire-steam-audio.sh
```

The script prints `STEAM_AUDIO_SDK_DIR=...` after verifying the official archive's exact byte
length and SHA-256. It stores the 173 MiB archive and extracted SDK under ignored `.cache/`.

## Verify the linked build

```sh
export STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1"
rustup run stable cargo test --workspace --features linked-sdk
rustup run stable cargo run -p fightbox-cli --features linked-sdk -- status
```

The linked CLI should report the backend as available while keeping S0 and S3 at `not_run`.
