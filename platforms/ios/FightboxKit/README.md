# FightboxKit iOS integration skeleton

FightboxKit contains the Swift ownership wrapper, a compile-ready Core Motion
head-tracking sample, a foreground GPS/local-ENU provider, and a seeded ABX
session-record scaffold. It deliberately does not provide an app, an
`AVAudioEngine`/Core Audio unit, device deployment, or simulator audio.

The minimum supported deployment target for this package is iOS 15.0. Steam
Audio 4.8.1 itself supports iOS 11.0, but this wrapper standardizes on iOS 15.

## Build the Rust device archive

From the repository root:

```sh
export STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1"
export IPHONEOS_DEPLOYMENT_TARGET=15.0
cargo +stable build -p fightbox-ffi --release --target aarch64-apple-ios
```

The resulting archive is:

```text
target/aarch64-apple-ios/release/libfightbox_ffi.a
```

Valve's device-only Steam Audio archive is:

```text
.cache/steam-audio/steamaudio-4.8.1/steamaudio/lib/ios/libphonon.a
```

The pinned archive contains iPhoneOS arm64 objects, not iOS Simulator objects.
The Rust ABI can therefore be compile-checked for `aarch64-apple-ios-sim`, but
simulator audio cannot link against this vendor archive.

The repository-pinned `1.91.1` toolchain currently has only the macOS standard
library installed; the otherwise identical `stable` toolchain owns both iOS
targets, hence `+stable` above. The device build also requires the backend
`build.rs` to select `lib/ios/libphonon.a` and emit
`cargo:rustc-link-lib=static=phonon` when `CARGO_CFG_TARGET_OS=ios`. Until that
three-branch link selection is added in its owning lane, the build stops while
incorrectly searching for `libphonon.so`.

## Xcode link settings

Add this directory as a local Swift package. Then add both static archives to
the app target's **Link Binary With Libraries** phase:

1. `target/aarch64-apple-ios/release/libfightbox_ffi.a`
2. `.cache/steam-audio/steamaudio-4.8.1/steamaudio/lib/ios/libphonon.a`

Set **Header Search Paths** to:

```text
$(SRCROOT)/path/to/spatial-audio-engine/crates/fightbox-ffi/include
```

Set **Library Search Paths** to the two archive directories above, or add the
archives by absolute file reference. Add `-lc++` to **Other Linker Flags** for
Steam Audio's C++ implementation. Link `CoreMotion.framework`; the SwiftPM
target declares it already. Keep the app deployment target at iOS 15.0 or
later, matching `IPHONEOS_DEPLOYMENT_TARGET` used for Rust.

The package's `FightboxC` shim includes the canonical generated header directly
from the monorepo. To regenerate that header after changing the Rust ABI:

```sh
cbindgen --config crates/fightbox-ffi/cbindgen.toml \
  --crate fightbox-ffi \
  --output crates/fightbox-ffi/include/fightbox.h
```

## Audio callback shape

Create `FightboxSession` on a serialized control queue before starting audio.
Keep one `[Float]` input buffer sized `sourceCount * blockSizeFrames` and one
stereo output buffer sized `blockSizeFrames * 2`; allocate both before the
callback. Fill the input in source-major mono order, then call:

```swift
try session.render(sourceMajorMono: sourceBlock, into: &stereoBlock)
```

The wrapper and C renderer do not allocate in this call when the arrays already
have the exact required sizes. Copy `stereoBlock` into the host
`AudioBufferList`. Do not call source/listener updates, telemetry, filesystem
APIs, or session destruction from the audio callback.

Drive `updateListener`, `updateSource`, and `telemetryJSON` from one serialized
control queue. `CoreMotionHeadTracker` demonstrates listener orientation
updates. `GpsLocalEnuProvider` fixes its origin at the first fresh fix with no
more than 20 m horizontal uncertainty and supplies the tracker's ENU position;
it requests foreground when-in-use location only. The host app must include an
`NSLocationWhenInUseUsageDescription` string.

`AbxSession` creates deterministic, seeded A/B/X assignments and emits the
`fightbox.abx.v1` evidence record. It does not play audio: the host presents its
own A and B stimuli according to each trial plan and captures the listener's
forced choice through the scaffold.

Stop the audio unit, location provider, and head tracker, join their queues, and
only then release the final `FightboxSession` reference.
