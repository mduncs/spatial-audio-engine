#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <new-output-directory>" >&2
  exit 64
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output="$(python3 - "$1" <<'PY'
from pathlib import Path
import sys
print(Path(sys.argv[1]).expanduser().resolve())
PY
)"
if [[ "$output" == "$repo" || "$output" == "$repo/"* ]]; then
  echo "wave17 locality output must be outside the repository working tree: $output" >&2
  exit 1
fi
if [[ -e "$output" ]]; then
  echo "wave17 locality output already exists: $output" >&2
  exit 1
fi
parent="$(dirname "$output")"
mkdir -p "$parent"
staging="$parent/.$(basename "$output").partial.$$"
if [[ -e "$staging" ]]; then
  echo "wave17 locality staging path already exists: $staging" >&2
  exit 1
fi
trap 'rm -rf "$staging"' EXIT
mkdir -p "$staging/packages"

cd "$repo"
echo "wave17-locality: building fightbox CLI" >&2
cargo build -q -p fightbox-cli
fightbox="$repo/target/debug/fightbox"
source_geojson="$repo/fixtures/city/wave17-locality/route-city.geojson"
probe_policy="$repo/fixtures/city/wave17-locality/graded-probe-policy.json"

compile_cell() {
  local east="$1"
  local north="$2"
  local cell_output="$staging/packages/e${east}-n${north}.fightbox"
  echo "wave17-locality: compile cell e${east}:n${north}" >&2
  "$fightbox" city compile-v2 \
    --geojson "$source_geojson" \
    --output "$cell_output" \
    --city-id wave17-locality \
    --origin-latitude-degrees 41.881832 \
    --origin-longitude-degrees -87.623177 \
    --origin-altitude-m 181 \
    --cell-east-index "$east" \
    --cell-north-index "$north" \
    --probe-policy "$probe_policy"
}

route_args=()
for east in $(seq 0 20); do
  compile_cell "$east" 0
  route_args+=(--cell-package "$staging/packages/e${east}-n0.fightbox")
done
compile_cell 0 1
compile_cell 1 1

echo "wave17-locality: assemble 21-cell 10 km route candidate" >&2
"$fightbox" city route-assemble \
  --route-id wave17-10km-east \
  "${route_args[@]}" \
  --owner-home-cell wave17-locality:e0:n0 \
  --output "$staging/route-10km"

echo "wave17-locality: assemble four-cell seam/oracle candidate" >&2
"$fightbox" city route-assemble \
  --route-id wave17-four-cell-seam \
  --cell-package "$staging/packages/e0-n0.fightbox" \
  --cell-package "$staging/packages/e1-n0.fightbox" \
  --cell-package "$staging/packages/e1-n1.fightbox" \
  --cell-package "$staging/packages/e0-n1.fightbox" \
  --owner-home-cell wave17-locality:e0:n0 \
  --four-cell-fixture \
  --output "$staging/four-cell"

python3 - \
  "$source_geojson" \
  "$probe_policy" \
  "$staging/route-10km/city-route-manifest.json" \
  "$staging/four-cell/city-route-manifest.json" \
  "$staging/candidate-summary.json" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

source, policy, route_path, four_path, output = map(Path, sys.argv[1:])
route = json.loads(route_path.read_text())
four = json.loads(four_path.read_text())
fixture = four["four_cell_fixture"]
assert len(route["cells"]) == 21
assert route["owner_home"]["cell_id"] == "wave17-locality:e0:n0"
assert route["owner_home"]["tier_id"] == "owner-home"
assert len(four["cells"]) == 4 and len(four["adjacencies"]) == 4
assert fixture["state"] == "plan_only" and fixture["bakes_launched"] is False
assert fixture["monolithic_oracle_path_range_m"] == 1750
assert fixture["monolithic_oracle_bounds_city_enu_mm"] == {
    "min": [-342500, -342500],
    "max": [827500, 827500],
}
policy_hashes = {cell["city_bake"]["placement_policy_sha256"] for cell in route["cells"]}
assert len(policy_hashes) == 1
assert all(
    cell["installed_size"]["projected_remaining_bake_bytes"] <= 64 * 1024 * 1024
    for cell in route["cells"]
)
sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
summary = {
    "schema_version": "fightbox.wave17-locality-candidates.v1",
    "artifact_state": "package_probe_plan_candidates",
    "bakes_launched": False,
    "city_id": route["city_id"],
    "source_geojson_sha256": sha(source),
    "graded_probe_policy_sha256": sha(policy),
    "resolved_placement_policy_sha256": next(iter(policy_hashes)),
    "package_count": 23,
    "route": {
        "route_id": route["route_id"],
        "cell_count": len(route["cells"]),
        "route_centerline_length_m": 20 * 485,
        "probe_footprint_union_length_m": 20 * 485 + 585,
        "owner_home_cell_id": route["owner_home"]["cell_id"],
        "manifest_sha256": sha(route_path),
        "projected_complete_installed_bytes": route["installed_totals"][
            "projected_complete_installed_bytes"
        ],
    },
    "four_cell_fixture": {
        "route_id": four["route_id"],
        "cell_count": len(four["cells"]),
        "seam_count": len(four["adjacencies"]),
        "manifest_sha256": sha(four_path),
        "streamed_union_bounds_city_enu_mm": fixture[
            "streamed_union_bounds_city_enu_mm"
        ],
        "monolithic_oracle_bounds_city_enu_mm": fixture[
            "monolithic_oracle_bounds_city_enu_mm"
        ],
        "monolithic_oracle_path_range_m": fixture[
            "monolithic_oracle_path_range_m"
        ],
        "streamed_cell_bakes_required": fixture["streamed_cell_bakes_required"],
        "monolithic_oracle_bakes_required": fixture[
            "monolithic_oracle_bakes_required"
        ],
    },
}
output.write_text(json.dumps(summary, indent=2) + "\n")
PY

mv "$staging" "$output"
trap - EXIT
echo "wave17-locality: complete $output/candidate-summary.json" >&2
