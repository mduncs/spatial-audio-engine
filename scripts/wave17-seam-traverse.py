#!/usr/bin/env python3
"""Wave 17 four-cell/oracle seam traversal evidence harness.

This harness deliberately separates two evidence classes:

* route/mechanical: exact 0.5 m sample ownership, 60 Hz controls, 15 Hz
  routing, stale-wrong-neighbor fallback, and the two-world ceiling;
* offline acoustic: fresh-process linked-SDK renders through each streamed
  cell and the separate desktop oracle, compared at the same route samples.

It does not claim a live callback, device, iOS, or production crossfade result.
Capture bundles are required to live outside the source tree.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path
from typing import Any

SAMPLE_RATE = 48_000
CONTROL_HZ = 60
ROUTE_HZ = 15
SPATIAL_STEP_M = 0.5
SPEED_MPS = 7.5  # exactly 0.5 m per 15 Hz routing sample
SIDE_M = 100.0
SPLIT_M = 242.5
SOURCE_CITY = (242.0, 242.0, 1.5)
CELL_IDS = ["wave17-locality:e0:n0", "wave17-locality:e1:n0",
            "wave17-locality:e1:n1", "wave17-locality:e0:n1"]
CELL_SHORT = {cell: cell.split(":", 1)[1].replace(":", "-") for cell in CELL_IDS}
CELL_OFFSET = {
    "wave17-locality:e0:n0": (0.0, 0.0),
    "wave17-locality:e1:n0": (485.0, 0.0),
    "wave17-locality:e1:n1": (485.0, 485.0),
    "wave17-locality:e0:n1": (0.0, 485.0),
}
# Four 100 m legs visit all four owners and put samples exactly on each switch.
WAYPOINTS = [(192.5, 192.5, 1.5), (292.5, 192.5, 1.5),
             (292.5, 292.5, 1.5), (192.5, 292.5, 1.5),
             (192.5, 392.5, 1.5)]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1024 * 1024), b""):
            h.update(chunk)
    return h.hexdigest()


def canonical_sha(value: Any) -> str:
    return hashlib.sha256((json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()).hexdigest()


def run(cmd: list[str], env: dict[str, str], *, cwd: Path) -> dict[str, Any]:
    started = time.monotonic()
    proc = subprocess.run(cmd, cwd=cwd, env=env, text=True, capture_output=True)
    result = {"command": cmd, "returncode": proc.returncode,
              "duration_s": time.monotonic() - started,
              "stdout": proc.stdout[-4000:], "stderr": proc.stderr[-4000:]}
    if proc.returncode:
        raise RuntimeError(json.dumps(result, indent=2))
    return result


def route_points() -> list[dict[str, Any]]:
    # Include each leg's start, exclude duplicate starts on later legs.
    points: list[dict[str, Any]] = []
    sample = 0
    for leg, (start, end) in enumerate(zip(WAYPOINTS, WAYPOINTS[1:])):
        dx, dy = end[0] - start[0], end[1] - start[1]
        distance = math.hypot(dx, dy)
        count = round(distance / SPATIAL_STEP_M)
        for j in range(count + 1):
            if leg > 0 and j == 0:
                continue
            fraction = (j * SPATIAL_STEP_M) / distance
            x, y = start[0] + dx * fraction, start[1] + dy * fraction
            # Half-open ownership is deliberately the route authority.
            east = 0 if x < SPLIT_M else 1
            north = 0 if y < SPLIT_M else 1
            owner = f"wave17-locality:e{east}:n{north}"
            points.append({"sample": sample, "time_s": sample / ROUTE_HZ,
                           "east_m": x, "north_m": y, "owner": owner,
                           "leg": leg})
            sample += 1
    return points


def fixture(source: tuple[float, float, float], waypoints: list[tuple[float, float, float]],
            fixture_id: str) -> dict[str, Any]:
    return {
        "schema_version": "fightbox.fixture.s6a.v1", "fixture_id": fixture_id,
        "gate": "S5", "coordinate_frame": {"name": "local_enu", "units": "meters_seconds",
        "axes": "x_east_y_north_z_up", "steam_audio_mapping": "steam_x=enu_x;steam_y=enu_z;steam_z=-enu_y"},
        "kernel": {"name": "Steam Audio", "version": "4.8.1"},
        "sources": [{"id": "wave17-seam-pink", "asset_id": "s3-calibrated-pink",
                      "reference_level": {"mode": "SplAtOneMeter", "db_spl": 85},
                      "position_m": list(source)}],
        "listener": {"trajectory": {"waypoints_m": [list(v) for v in waypoints],
                      "speed_mps": SPEED_MPS, "max_speed_mps": SPEED_MPS},
                     "forward_enu": [0, 1, 0], "up_enu": [0, 0, 1]},
        # The city command replaces this fixture mesh with the package mesh; the
        # valid tiny mesh only satisfies the fixture parser before replacement.
        "geometry": {"vertices_m": [[-1, -1, 0], [1, -1, 0], [1, 1, 0], [-1, 1, 0]],
                     "triangles": [{"indices": [0, 1, 2], "material": "masonry"},
                                   {"indices": [0, 2, 3], "material": "masonry"}],
                     "materials": {"masonry": {"absorption": [0.03, 0.05, 0.07],
                     "scattering": 0.1, "transmission": [0, 0, 0]}}},
        "simulation": {"direct": {"distance_attenuation": True, "occlusion": True,
                      "occlusion_samples": 64},
            "reflections": {"enabled": True, "rays": 4096, "bounces": 2, "duration_s": 1.0},
            "pathing": {"enabled": True, "order": 2, "validation": True,
                        "alternate_paths": True, "runtime_order": ["direct", "path", "reflections"]},
            "probe_volume": {"type": "box", "min_m": [-2, -2, 0.5],
                              "max_m": [2, 2, 2.5], "spacing_m": 1.0},
            "probe_generation": {"type": "uniform_floor", "height_m": 1.5},
            "path_bake": {"identifier": "wave17-seam-harness-v1", "required_call": "iplPathBakerBake",
                          "probe_batch_serialization": "required", "fresh_process_reload": True,
                          "bake_order": 2}},
        "expected": {"properties": ["offline four-cell/oracle comparison"],
                      "non_claims": ["No live callback, device, iOS, crossfade, or listening claim."]},
    }


def owner_from_manifest(route: dict[str, Any], east: float, north: float) -> str | None:
    mm_e, mm_n = round(east * 1000), round(north * 1000)
    owners = []
    for cell in route["cells"]:
        b = cell["selection"]["ownership_bounds_city_enu_mm"]
        if b["min"][0] <= mm_e < b["max"][0] and b["min"][1] <= mm_n < b["max"][1]:
            owners.append(cell["cell_id"])
    if len(owners) > 1:
        raise RuntimeError(f"overlapping owner cells at {east},{north}: {owners}")
    return owners[0] if owners else None


def write_bake_with_identity(candidate: Path, cell: str, out: Path) -> dict[str, str]:
    short = CELL_SHORT[cell]
    src = candidate / "bakes" / f"{short}.baked"
    pkg = candidate / "packages" / f"{short}.fightbox"
    out.mkdir(parents=True)
    for name in ("probe-batch.bin", "probe-batch-metadata.json"):
        shutil.copy2(src / name, out / name)
    if (src / "capabilities").exists():
        shutil.copytree(src / "capabilities", out / "capabilities")
    metadata = json.loads((out / "probe-batch-metadata.json").read_text())
    manifest = json.loads((pkg / "manifest.json").read_text())
    identity = {"mesh_content_sha256": manifest["mesh"]["content_sha256"],
                "materials_content_sha256": manifest["materials_content_sha256"],
                "probe_batch_sha256": metadata["content_sha256"]}
    (out / "city-bake-manifest.json").write_text(json.dumps(identity, indent=2) + "\n")
    return identity


def write_oracle_bake_with_identity(candidate: Path, out: Path) -> dict[str, str]:
    src = candidate / "monolithic-oracle-v1"
    pkg = src / "oracle.fightbox"
    out.mkdir(parents=True)
    for name in ("probe-batch.bin", "probe-batch-metadata.json"):
        shutil.copy2(src / name, out / name)
    metadata = json.loads((out / "probe-batch-metadata.json").read_text())
    manifest = json.loads((pkg / "manifest.json").read_text())
    identity = {"mesh_content_sha256": manifest["mesh"]["content_sha256"],
                "materials_content_sha256": manifest["materials_content_sha256"],
                "probe_batch_sha256": metadata["content_sha256"]}
    (out / "city-bake-manifest.json").write_text(json.dumps(identity, indent=2) + "\n")
    return identity


def read_pcm(path: Path) -> list[float]:
    """Read the repository evidence WAVs (PCM16 or IEEE float32 stereo)."""
    import struct
    raw = path.read_bytes()
    if raw[:4] != b"RIFF" or raw[8:12] != b"WAVE":
        raise RuntimeError(f"not a RIFF/WAVE file: {path}")
    offset, fmt, data = 12, None, None
    while offset + 8 <= len(raw):
        chunk, size = raw[offset:offset + 4], struct.unpack_from("<I", raw, offset + 4)[0]
        payload = raw[offset + 8:offset + 8 + size]
        if chunk == b"fmt ": fmt = payload
        elif chunk == b"data": data = payload
        offset += 8 + size + (size & 1)
    if fmt is None or data is None:
        raise RuntimeError(f"WAV missing fmt/data: {path}")
    tag, channels, rate, _, block_align, bits = struct.unpack_from("<HHIIHH", fmt)
    if channels != 2 or rate != SAMPLE_RATE:
        raise RuntimeError(f"unexpected WAV format {path}: channels={channels}, rate={rate}")
    if tag == 3 and bits == 32:
        return list(struct.unpack("<%df" % (len(data) // 4), data))
    if tag == 1 and bits == 16:
        return [x / 32768.0 for x in struct.unpack("<%dh" % (len(data) // 2), data)]
    raise RuntimeError(f"unsupported WAV format {path}: tag={tag}, bits={bits}, align={block_align}")


def rms_dbfs(pcm: list[float], frame: int, window: int = 256) -> float | None:
    first, last = max(0, frame - window // 2), min(len(pcm) // 2, frame + window // 2)
    values = pcm[2 * first:2 * last]
    if not values or not all(math.isfinite(x) for x in values):
        return None
    rms = math.sqrt(sum(x * x for x in values) / len(values))
    return -300.0 if rms <= 0 else 20 * math.log10(rms)


def run_render(binary: Path, sdk: Path, repo: Path, package: Path, bake: Path,
               fixture_path: Path, out: Path) -> dict[str, Any]:
    out.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    env["STEAM_AUDIO_SDK_DIR"] = str(sdk)
    return run([str(binary), "city", "render", "--package", str(package), "--baked", str(bake),
                "--fixture", str(fixture_path), "--output", str(out)], env, cwd=repo)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--candidate", type=Path,
                    default=Path("/path/to/spatial-audio/evidence/wave17-locality-candidates-v1"))
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--oracle-root", type=Path,
                    default=Path("/path/to/spatial-audio/evidence/wave17-locality-candidates-v1"),
                    help="separate monolithic-oracle-v1 root (never treated as mobile route)")
    ap.add_argument("--fightbox", type=Path, default=Path("target/debug/fightbox"))
    ap.add_argument("--sdk-dir", type=Path,
                    default=Path(".cache/steam-audio/steamaudio-4.8.1/steamaudio"))
    ap.add_argument("--skip-renders", action="store_true")
    args = ap.parse_args()
    repo = Path(__file__).resolve().parents[1]
    candidate = args.candidate.resolve()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    route_path = candidate / "route" / "city-route-manifest.json"
    if not route_path.exists():
        route_path = candidate / "four-cell-baked-v2" / "city-route-manifest.json"
    oracle_root = args.oracle_root.resolve()
    oracle_path = oracle_root / "monolithic-oracle-v1" / "city-oracle-manifest.json"
    route = json.loads(route_path.read_text())
    oracle = json.loads(oracle_path.read_text())
    points = route_points()
    for p in points:
        expected = owner_from_manifest(route, p["east_m"], p["north_m"])
        if expected != p["owner"]:
            raise RuntimeError(f"owner mismatch at sample {p['sample']}: {expected} != {p['owner']}")
    # Every route sample is exactly 0.5m apart and every route tick exactly 4 controls.
    distances = [math.hypot(points[i]["east_m"] - points[i-1]["east_m"],
                             points[i]["north_m"] - points[i-1]["north_m"])
                 for i in range(1, len(points))]
    route_mechanical = {
        "evidence_class": "mechanical_offline_schedule",
        "sample_count": len(points), "spatial_sampling_m": SPATIAL_STEP_M,
        "control_hz": CONTROL_HZ, "route_hz": ROUTE_HZ,
        "controls_per_route_sample": CONTROL_HZ // ROUTE_HZ,
        "speed_mps": SPEED_MPS, "sample_spacing_min_m": min(distances),
        "sample_spacing_max_m": max(distances),
        "owner_cells_visited": list(dict.fromkeys(p["owner"] for p in points)),
        "owner_switch_samples": [p["sample"] for p in points
                                 if p["sample"] == 0 or p["owner"] != points[p["sample"] - 1]["owner"]],
        "route_manifest_sha256": sha256(route_path),
        "oracle_manifest_sha256": sha256(oracle_path),
        "oracle_path_range_m": oracle["path_range_m"],
        "claims": ["exact route ownership and cadence only"],
        "non_claims": ["not a live callback/device/iOS/production crossfade test"],
    }
    (output / "route-mechanical.json").write_text(json.dumps(route_mechanical, indent=2) + "\n")
    # The Swift host-consumer harness exercises the production-eligible route
    # decoder/selector and its hostile artifact mutations. Keep its JSON beside
    # this run so route proof is attributable, not inferred from acoustic WAVs.
    host = subprocess.run(["bash", str(repo / "scripts/test-wave17-route-manifest.sh"),
                           "--manifest", str(route_path), "--artifact-root", str(candidate)],
                          cwd=repo, text=True, capture_output=True)
    if host.returncode:
        raise RuntimeError(host.stderr[-4000:])
    (output / "route-host-consumer.json").write_text(host.stdout)
    fixture_dir = output / "fixtures"; fixture_dir.mkdir()
    render_dir = output / "renders"; render_dir.mkdir()
    oracle_fixture = fixture(SOURCE_CITY, WAYPOINTS, "wave17-four-cell-oracle-traversal")
    oracle_fixture_path = fixture_dir / "oracle.json"; oracle_fixture_path.write_text(json.dumps(oracle_fixture, indent=2) + "\n")
    oracle_pkg = oracle_root / "monolithic-oracle-v1" / "oracle.fightbox"
    oracle_bake = render_dir / "oracle.baked"
    oracle_identity = write_oracle_bake_with_identity(oracle_root, oracle_bake)
    command_results = []
    acoustic = {"evidence_class": "offline_linked_sdk_render", "sample_count": len(points),
                "spatial_sampling_m": SPATIAL_STEP_M, "route_hz": ROUTE_HZ,
                "oracle_path_range_m": oracle["path_range_m"], "oracle": {}, "streamed": {},
                "timeline_alignment": "global_frame_zero_full_route_exact_asset_frame",
                "claims": ["fresh-process offline linked-SDK output comparison",
                           "exact source/render frame alignment"],
                "non_claims": ["not a live callback, device, iOS, crossfade, in-flight event, tail, or listening result"]}
    if not args.skip_renders:
        command_results.append(run_render(args.fightbox.resolve(), args.sdk_dir.resolve(), repo,
                                          oracle_pkg, oracle_bake, oracle_fixture_path,
                                          render_dir / "oracle"))
        # Render one fresh process per contiguous owner run. This is deliberately
        # an offline comparison harness, not a claim that a production swap can
        # be replaced by process restart.
        owner_runs = {cell: [p for p in points if p["owner"] == cell] for cell in CELL_IDS}
        for cell in CELL_IDS:
            short = CELL_SHORT[cell]
            offset = CELL_OFFSET[cell]
            src_local = (SOURCE_CITY[0] - offset[0], SOURCE_CITY[1] - offset[1], SOURCE_CITY[2])
            segment_points = owner_runs[cell]
            if len(segment_points) < 2:
                raise RuntimeError(f"owner run for {cell} has fewer than two samples")
            # Render every cell against the complete global-time trajectory.
            # Exact asset frame/seek identity is part of the production macro
            # contract; restarting the pink source at each owner run compares
            # unrelated 256-frame noise windows and can manufacture ±9 dB
            # deltas. We still measure only samples owned by this cell.
            route_local = [(p[0] - offset[0], p[1] - offset[1], p[2])
                           for p in WAYPOINTS]
            fp = fixture_dir / f"{short}.json"
            fp.write_text(json.dumps(fixture(src_local, route_local, f"wave17-{short}-full-route"), indent=2) + "\n")
            pkg = candidate / "packages" / f"{short}.fightbox"
            bake = render_dir / f"{short}.baked"
            write_bake_with_identity(candidate, cell, bake)
            command_results.append(run_render(args.fightbox.resolve(), args.sdk_dir.resolve(), repo,
                                              pkg, bake, fp, render_dir / short))
        oracle_pcm = read_pcm(render_dir / "oracle" / "mix.wav")
        # Oracle and each streamed leg are at the same speed and duration. Compare settled
        # 256-frame windows at the exact 0.5m route samples, not arbitrary WAV offsets.
        acoustic["oracle"] = {"wav_sha256": sha256(render_dir / "oracle" / "mix.wav"),
                               "frame_count": len(oracle_pcm) // 2}
        all_deltas = []
        stitched_max_abs_pcm_error = 0.0
        stitched_squared_error = 0.0
        stitched_value_count = 0
        boundary_samples: dict[str, dict[int, list[float]]] = {}
        for cell in CELL_IDS:
            short = CELL_SHORT[cell]
            pcm = read_pcm(render_dir / short / "mix.wav")
            segment_points = [p for p in points if p["owner"] == cell]
            # Cell and oracle renders share global frame zero, including the
            # same renderer warmup. Compare every owned sample symmetrically;
            # no per-cell startup exclusion is needed or permitted.
            startup_samples_excluded = 0
            measurement_points = segment_points
            deltas = []
            for p in measurement_points:
                frame = round(p["sample"] * SAMPLE_RATE / ROUTE_HZ)
                oracle_frame = frame
                stream_db = rms_dbfs(pcm, frame)
                oracle_db = rms_dbfs(oracle_pcm, oracle_frame)
                delta = (stream_db - oracle_db) if stream_db is not None and oracle_db is not None else None
                if delta is not None and math.isfinite(delta): deltas.append(delta); all_deltas.append(delta)
            owned_start_frame = round(segment_points[0]["sample"] * SAMPLE_RATE / ROUTE_HZ)
            owned_end_frame = min(
                len(oracle_pcm) // 2,
                round((segment_points[-1]["sample"] + 1) * SAMPLE_RATE / ROUTE_HZ),
            )
            owned_max_error = 0.0
            owned_squared_error = 0.0
            owned_value_count = 0
            for value_index in range(2 * owned_start_frame, 2 * owned_end_frame):
                error = pcm[value_index] - oracle_pcm[value_index]
                owned_max_error = max(owned_max_error, abs(error))
                owned_squared_error += error * error
                owned_value_count += 1
            stitched_max_abs_pcm_error = max(stitched_max_abs_pcm_error, owned_max_error)
            stitched_squared_error += owned_squared_error
            stitched_value_count += owned_value_count
            boundary_samples[cell] = {}
            for switch_sample in route_mechanical["owner_switch_samples"][1:]:
                switch_frame = round(switch_sample * SAMPLE_RATE / ROUTE_HZ)
                if cell in (points[switch_sample - 1]["owner"], points[switch_sample]["owner"]):
                    boundary_samples[cell][switch_frame - 1] = pcm[2 * (switch_frame - 1):2 * switch_frame]
                    boundary_samples[cell][switch_frame] = pcm[2 * switch_frame:2 * (switch_frame + 1)]
            acoustic["streamed"][cell] = {"wav_sha256": sha256(render_dir / short / "mix.wav"),
                                          "frame_count": len(pcm) // 2,
                                          "startup_samples_excluded": startup_samples_excluded,
                                          "samples_compared": len(deltas),
                                          "max_abs_level_delta_db": max(map(abs, deltas), default=None),
                                          "p95_abs_level_delta_db": sorted(map(abs, deltas))[int(0.95 * (len(deltas) - 1))] if deltas else None,
                                          "owned_pcm_max_abs_error": owned_max_error,
                                          "owned_pcm_rms_error": math.sqrt(owned_squared_error / owned_value_count)}
        seam_jumps = []
        for switch_sample in route_mechanical["owner_switch_samples"][1:]:
            switch_frame = round(switch_sample * SAMPLE_RATE / ROUTE_HZ)
            old_cell = points[switch_sample - 1]["owner"]
            new_cell = points[switch_sample]["owner"]
            old_values = boundary_samples[old_cell][switch_frame - 1]
            new_values = boundary_samples[new_cell][switch_frame]
            oracle_old = oracle_pcm[2 * (switch_frame - 1):2 * switch_frame]
            oracle_new = oracle_pcm[2 * switch_frame:2 * (switch_frame + 1)]
            actual_jump = [new_values[channel] - old_values[channel] for channel in range(2)]
            oracle_jump = [oracle_new[channel] - oracle_old[channel] for channel in range(2)]
            seam_jumps.append({
                "route_sample": switch_sample,
                "frame": switch_frame,
                "from_cell": old_cell,
                "to_cell": new_cell,
                "actual_jump_peak": max(map(abs, actual_jump)),
                "oracle_jump_peak": max(map(abs, oracle_jump)),
                "extra_jump_peak": max(abs(actual_jump[channel] - oracle_jump[channel]) for channel in range(2)),
            })
        acoustic["exact_frame_stitch"] = {
            "owned_pcm_max_abs_error": stitched_max_abs_pcm_error,
            "owned_pcm_rms_error": math.sqrt(stitched_squared_error / stitched_value_count),
            "seam_jumps": seam_jumps,
        }
        acoustic["aggregate_max_abs_level_delta_db"] = max(map(abs, all_deltas), default=None)
        acoustic["aggregate_p95_abs_level_delta_db"] = sorted(map(abs, all_deltas))[int(0.95 * (len(all_deltas) - 1))] if all_deltas else None
        acoustic["seam_level_gate_db"] = 1.0
        acoustic["offline_level_gate_passed"] = (acoustic["aggregate_max_abs_level_delta_db"] is not None
                                                   and acoustic["aggregate_max_abs_level_delta_db"] <= 1.0)
    else:
        acoustic["status"] = "render_skipped"
        acoustic["offline_level_gate_passed"] = None
    (output / "acoustic-offline.json").write_text(json.dumps(acoustic, indent=2, allow_nan=False) + "\n")
    # Capture the production runtime fallback regression without pretending that this
    # process is a live route host. It proves the exact stale-completion state machine.
    env = dict(os.environ); env["STEAM_AUDIO_SDK_DIR"] = str(args.sdk_dir.resolve())
    test = subprocess.run(["cargo", "test", "-p", "fightbox-runtime", "--lib",
                           "multiway_prediction_miss_drops_stale_completion_before_replacement"], cwd=repo, env=env, text=True, capture_output=True)
    fallback = {"evidence_class": "mechanical_runtime_unit", "command": ["cargo", "test", "-p", "fightbox-runtime", "--lib", "multiway_prediction_miss_drops_stale_completion_before_replacement"],
                "returncode": test.returncode, "passed": test.returncode == 0,
                "no_third_world_claim": "CellStreamManager ceiling is covered by the unit state machine; no live callback claim",
                "stdout": test.stdout[-3000:], "stderr": test.stderr[-3000:]}
    (output / "prediction-fallback.json").write_text(json.dumps(fallback, indent=2) + "\n")
    report = {"schema_version": "fightbox.wave17-locality-seam-run.v1",
              "artifact_state": "offline_mechanical_and_linked_render_evidence",
              "route_mechanical": "route-mechanical.json", "route_host_consumer": "route-host-consumer.json",
              "acoustic_offline": "acoustic-offline.json", "prediction_fallback": "prediction-fallback.json", "commands": command_results,
              "route_manifest_sha256": sha256(route_path), "oracle_manifest_sha256": sha256(oracle_path),
              "mechanical_gate_status": "passed" if fallback["passed"] else "failed",
              "offline_acoustic_level_gate_status": (
                  "passed" if acoustic.get("offline_level_gate_passed") is True else
                  "failed" if acoustic.get("offline_level_gate_passed") is False else "not_run"
              ),
              "production_crossfade_gate_status": "unproven",
              "claims": ["mechanical route schedule", "offline linked-SDK rendered comparison"],
              "non_claims": ["No live audio callback/device/iOS/AirPods/thermal/listening evidence.",
                             "Fresh-process per-cell renders do not prove production crossfade continuity."],
              "artifact_hashes": {name: sha256(output / name) for name in
                                  ["route-mechanical.json", "route-host-consumer.json", "acoustic-offline.json", "prediction-fallback.json"]}}
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"report": str(output / "report.json"), "sample_count": len(points),
                      "fallback_passed": fallback["passed"],
                      "acoustic": acoustic.get("aggregate_max_abs_level_delta_db", "not-run")}, indent=2))
    return 0

if __name__ == "__main__":
    try: raise SystemExit(main())
    except Exception as exc:
        print(f"wave17 seam traversal failed: {exc}", file=sys.stderr)
        raise
