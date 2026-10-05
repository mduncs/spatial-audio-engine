#!/usr/bin/env bash
# Compile the legacy Swift FFI consumers plus the additive V2 preparation and
# V3 tokened macro-production surfaces against the canonical C ABI.
# This is deliberately compile-only: it neither links, launches Xcode/Simulator,
# nor requires the Rust, Steam Audio, PFFFT, or libmysofa device archives.
# Every source path is derived from this checkout, so isolated git worktrees do
# not depend on generated artifacts stored in a different checkout.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
KIT_WRAPPER="$SCRIPT_DIR/FightboxKit/Sources/FightboxKit/FightboxSession.swift"
KIT_V2_CONTROL_PROBE="$SCRIPT_DIR/FightboxKit/Sources/FightboxKit/V2PreparationABIProbe.swift"
KIT_PACKAGE="$SCRIPT_DIR/FightboxKit"
KIT_PACKAGE_MANIFEST="$KIT_PACKAGE/Package.swift"
KIT_SHIM="$KIT_PACKAGE/Sources/FightboxC/include/fightbox_shim.h"
APP_WRAPPER="$SCRIPT_DIR/FightboxApp/Sources/FightboxSession.swift"
APP_V2_CONTROL_PROBE="$SCRIPT_DIR/FightboxApp/Sources/V2PreparationABIProbe.swift"
APP_BRIDGING_HEADER="$SCRIPT_DIR/FightboxApp/Sources/FightboxApp-Bridging-Header.h"
CANONICAL_HEADER_DIR="$REPO_ROOT/crates/fightbox-ffi/include"
CANONICAL_HEADER="$CANONICAL_HEADER_DIR/fightbox.h"

skip() {
  echo "SKIP swift-ffi-boundary: $1"
  exit 0
}

fail() {
  echo "FAIL swift-ffi-boundary: $1" >&2
  exit 1
}

command -v xcrun >/dev/null 2>&1 || skip "xcrun is unavailable (Apple toolchain missing)"

if ! SWIFTC="$(xcrun --find swiftc 2>/dev/null)" || [[ ! -x "$SWIFTC" ]]; then
  skip "swiftc is unavailable through xcrun"
fi
if ! SWIFT="$(xcrun --find swift 2>/dev/null)" || [[ ! -x "$SWIFT" ]]; then
  skip "Swift Package Manager is unavailable through xcrun"
fi
if ! IOS_SDK="$(xcrun --sdk iphoneos --show-sdk-path 2>/dev/null)" \
  || [[ ! -d "$IOS_SDK" ]]; then
  skip "the iPhoneOS SDK is unavailable through xcrun"
fi
if ! "$SWIFTC" --version >/dev/null 2>&1; then
  fail "the selected Swift compiler exists but cannot run"
fi

for required in \
  "$CANONICAL_HEADER" \
  "$KIT_WRAPPER" \
  "$KIT_V2_CONTROL_PROBE" \
  "$KIT_PACKAGE_MANIFEST" \
  "$KIT_SHIM" \
  "$APP_WRAPPER" \
  "$APP_V2_CONTROL_PROBE" \
  "$APP_BRIDGING_HEADER"; do
  [[ -f "$required" ]] || fail "required repository input is missing: $required"
done

SCRATCH="$(mktemp -d "${TMPDIR:-/tmp}/fightbox-swift-ffi.XXXXXX")"
cleanup() {
  rm -rf -- "$SCRATCH"
}
trap cleanup EXIT

COMMON_FLAGS=(
  -target arm64-apple-ios15.0
  -sdk "$IOS_SDK"
  -module-cache-path "$SCRATCH/module-cache"
  -parse-as-library
  -typecheck
  -warnings-as-errors
  -Xcc -Werror
)

echo "CHECK FightboxKit legacy, V2 preparation, and V3 macro-production surfaces through FightboxC"
"$SWIFT" build \
  --package-path "$KIT_PACKAGE" \
  --scratch-path "$SCRATCH/swiftpm" \
  --triple arm64-apple-ios15.0 \
  --sdk "$IOS_SDK" \
  --target FightboxKit \
  -Xswiftc -warnings-as-errors \
  -Xcc -Werror
echo "PASS  FightboxKit legacy, V2 preparation, and V3 macro-production surfaces through FightboxC"

echo "CHECK FightboxApp-local wrapper and V2 control-boundary probe through bridging header"
"$SWIFTC" \
  "${COMMON_FLAGS[@]}" \
  -swift-version 5 \
  -import-objc-header "$APP_BRIDGING_HEADER" \
  -Xcc -I \
  -Xcc "$CANONICAL_HEADER_DIR" \
  "$APP_WRAPPER" \
  "$APP_V2_CONTROL_PROBE"
echo "PASS  FightboxApp-local wrapper and V2 control-boundary probe through bridging header"

echo "PASS  swift-ffi-boundary: legacy, V2 preparation, and V3 macro-production surfaces compile (no link proof)"
