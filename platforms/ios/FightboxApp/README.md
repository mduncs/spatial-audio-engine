# FightboxApp

FightboxApp is the minimal iPhoneOS host for the frozen iOS host-app contract
in `EXECUTION.md` (2026-07-30 10:58). It creates a 48 kHz, 128-frame,
Mobile-tier Fightbox session and displays delivered-quality telemetry. On iOS
18 it first admits the exact installed Wave 17 four-cell production route, then
uses a licensed canonical source package, target-device axis calibration,
personalized-HRTF admission, and the selected Tom's Diner mono or authored-
stereo contract. The app verifies the route manifest SHA and all four strict
package/bake closures before opening audio. Any missing resource or failed
Apple gate leaves the established bundled Chicago Steam final-stereo host as an
explicitly labelled single-world fallback; that fallback is never route
qualification evidence and still renders silent host-owned source input through
an `AVAudioSourceNode`. Launching or foregrounding the app does not start audio:
the operator must press **Start Session**. The ABX tab wires the deterministic
`AbxSession` plan but intentionally leaves A/B stimuli and response capture
disabled.

The committed project source of truth is `project.yml`, not a committed
`.pbxproj`. The generated `FightboxApp.xcodeproj` is ignored. Regenerate it
after changing sources or build settings:

```sh
cd platforms/ios/FightboxApp
xcodegen generate
```

The app target directly compiles its selected integration files from the
adjacent `FightboxKit` tree. Its app-owned `FightboxSession` adapter selects
`FbQualityMobile` and serializes all listener/source/telemetry calls on one
control queue. On the Steam fallback, only `fb_session_render_block` runs on
the AVAudioSourceNode audio callback, using preallocated source-major mono and
interleaved stereo arrays. On the Apple route, the canonical provider only
copies already-decoded planar PCM into its fixed bank; filesystem reads,
SHA-256, zstd, cache replacement, and seeks remain off-callback.

## Exact installed four-cell route

The Apple production path is fail-closed to route
`wave17-locality-production-fixed-tier-mesh-v2` with manifest SHA-256
`14ef017413b52cfbd25661b81aef9d3b8b505e850a339b3721352dea21313f2a`.
Install the source-matched closure with this exact app-container layout:

```text
Application Support/Fightbox/CityRoutes/active/
├── route/city-route-manifest.json
├── packages/e0-n0.fightbox/
├── packages/e1-n0.fightbox/
├── packages/e1-n1.fightbox/
├── packages/e0-n1.fightbox/
├── bakes/e0-n0.baked/
├── bakes/e1-n0.baked/
├── bakes/e1-n1.baked/
└── bakes/e0-n1.baked/
```

The checked-in app does not vendor these generated evidence artifacts. After a
signed app is installed, stage and copy the exact retained route without
changing its package/bake closures:

```sh
route_root=/path/to/spatial-audio/evidence/wave17-locality-production-fixed-tier-mesh-v2-20260811T134214Z
stage="$(mktemp -d /tmp/fightbox-city-route.XXXXXX)"
trap 'rm -rf -- "$stage"' EXIT
mkdir -p "$stage/active"
for directory in route packages bakes; do
  /usr/bin/ditto "$route_root/$directory" "$stage/active/$directory"
done

DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer xcrun devicectl device copy to   --device A0499710-6DB7-5E64-B46B-C4FBCF9207C3   --source "$stage/active"   --destination "Library/Application Support/Fightbox/CityRoutes"   --domain-type appDataContainer   --domain-identifier com.fightbox.spatial-audio
```

`FightboxInstalledCityRoute` verifies the exact manifest and all four artifacts
before constructing the initial session. The **Run Exact 4-Cell Route** control
then performs the three authored transitions through the live neutral session.
It pauses GPS and phone-body pose input, reduces monitor gain to -60 dB, crosses
each half-open owner boundary by exactly 1 mm, waits for candidate preparation,
and refuses the next cell until the old environmental tail is collected. Apple
continues to own AirPods-relative head rotation. This is a deterministic target
lifecycle/streaming qualification path, not an acoustic comparison, personalized-
HRTF result, thermal pass, or human listening judgment.

A host-side loader check for the retained closure is:

```sh
DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer   scripts/test-wave17-ios-route-loader.sh "$route_root"
```

The script compiles the app loader with warnings as errors, calls
`FightboxInstalledCityRoute.load(rootURL:)`, and requires the exact route id,
manifest SHA, and four verified cell identities. The retained 2026-08-11 check
passed all four artifacts; it remains host file-verification evidence, not a
physical-device run.

## Apple canonical-source route

The source picker selects one of two metadata contracts. It does not bundle
copyrighted audio:

- `toms-diner.fightbox-audio`: canonical native mono rendered as a Point;
- `toms-diner-authored-stereo.fightbox-audio`: canonical authored L/R rendered
  as a StereoImage without downmixing or synthetic widening.

Install the selected package directory under:

```text
Application Support/Fightbox/CanonicalAssets/
```

The same package may be supplied as a bundle resource for a licensed internal
build. Each directory must contain the canonical `manifest.json` and indexed
one-second planar-f32 chunks produced by `fightbox asset pack`.

Production admission also requires:

```text
Application Support/Fightbox/apple-spatial-axis-calibration.json
```

The calibration uses schema
`fightbox.apple-spatial-axis-calibration.v1` and records `right_enu`,
`front_enu`, `up_enu`, an 81-float `environmental_acn_transform`,
`expected_environmental_basis`, `evidence_identifier`, and
`target_device_verified: true`. The axes must be orthonormal. Accepted basis
names are `right_handed_enu` and `steam_x_right_y_up_z_back`. A compile-only
provisional calibration is never promoted by the app.

Canonical fill is transactional. A program block advances its original asset
timeline only after the neutral backend returns a valid spatial block. An
explicit macro seek or a silent cell-swap discontinuity flushes Apple latency
history and presents the same unconsumed asset frame on the next valid block.
Legacy one-source playback still exposes
`FightboxHostModel.applyMacroArrival(programSeekFrame:)`. Production macro delivery instead uses
the opt-in 16-source V3 token transaction: the host admits typed assets, binds any package-authored
`FightboxStableSpatialKey` before prepare, seeks and stages exact readiness, commits only on the
exact control frame, and releases provider generations only after token-qualified audio ACK.

## Compile-check the Swift FFI boundary

From any checkout or git worktree, run:

```sh
platforms/ios/test-swift-ffi-boundary.sh
```

The harness builds FightboxKit through its real `FightboxC` target and
type-checks the app-local wrapper through its bridging header against that
checkout's canonical C header. It does not link the native archives or prove a
device build.

## Test the streaming coordinator

The pure `FightboxCellStreamingCoordinator` state machine is compiled from the
same source on macOS for deterministic host XCTest. The real iOS memory sampler
and UIKit pressure observer remain iOS-only; the non-iOS default sampler fails
closed, and tests inject exact memory and thermal samples. Lifecycle backend
operations other than construction are deliberately non-suspending so route
and pressure tasks cannot re-enter ownership publication.

```sh
DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer \
swift test --package-path platforms/ios/FightboxKit \
  -Xswiftc -warnings-as-errors

DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer \
swift build --package-path platforms/ios/FightboxKit \
  --triple arm64-apple-ios15.0-simulator --target FightboxKit \
  -Xswiftc -warnings-as-errors
```

The host tests cover preparation/adoption/tail completion, the two-world gate,
stale completion and memory-warning cancellation, direct memory/thermal
refusals, mismatched payload release, and truthful adoption telemetry. The
simulator command is compile-only while no simulator runtime is installed;
neither command is physical-device evidence.

## Rebuild the native archives

Install the device Rust standard library once, then use the selected full Xcode
and the exact absolute SDK root from `AGENTS.md`:

```sh
rustup target add aarch64-apple-ios
cd /path/to/spatial-audio/engine
DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer \
IPHONEOS_DEPLOYMENT_TARGET=15.0 \
STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1/steamaudio" \
cargo +stable build --release --target aarch64-apple-ios -p fightbox-ffi
```

Steam Audio's iOS static library does not contain its PFFFT or libmysofa
dependencies. Rebuild the checked-in arm64 iPhoneOS archives when updating
Xcode, the deployment target, or an upstream pin:

```sh
cd /path/to/spatial-audio/engine
DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer \
  platforms/ios/third-party/build-ios.sh
```

Exact upstream revisions, archive hashes, licenses, and the offline-source
overrides are recorded in
[`../third-party/THIRD-PARTY.md`](../third-party/THIRD-PARTY.md).

The target links:

- `target/aarch64-apple-ios/release/libfightbox_ffi.a`
- `.cache/steam-audio/steamaudio-4.8.1/steamaudio/lib/ios/libphonon.a`
- `platforms/ios/third-party/lib/ios/libpffft.a`
- `platforms/ios/third-party/lib/ios/libmysofa.a`
- the iPhoneOS SDK's `libz.tbd`

All four archives are device arm64 inputs. The Xcode project intentionally
supports `iphoneos` only; there is no simulator link or simulator-audio path.

The generated project defaults `FIGHTBOX_ARTIFACT_ROOT` to the current checkout
root. In a git worktree that reuses archives from another checkout, override it
with the absolute root that owns both `target/` and `.cache/`:

```sh
xcodebuild -project FightboxApp.xcodeproj \
  -target FightboxApp \
  -sdk iphoneos \
  -configuration Release \
  CODE_SIGNING_ALLOWED=NO \
  FIGHTBOX_ARTIFACT_ROOT=/path/to/spatial-audio/engine \
  build
```

This changes only the two generated-archive search paths. Headers and the
checked-in PFFFT/libmysofa archives continue to come from the active worktree.

## Privacy manifest

The app bundles `Resources/PrivacyInfo.xcprivacy` because the runtime's
preinitialized callback timer calls `mach_absolute_time()`. The declaration is
limited to Apple's System Boot Time reason `35F9.1`: measuring elapsed time for
an in-app audio operation. Raw counter values and boot-time-derived absolute
information never leave the device. Any other host embedding the Rust archive
must carry the same required-reason declaration.

## Refresh the bundled city package

Preserve each resource as a directory bundle. Use `/bin/cp` explicitly because
interactive `cp` may be aliased:

```sh
cd /path/to/spatial-audio/engine
/bin/cp -R /private/tmp/fightbox-app-package/chicago-block-a.fightbox \
  platforms/ios/FightboxApp/Resources/
/bin/cp -R /private/tmp/fightbox-app-package/chicago-block-baked \
  platforms/ios/FightboxApp/Resources/
```

## Unsigned device build

Xcode 27.0 beta (`27A5228h`) contains the iPhoneOS and iPhoneSimulator 27.0 SDKs
but is not the globally selected developer directory. Keep the selection local
to each command:

```sh
cd /path/to/spatial-audio/engine/platforms/ios/FightboxApp
xcodegen generate
DEVELOPER_DIR=/Applications/Xcode-beta.app/Contents/Developer \
xcodebuild -project FightboxApp.xcodeproj \
  -target FightboxApp \
  -sdk iphoneos \
  -configuration Release \
  CODE_SIGNING_ALLOWED=NO build
```

The generated target is pinned to iPhone device family `1` in both Debug and
Release configurations so XcodeGen's default iPhone+iPad preset cannot reappear.

The broad source-matched unsigned proof retained on 2026-08-11 is under
`/path/to/spatial-audio/evidence/wave17-apple-build-20260811T1742Z`
(report SHA-256
`098b53671b4bfadddc53657c9ccd81fe52c199cd3c8325ec58d9aaca763bfdf3`).
The source-matched exact-route and coordinator authority is under
`/path/to/spatial-audio/evidence/wave17-ios-route-host-coordinator-v2-20260811T185441Z`
(report SHA-256
`1371e3b3f069c0c84ccd1c16107c5f67717386ade652378337acf1c41aac8c4d`).
Together they cover the arm64 Rust iPhoneOS archive, unavailable-backend
simulator archive, Swift FFI boundary, ten host XCTest cases, 400 repeated
coordinator executions, exact installed-route verification, and warnings-as-
errors unsigned arm64 app builds. They are not a signed install, target runtime,
thermal, AirPods, or listening result.

For a physical-device run, configure the development team, enable Developer
Mode on the paired iPhone, reboot/confirm the device if requested, and use the
scheme with that exact destination. Do not treat a visible paired device as a
runtime pass. As of the retained proof, md's iPhone is paired and booted but
Developer Mode is disabled, and the project has no development team configured.
Install and launch therefore remain blocked.
