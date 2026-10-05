#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <wave17-locality-candidate-directory>" >&2
  exit 64
fi
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
candidate="$(python3 - "$1" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).expanduser().resolve())
PY
)"
if [[ "$candidate" == "$repo" || "$candidate" == "$repo/"* ]]; then
  echo "wave17 oracle artifacts must remain outside the repository: $candidate" >&2
  exit 1
fi
for path in \
  "$candidate/candidate-summary.json" \
  "$candidate/four-cell-baked-v2/city-route-manifest.json" \
  "$candidate/streamed-bake-summary.json"; do
  if [[ ! -f "$path" ]]; then
    echo "missing completed Wave 17 streamed-bake input: $path" >&2
    exit 1
  fi
done
output="$candidate/monolithic-oracle-v1"
if [[ -e "$output" ]]; then
  echo "refusing to replace existing monolithic oracle: $output" >&2
  exit 1
fi

export STEAM_AUDIO_SDK_DIR="$repo/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
export RUST_MIN_STACK=16777216
cd "$repo"
echo "wave17-oracle: building linked fightbox CLI" >&2
cargo build -q -p fightbox-cli --features linked-sdk
fightbox="$repo/target/debug/fightbox"

echo "wave17-oracle: 1,750 m monolithic bake start" >&2
"$fightbox" city oracle-bake \
  --geojson "$repo/fixtures/city/wave17-locality/route-city.geojson" \
  --probe-policy "$repo/fixtures/city/wave17-locality/graded-probe-policy.json" \
  --route-manifest "$candidate/four-cell-baked-v2/city-route-manifest.json" \
  --cell-package "$candidate/packages/e0-n0.fightbox" \
  --cell-package "$candidate/packages/e1-n0.fightbox" \
  --cell-package "$candidate/packages/e1-n1.fightbox" \
  --cell-package "$candidate/packages/e0-n1.fightbox" \
  --cell-bake "$candidate/bakes/e0-n0.baked" \
  --cell-bake "$candidate/bakes/e1-n0.baked" \
  --cell-bake "$candidate/bakes/e1-n1.baked" \
  --cell-bake "$candidate/bakes/e0-n1.baked" \
  --bake-threads 4 \
  --output "$output"
"$fightbox" city oracle-verify --artifact "$output"
echo "wave17-oracle: complete $output/city-oracle-manifest.json" >&2
