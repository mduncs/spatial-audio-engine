#!/usr/bin/env python3
"""Prepare local-only Squad v8.1 assets for the Checkpoint Block fixture.

The source OGGs remain on the external drive. Prepared WAV bytes are written
only under the gitignored fixtures/assets/squad/ directory; tracked JSON
descriptors pin those local bytes for the workbench loader.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import shutil
import struct
import subprocess
import sys
from array import array
from dataclasses import dataclass
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
SOURCE_ROOT = Path("/Volumes/External/GameAssets/Squad (v8.1) Sound, VO & Music")
WAV_ROOT = REPOSITORY_ROOT / "fixtures/assets/squad"
DESCRIPTOR_ROOT = REPOSITORY_ROOT / "fixtures/assets"
SAMPLE_RATE_HZ = 48_000
TARGET_RMS_DBFS = -20.0
TARGET_PEAK_HEADROOM_DBFS = -1.0
CROSSFADE_FRAMES = 2_400  # 50 ms at 48 kHz.
MIN_BAD_JUMP_DBFS = -45.0
BAD_JUMP_OVER_STEP_DB = 6.0


@dataclass(frozen=True)
class Asset:
    asset_id: str
    relative_path: str
    loop_intended: bool = True


ASSETS = (
    Asset(
        "squad-abrams-idle",
        "Game/Vehicles/M1A2/Sounds/Engine/EXT/Idle/abrams_engine_ext_idle_01.ogg",
    ),
    Asset(
        "squad-ural-idle",
        "Game/Vehicles/Ural4320/Sounds/Engine/EXT/Idle/Ural2_engine_ext_idle_01.ogg",
    ),
    Asset(
        "squad-generator-diesel",
        "Game/Audio/Ambient/Industrial/Generators/ambient_generator_diesel_01.ogg",
    ),
    Asset("squad-fire-car", "Game/Audio/Ambient/Fires/fire_car_01.ogg"),
    Asset(
        "squad-fire-building-large",
        "Game/Audio/Ambient/Fires/fire_building_large_01.ogg",
    ),
    Asset(
        "squad-fob-radio-static",
        "Game/Audio/Deployables/FOB/Radios/us_fob_radio_static_noise_loop.ogg",
    ),
    Asset(
        "squad-camo-tent-flap",
        "Game/Audio/Ambient/Wind/Tent/camo_tent_flapping_01.ogg",
    ),
    Asset(
        "squad-mi8-rotor-close",
        "Game/Vehicles/MI8/Sounds/Engine/Rotor/Close/mi8_engine_idle_close_01.ogg",
    ),
    Asset(
        "squad-m2-blast",
        "Game/Vehicles/Emplaced50cal/Sounds/Fire/3p/AUTO/Default/Initial/m2_fire_loop_initial_3p_default_01.ogg",
        loop_intended=False,
    ),
)


def run(command: list[str]) -> bytes:
    try:
        return subprocess.run(command, check=True, capture_output=True).stdout
    except FileNotFoundError as error:
        raise RuntimeError(f"required executable is unavailable: {command[0]}") from error
    except subprocess.CalledProcessError as error:
        stderr = error.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"command failed ({command[0]}): {stderr}") from error


def probe(path: Path, ffprobe: str) -> dict[str, float | int]:
    payload = json.loads(
        run(
            [
                ffprobe,
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=sample_rate,channels,duration:format=duration",
                "-of",
                "json",
                str(path),
            ]
        )
    )
    stream = payload["streams"][0]
    duration = stream.get("duration", payload.get("format", {}).get("duration"))
    return {
        "sample_rate_hz": int(stream["sample_rate"]),
        "channels": int(stream["channels"]),
        "duration_s": float(duration),
    }


def decode(path: Path, channels: int, ffmpeg: str, resampler_filter: str) -> array:
    if channels not in (1, 2):
        raise RuntimeError(f"{path} has unsupported channel count {channels}")
    filters: list[str] = []
    if channels == 2:
        filters.append(
            "pan=mono|c0=0.7071067811865476*c0+0.7071067811865476*c1"
        )
    filters.append(resampler_filter)
    raw = run(
        [
            ffmpeg,
            "-v",
            "error",
            "-i",
            str(path),
            "-map",
            "0:a:0",
            "-vn",
            "-af",
            ",".join(filters),
            "-ac",
            "1",
            "-ar",
            str(SAMPLE_RATE_HZ),
            "-c:a",
            "pcm_f32le",
            "-f",
            "f32le",
            "pipe:1",
        ]
    )
    if len(raw) % 4:
        raise RuntimeError(f"ffmpeg returned a partial float sample for {path}")
    samples = array("f")
    samples.frombytes(raw)
    if sys.byteorder != "little":
        samples.byteswap()
    if not samples or any(not math.isfinite(sample) for sample in samples):
        raise RuntimeError(f"ffmpeg returned empty or non-finite audio for {path}")
    return samples


def amplitude_dbfs(value: float) -> float:
    return 20.0 * math.log10(max(abs(value), 1.0e-12))


def rms_dbfs(samples: array) -> float:
    energy = math.fsum(float(sample) * float(sample) for sample in samples)
    return amplitude_dbfs(math.sqrt(energy / len(samples)))


def seam_metrics(samples: array) -> dict[str, float | bool]:
    jump = abs(float(samples[-1]) - float(samples[0]))
    step_energy = math.fsum(
        (float(right) - float(left)) ** 2
        for left, right in zip(samples, samples[1:])
    )
    step_rms = math.sqrt(step_energy / max(1, len(samples) - 1))
    jump_dbfs = amplitude_dbfs(jump)
    jump_over_step_db = amplitude_dbfs(jump / max(step_rms, 1.0e-12))
    bad = jump_dbfs > MIN_BAD_JUMP_DBFS and jump_over_step_db > BAD_JUMP_OVER_STEP_DB
    return {
        "boundary_jump_dbfs": round(jump_dbfs, 3),
        "jump_over_adjacent_step_rms_db": round(jump_over_step_db, 3),
        "bad": bad,
    }


def crossfade_loop(samples: array) -> array:
    frames = min(CROSSFADE_FRAMES, len(samples) // 8)
    if frames < 2:
        raise RuntimeError("asset is too short for the loop crossfade")
    middle = array("f", samples[frames : len(samples) - frames])
    overlap = array("f")
    for index in range(frames):
        phase = math.pi * 0.5 * index / (frames - 1)
        tail = float(samples[len(samples) - frames + index]) * math.cos(phase)
        head = float(samples[index]) * math.sin(phase)
        overlap.append(tail + head)
    middle.extend(overlap)
    return middle


def write_float_wav(path: Path, samples: array) -> None:
    data = samples.tobytes()
    if sys.byteorder != "little":
        swapped = array("f", samples)
        swapped.byteswap()
        data = swapped.tobytes()
    fmt = struct.pack(
        "<HHIIHH", 3, 1, SAMPLE_RATE_HZ, SAMPLE_RATE_HZ * 4, 4, 32
    )
    body = b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt
    body += b"data" + struct.pack("<I", len(data)) + data
    payload = b"RIFF" + struct.pack("<I", len(body)) + body
    temporary = path.with_suffix(".wav.tmp")
    temporary.write_bytes(payload)
    temporary.replace(path)


def descriptor(asset: Asset, wav_path: Path, samples: array) -> dict[str, object]:
    raw_rms = rms_dbfs(samples)
    raw_peak = amplitude_dbfs(max(abs(float(sample)) for sample in samples))
    target_rms = min(
        TARGET_RMS_DBFS,
        raw_rms + TARGET_PEAK_HEADROOM_DBFS - raw_peak,
    )
    return {
        "schema_version": "fightbox.asset-descriptor.v1",
        "asset_id": asset.asset_id,
        "kind": "wav",
        "generator": {
            "wav": {
                "path": wav_path.relative_to(REPOSITORY_ROOT).as_posix(),
                "sha256": hashlib.sha256(wav_path.read_bytes()).hexdigest(),
                "start_frame": 0,
                "loop": asset.loop_intended,
            }
        },
        "channels": 1,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "duration_s": round(len(samples) / SAMPLE_RATE_HZ, 9),
        "target_rms_dbfs": round(target_rms, 6),
        "expected_reference_rms_dbfs": round(raw_rms, 6),
        "calibration": {"applied_gain_db": round(target_rms - raw_rms, 6)},
        "non_claims": [
            "This descriptor makes no delivered-ear-SPL claim without output calibration.",
            "The recording hash establishes file identity, not authorship, licensing, or provenance beyond the pinned bytes.",
            "The prepared WAV is a local derivative of a Squad v8.1 game asset; this descriptor grants no right to redistribute audio bytes.",
        ],
    }


def prepare(
    asset: Asset,
    ffmpeg: str,
    ffprobe: str,
    resampler_filter: str,
) -> dict[str, object]:
    source = SOURCE_ROOT / asset.relative_path
    if not source.is_file():
        raise RuntimeError(f"Squad source asset is missing: {source}")
    input_info = probe(source, ffprobe)
    samples = decode(source, int(input_info["channels"]), ffmpeg, resampler_filter)
    expected_frames = round(float(input_info["duration_s"]) * SAMPLE_RATE_HZ)
    if len(samples) > expected_frames:
        del samples[expected_frames:]
    seam_before = seam_metrics(samples) if asset.loop_intended else None
    crossfade_applied = bool(seam_before and seam_before["bad"])
    if crossfade_applied:
        samples = crossfade_loop(samples)
    seam_after = seam_metrics(samples) if asset.loop_intended else None

    wav_path = WAV_ROOT / f"{asset.asset_id}.wav"
    write_float_wav(wav_path, samples)
    output_descriptor = descriptor(asset, wav_path, samples)
    descriptor_path = DESCRIPTOR_ROOT / f"{asset.asset_id}.json"
    descriptor_path.write_text(
        json.dumps(output_descriptor, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    return {
        "asset_id": asset.asset_id,
        "source": str(source),
        "input": input_info,
        "output": {
            "sample_rate_hz": SAMPLE_RATE_HZ,
            "channels": 1,
            "duration_s": output_descriptor["duration_s"],
            "rms_dbfs": output_descriptor["expected_reference_rms_dbfs"],
            "loader_target_rms_dbfs": output_descriptor["target_rms_dbfs"],
            "sha256": output_descriptor["generator"]["wav"]["sha256"],
        },
        "loop": {
            "intended": asset.loop_intended,
            "seam_before": seam_before,
            "crossfade_applied": crossfade_applied,
            "crossfade_ms": 50.0 if crossfade_applied else 0.0,
            "seam_after": seam_after,
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--report",
        type=Path,
        help="also write the machine-readable preparation report to this path",
    )
    args = parser.parse_args()
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    if not ffmpeg or not ffprobe:
        raise RuntimeError("ffmpeg and ffprobe are required for Squad asset preparation")
    if not SOURCE_ROOT.is_dir():
        raise RuntimeError(f"Squad asset root is unavailable: {SOURCE_ROOT}")

    soxr_probe = subprocess.run(
        [
            ffmpeg,
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=44100:cl=mono",
            "-t",
            "0.01",
            "-af",
            "aresample=48000:resampler=soxr:precision=28:cheby=1",
            "-f",
            "null",
            "-",
        ],
        capture_output=True,
        check=False,
    )
    if soxr_probe.returncode == 0:
        resampler_filter = "aresample=48000:resampler=soxr:precision=28:cheby=1"
        resampler_description = (
            "FFmpeg libsoxr, 48 kHz, 28-bit precision, Chebyshev passband"
        )
    else:
        resampler_filter = (
            "aresample=48000:resampler=swr:filter_size=64:phase_shift=10:"
            "exact_rational=1:cutoff=0.97:dither_method=triangular_hp"
        )
        resampler_description = (
            "FFmpeg swresample, 48 kHz, 64-tap sinc, exact-rational phase, "
            "0.97 cutoff, high-pass triangular dither (libsoxr unavailable)"
        )

    WAV_ROOT.mkdir(parents=True, exist_ok=True)
    results = [
        prepare(asset, ffmpeg, ffprobe, resampler_filter) for asset in ASSETS
    ]
    report = {
        "schema_version": "fightbox.squad-asset-preparation.v1",
        "source_root": str(SOURCE_ROOT),
        "decoder": ffmpeg,
        "resampler": resampler_description,
        "stereo_fold": "equal-power: 0.7071067811865476 * (left + right)",
        "preferred_target_rms_dbfs": TARGET_RMS_DBFS,
        "target_peak_headroom_dbfs": TARGET_PEAK_HEADROOM_DBFS,
        "assets": results,
    }
    rendered = json.dumps(report, indent=2, ensure_ascii=False) + "\n"
    if args.report:
        args.report.write_text(rendered, encoding="utf-8")
    print(rendered, end="")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"prepare-squad-assets: {error}", file=sys.stderr)
        raise SystemExit(1)
