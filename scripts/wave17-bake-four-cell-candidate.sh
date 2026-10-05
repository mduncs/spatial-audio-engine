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
  echo "wave17 bake artifacts must remain outside the repository: $candidate" >&2
  exit 1
fi
if [[ ! -f "$candidate/candidate-summary.json" ]]; then
  echo "not a Wave 17 locality candidate: $candidate" >&2
  exit 1
fi
for path in "$candidate/bakes" "$candidate/four-cell-baked" "$candidate/streamed-bake-summary.json"; do
  if [[ -e "$path" ]]; then
    echo "refusing to replace existing streamed-bake artifact: $path" >&2
    exit 1
  fi
done

export STEAM_AUDIO_SDK_DIR="$repo/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
export RUST_MIN_STACK=16777216
cd "$repo"
echo "wave17-four-cell: building linked fightbox CLI" >&2
cargo build -q -p fightbox-cli --features linked-sdk
fightbox="$repo/target/debug/fightbox"
mkdir -p "$candidate/bakes"
for cell in e0-n0 e1-n0 e1-n1 e0-n1; do
  echo "wave17-four-cell: bake start $cell" >&2
  "$fightbox" city bake-v2     --package "$candidate/packages/$cell.fightbox"     --output "$candidate/bakes/$cell.baked"     --bake-threads 4
  echo "wave17-four-cell: bake complete $cell" >&2
done

"$fightbox" city route-assemble   --route-id wave17-four-cell-seam-baked   --cell-package "$candidate/packages/e0-n0.fightbox"   --cell-bake "$candidate/bakes/e0-n0.baked"   --cell-package "$candidate/packages/e1-n0.fightbox"   --cell-bake "$candidate/bakes/e1-n0.baked"   --cell-package "$candidate/packages/e1-n1.fightbox"   --cell-bake "$candidate/bakes/e1-n1.baked"   --cell-package "$candidate/packages/e0-n1.fightbox"   --cell-bake "$candidate/bakes/e0-n1.baked"   --owner-home-cell wave17-locality:e0:n0   --four-cell-fixture   --output "$candidate/four-cell-baked"

python3 - "$candidate" <<'PY'
import hashlib
import json
from pathlib import Path
import sys
root = Path(sys.argv[1])
manifest_path = root / "four-cell-baked/city-route-manifest.json"
manifest = json.loads(manifest_path.read_text())
fixture = manifest["four_cell_fixture"]
assert manifest["installed_totals"]["completed_cell_count"] == 4
assert fixture["state"] == "streamed_cell_bakes_complete_oracle_pending"
assert fixture["bakes_launched"] is True
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
bakes = {}
for cell in ("e0-n0", "e1-n0", "e1-n1", "e0-n1"):
    sidecar = root / f"bakes/{cell}.baked/capabilities/city-bake-v2.json"
    payload = root / f"bakes/{cell}.baked/probe-batch.bin"
    metadata = json.loads(sidecar.read_text())
    bakes[cell] = {
        "probe_count": metadata["probe_batch"]["probe_count"],
        "serialized_size_bytes": metadata["probe_batch"]["serialized_size_bytes"],
        "probe_batch_sha256": sha(payload),
        "completed_sidecar_sha256": sha(sidecar),
    }
summary = {
    "schema_version": "fightbox.wave17-streamed-bake-candidate.v1",
    "artifact_state": fixture["state"],
    "bakes_launched": fixture["bakes_launched"],
    "completed_streamed_cell_count": 4,
    "monolithic_oracle_complete": False,
    "four_cell_manifest_sha256": sha(manifest_path),
    "actual_installed_bytes": manifest["installed_totals"]["actual_installed_bytes"],
    "bakes": bakes,
}
(root / "streamed-bake-summary.json").write_text(json.dumps(summary, indent=2) + "\n")
PY

echo "wave17-four-cell: complete $candidate/streamed-bake-summary.json" >&2
