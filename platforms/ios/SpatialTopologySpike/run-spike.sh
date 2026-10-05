#!/bin/zsh
set -euo pipefail

spike_dir=${0:A:h}
spike_source="$spike_dir/spatial_topology_spike.c"
spike_tmp=$(mktemp -d "${TMPDIR:-/tmp}/fightbox-spatial-topology.XXXXXX")
trap 'rm -rf "$spike_tmp"' EXIT

ios_sdk=$(xcrun --sdk iphoneos --show-sdk-path)
ios_version=$(xcrun --sdk iphoneos --show-sdk-version)

printf 'Cross-linking against iPhoneOS %s for arm64 / iOS 15...\n' "$ios_version"
xcrun --sdk iphoneos clang \
    -std=c11 \
    -Wall \
    -Wextra \
    -Werror \
    -arch arm64 \
    -miphoneos-version-min=15.0 \
    -isysroot "$ios_sdk" \
    "$spike_source" \
    -framework AudioToolbox \
    -framework CoreAudio \
    -o "$spike_tmp/spatial-topology-ios"
file "$spike_tmp/spatial-topology-ios"

printf 'Building and running the matching macOS AUSpatialMixer probe...\n'
xcrun --sdk macosx clang \
    -std=c11 \
    -Wall \
    -Wextra \
    -Werror \
    "$spike_source" \
    -framework AudioToolbox \
    -framework CoreAudio \
    -o "$spike_tmp/spatial-topology-macos"
"$spike_tmp/spatial-topology-macos"
