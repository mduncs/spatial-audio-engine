# FightboxKit iOS integration skeleton

FightboxKit contains the Swift ownership wrappers, Core Motion integration, a
foreground GPS/local-ENU provider, a seeded ABX session-record scaffold, and an
iOS 18 `AUSpatialMixer` adapter for the V2 neutral spatial route. It does not
provide device deployment or simulator audio.

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
targets, hence `+stable` above. The backend `build.rs` now selects
`lib/ios/libphonon.a` for `CARGO_CFG_TARGET_OS=ios` and emits
`cargo:rustc-link-lib=static=phonon`; device builds still require the absolute
verified SDK path and the installed `aarch64-apple-ios` Rust target.

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

## Apple neutral spatial route

`FightboxNeutralSpatialSession` consumes the fixed 48-plane presentation bank
and the 1/4/9-channel ACN/N3D environmental bank. `AppleSpatialMixerAdapter`
maps valid presentation planes to mono point inputs, converts N3D to SN3D once,
latency-aligns the point and environmental streams in preallocated storage, and
feeds the environmental prefix to one Ambisonic input. Apple owns the only final
HRTF and automatic AirPods-relative rotation. `AppleBodyMotionTracker` supplies
phone/body orientation and never starts `CMHeadphoneMotionManager`.

The production app still starts the legacy Steam final-stereo host. Apple route
admission requires iOS 18, headphones, successful automatic-head-tracking and
personalized-HRTF properties, and target-device cardinal-axis evidence. The
compile-only provisional ENU mapping is available for lab work but cannot
promote the route. The app target includes the head-pose and spatial-profile
entitlements and `AVGameBypassSystemSpatialAudio` to prevent an additional iOS
18 game-spatialization pass.

## Cell streaming host

`FightboxCellStreamingCoordinator` owns one active cell and at most one queued,
preparing, or prepared neighbor. Every admission and completion takes a new
`os_proc_available_memory()` plus physical-footprint sample. Serious or critical
thermal pressure, an iOS memory-warning cooldown, the 64 MiB raw-cell limit,
the 512 MiB active-plus-prepared target, and the 640 MiB preparation peak can
all refuse or shed the optional neighbor. A stale route direction cancels its
in-flight preparation before the replacement begins. Once a cell is adopted,
the old cell enters `TailRetiring`; no third world is admitted until the backend
declares that tail complete.

`FightboxNeutralSessionCellBackend` drives the live neutral session through the
complete control-side cell seam:

```text
fb_session_prepare_cell_v2(active_session, package_path, bake_path,
                           cell_config, out_prepared_cell)
fb_session_offer_prepared_cell_v2(active_session, prepared_cell)
fb_session_cell_stream_state_v2(active_session, out_state)
fb_session_collect_retired_cell_v2(active_session)
fb_prepared_cell_destroy_v2(unoffered_prepared_cell)
```

The prepared-cell handle is opaque and control-thread-owned. `offer` consumes
it only on success; the callback adopts it at a block boundary. `out_state`
exposes prepared, crossfade, TailRetiring, and TailComplete without touching SDK
handles. `collect` destroys only a TailComplete generation on the control
thread, while cancellation destroys an unoffered handle explicitly. World I/O,
Steam construction, priming, and destruction never move onto the callback.
