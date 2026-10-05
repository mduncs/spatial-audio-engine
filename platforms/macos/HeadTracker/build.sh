#!/bin/bash
set -euo pipefail

source_dir="$(cd "$(dirname "$0")" && pwd)"
repo_dir="$(cd "$source_dir/../../.." && pwd)"
app_dir="$repo_dir/target/HeadTracker.app"
binary="$app_dir/Contents/MacOS/HeadTracker"

fresh=true
for input in "$source_dir"/*.swift "$source_dir/mapping-cases.tsv" "$source_dir/Info.plist" "$source_dir/build.sh"; do
    if [[ ! -x "$binary" || ! "$binary" -nt "$input" ]]; then fresh=false; break; fi
done
if $fresh && /usr/bin/codesign --verify --strict "$app_dir" 2>/dev/null; then
    printf '%s\n' "$app_dir"
    exit 0
fi

mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources" "$repo_dir/target/headtracker-module-cache"
cp "$source_dir/Info.plist" "$app_dir/Contents/Info.plist"
cp "$source_dir/mapping-cases.tsv" "$app_dir/Contents/Resources/mapping-cases.tsv"
echo "Building and ad-hoc signing HeadTracker…" >&2
xcrun swiftc -swift-version 6 -O -whole-module-optimization \
    -target "$(uname -m)-apple-macosx14.0" \
    -module-cache-path "$repo_dir/target/headtracker-module-cache" \
    -framework Foundation -framework CoreMotion \
    "$source_dir"/*.swift -o "$binary"
/usr/bin/codesign --force --sign - "$app_dir" >&2
/usr/bin/codesign --verify --strict "$app_dir"
printf '%s\n' "$app_dir"
