# FightboxApp

FightboxApp is the minimal iPhoneOS host for the frozen iOS host-app contract
in `EXECUTION.md` (2026-07-30 10:58). It creates a 48 kHz, 128-frame,
Mobile-tier Fightbox session from the bundled Chicago package, renders silent
host-owned source input through an `AVAudioSourceNode`, feeds listener motion
and GPS/local-ENU position into the session, and displays delivered-quality
telemetry. The ABX tab wires the deterministic `AbxSession` plan but
intentionally leaves A/B stimuli and response capture disabled.

The committed project source of truth is `project.yml`, not a committed
`.pbxproj`. The generated `FightboxApp.xcodeproj` is ignored. Regenerate it
after changing sources or build settings:

```sh
cd platforms/ios/FightboxApp
xcodegen generate
```

The app target directly compiles the existing
`CoreMotionHeadTracker.swift`, `GpsLocalEnuProvider.swift`, and
`AbxSession.swift` files from the adjacent `FightboxKit` tree. Its app-owned
`FightboxSession` adapter selects `FbQualityMobile` and serializes all
listener/source/telemetry calls on one control queue. Only
`fb_session_render_block` runs on the AVAudioSourceNode audio callback, using
preallocated source-major mono and interleaved stereo arrays.

## Rebuild the native archives

The vendor SDK path must be absolute:

```sh
cd /Users/md/code/spatial-audio-engine
IPHONEOS_DEPLOYMENT_TARGET=15.0 \
STEAM_AUDIO_SDK_DIR=/Users/md/code/spatial-audio-engine/.cache/steam-audio/steamaudio-4.8.1 \
cargo +stable build --release --target aarch64-apple-ios -p fightbox-ffi
```

Steam Audio's iOS static library does not contain its PFFFT or libmysofa
dependencies. Rebuild the checked-in arm64 iPhoneOS archives when updating
Xcode, the deployment target, or an upstream pin:

```sh
cd /Users/md/code/spatial-audio-engine
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

## Refresh the bundled city package

Preserve each resource as a directory bundle. Use `/bin/cp` explicitly because
interactive `cp` may be aliased:

```sh
cd /Users/md/code/spatial-audio-engine
/bin/cp -R /private/tmp/fightbox-app-package/chicago-block-a.fightbox \
  platforms/ios/FightboxApp/Resources/
/bin/cp -R /private/tmp/fightbox-app-package/chicago-block-baked \
  platforms/ios/FightboxApp/Resources/
```

## Unsigned device build

On this machine, Xcode 26.6 has the iPhoneOS 26.5 SDK but not the separately
downloadable iOS 26.5 platform component. Use the target-based invocation for
the current unsigned build proof:

```sh
cd /Users/md/code/spatial-audio-engine/platforms/ios/FightboxApp
xcodegen generate
xcodebuild -project FightboxApp.xcodeproj \
  -target FightboxApp \
  -sdk iphoneos26.5 \
  -configuration Release \
  CODE_SIGNING_ALLOWED=NO build
```

Once the matching platform component is installed, the preferred destination-
based form is:

```sh
xcodebuild -project FightboxApp.xcodeproj \
  -scheme FightboxApp \
  -destination 'generic/platform=iOS' \
  -configuration Release \
  CODE_SIGNING_ALLOWED=NO build
```

Without that component, the scheme/destination form fails with
`iOS 26.5 is not installed`; changing the link inputs does not repair that
Xcode installation issue.

For a device run, open the generated project, choose a development team, and
run on an arm64 iPhone. Signing, deployment, simulator support, App Store
metadata, and authored ABX stimuli are outside this target.
