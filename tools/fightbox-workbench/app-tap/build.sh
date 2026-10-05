#!/bin/bash
set -euo pipefail

source_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$source_dir/../../.." && pwd)"
build_dir="$repo_dir/target/app-tap"
app_dir="$build_dir/FightboxAudioTap.app"
binary="$app_dir/Contents/MacOS/FightboxAudioTap"

fresh=true
for input in "$source_dir"/*.swift "$source_dir"/*.c "$source_dir"/*.h "$source_dir/Info.plist" "$source_dir/build.sh"; do
    if [[ ! -x "$binary" || ! "$binary" -nt "$input" ]]; then fresh=false; break; fi
done
if $fresh && /usr/bin/codesign --verify --strict "$app_dir" 2>/dev/null; then
    printf '%s\n' "$app_dir"
    exit 0
fi

mkdir -p "$app_dir/Contents/MacOS" "$build_dir/module-cache"
cp "$source_dir/Info.plist" "$app_dir/Contents/Info.plist"
echo "Building and ad-hoc signing Fightbox City Audio…" >&2
xcrun clang -std=c11 -O2 -Wall -Wextra -Werror \
    -target "$(uname -m)-apple-macosx14.0" \
    -c "$source_dir/TapBridge.c" -o "$build_dir/TapBridge.o"
xcrun swiftc -swift-version 6 -O -whole-module-optimization \
    -target "$(uname -m)-apple-macosx14.0" \
    -module-cache-path "$build_dir/module-cache" \
    -import-objc-header "$source_dir/TapBridge.h" \
    -framework Foundation -framework AppKit -framework CoreAudio \
    "$source_dir"/*.swift "$build_dir/TapBridge.o" -o "$binary"
/usr/bin/codesign --force --sign - --identifier dev.fightbox.AudioTap \
    --requirements '=designated => identifier "dev.fightbox.AudioTap"' "$app_dir" >&2
/usr/bin/codesign --verify --strict "$app_dir"
printf '%s\n' "$app_dir"
