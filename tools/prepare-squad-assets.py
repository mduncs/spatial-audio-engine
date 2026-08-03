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


@dataclass(frozen=True)
class Burst:
    round_variants: tuple[int, ...]
    tail_variant: int
    silence_s: float


@dataclass(frozen=True)
class ComposedAsset:
    asset_id: str
    relative_directory: str
    round_filename: str
    tail_filename: str
    bursts: tuple[Burst, ...]
    crossfade_ms: float
    preparation_notes: tuple[str, ...]


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


COMPOSED_ASSETS = (
    ComposedAsset(
        asset_id="squad-m2-burst-loop",
        relative_directory=(
            "Game/Vehicles/Emplaced50cal/Sounds/Fire/3p/AUTO/Default/Initial"
        ),
        round_filename="m2_fire_loop_initial_3p_default_{variant:02}.ogg",
        tail_filename="m2_fire_loop_initial_3p_default_tail_{variant:02}.ogg",
        bursts=(
            Burst((1, 7, 13, 19, 4, 10), 1, 2.5),
            Burst((16, 2, 8, 14, 20, 5, 11, 17, 3, 9, 15), 2, 3.75),
            Burst((21, 6, 12, 18), 3, 2.25),
            Burst((4, 13, 1, 16, 7, 20, 10, 2, 18), 4, 2.0),
        ),
        crossfade_ms=0.0,
        preparation_notes=(
            "The fixed offline M2 composition uses close third-person Default/Initial round slices and tails only; Mid, Far, and Echo_Close layers are excluded because the engine supplies distance and reflection effects.",
            "The fixture's 153 dB SplAtOneMeter steady-source control is anchored to the 153 dB M2 HB .50-cal impulse-noise value in U.S. Army ATP 4-25.12, Unit Field Sanitation Teams, 30 April 2014, chapter 10: https://rdl.train.army.mil/catalog-ws/view/100.ATSC/6FDDE4EE-4362-464B-AEA0-1EC3A3D90D88-1399551821430/atp4_25x12.pdf",
            "Peak-to-reference rationale: the cited per-shot operator peak is used directly as a conservative steady-source scene anchor; it is not a claim that the loop has a continuous physical SPL or that its duty-cycle energy was measured.",
        ),
    ),
    ComposedAsset(
        asset_id="squad-dshk-burst-loop",
        relative_directory=(
            "Game/Vehicles/Emplaced_Dshk/Sounds/Fire/1p/Default/Initial"
        ),
        round_filename="dshk_fire_AUTO_1p_initial_{variant:02}.ogg",
        tail_filename="dshk_fire_AUTO_1p_initial_tail_{variant:02}.ogg",
        bursts=(
            Burst((2, 7, 11, 5, 9, 3, 8, 1), 2, 3.25),
            Burst((10, 4, 6, 2, 9), 4, 2.5),
            Burst((5, 11, 3, 8, 1, 7, 10, 4, 6, 2), 1, 4.0),
            Burst((9, 5, 1, 8, 3, 11, 6), 3, 2.0),
        ),
        crossfade_ms=5.0,
        preparation_notes=(
            "The DShK 3p Default/Initial folder has only a start recording plus silence, not a per-round variant set, so this fixed offline composition uses the requested 1p Default/Initial fallback; Mech and silence files are excluded.",
            "No direct DShK operator-peak measurement was located. The fixture's 154 dB SplAtOneMeter steady-source control is an explicit 12.7 mm heavy-machine-gun class inference: 1 dB above the 153 dB M2 HB .50-cal value in U.S. Army ATP 4-25.12 (30 April 2014, chapter 10, https://rdl.train.army.mil/catalog-ws/view/100.ATSC/6FDDE4EE-4362-464B-AEA0-1EC3A3D90D88-1399551821430/atp4_25x12.pdf) and below the 160.7 dB(C) bridge-wing peak measured for a 12.7 mm L111A1 by Paddan (2015), doi:10.1093/annhyg/mev053, https://doi.org/10.1093/annhyg/mev053.",
            "Peak-to-reference rationale: the class-inferred per-shot peak is used as a conservative steady-source scene anchor; it is not a direct DShK measurement, continuous physical SPL, or delivered-ear-SPL claim.",
        ),
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


def decode_trimmed(
    source: Path,
    ffmpeg: str,
    ffprobe: str,
    resampler_filter: str,
) -> tuple[array, dict[str, float | int]]:
    input_info = probe(source, ffprobe)
    samples = decode(source, int(input_info["channels"]), ffmpeg, resampler_filter)
    expected_frames = round(float(input_info["duration_s"]) * SAMPLE_RATE_HZ)
    if len(samples) > expected_frames:
        del samples[expected_frames:]
    return samples, input_info


def append_equal_power_crossfade(
    output: array,
    segment: array,
    crossfade_frames: int,
) -> None:
    frames = min(crossfade_frames, len(output), len(segment))
    if frames < 2:
        output.extend(segment)
        return
    start = len(output) - frames
    for index in range(frames):
        phase = math.pi * 0.5 * index / (frames - 1)
        left = float(output[start + index]) * math.cos(phase)
        right = float(segment[index]) * math.sin(phase)
        output[start + index] = left + right
    output.extend(segment[frames:])


def normalize_composition(samples: array) -> tuple[array, dict[str, float]]:
    raw_rms = rms_dbfs(samples)
    raw_peak = amplitude_dbfs(max(abs(float(sample)) for sample in samples))
    applied_gain_db = min(
        TARGET_RMS_DBFS - raw_rms,
        TARGET_PEAK_HEADROOM_DBFS - raw_peak,
    )
    if applied_gain_db:
        gain = 10.0 ** (applied_gain_db / 20.0)
        normalized = array("f", (float(sample) * gain for sample in samples))
    else:
        normalized = array("f", samples)
    output_rms = rms_dbfs(normalized)
    output_peak = amplitude_dbfs(max(abs(float(sample)) for sample in normalized))
    if output_peak > TARGET_PEAK_HEADROOM_DBFS + 1.0e-5:
        raise RuntimeError(
            f"composition peak {output_peak:.6f} dBFS exceeds "
            f"{TARGET_PEAK_HEADROOM_DBFS:.1f} dBFS"
        )
    return normalized, {
        "raw_rms_dbfs": raw_rms,
        "raw_peak_dbfs": raw_peak,
        "applied_gain_db": applied_gain_db,
        "output_rms_dbfs": output_rms,
        "output_peak_dbfs": output_peak,
    }


def composed_descriptor(
    asset: ComposedAsset,
    wav_path: Path,
    samples: array,
    levels: dict[str, float],
) -> dict[str, object]:
    non_claims = [
        "This descriptor makes no delivered-ear-SPL claim without output calibration.",
        "The recording hash establishes file identity, not authorship, licensing, or provenance beyond the pinned bytes.",
        "The prepared WAV is a local derivative of Squad v8.1 game assets; this descriptor grants no right to redistribute audio bytes.",
        *asset.preparation_notes,
    ]
    return {
        "schema_version": "fightbox.asset-descriptor.v1",
        "asset_id": asset.asset_id,
        "kind": "wav",
        "generator": {
            "wav": {
                "path": wav_path.relative_to(REPOSITORY_ROOT).as_posix(),
                "sha256": hashlib.sha256(wav_path.read_bytes()).hexdigest(),
                "start_frame": 0,
                "loop": True,
            }
        },
        "channels": 1,
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "duration_s": round(len(samples) / SAMPLE_RATE_HZ, 9),
        "target_rms_dbfs": round(levels["output_rms_dbfs"], 6),
        "expected_reference_rms_dbfs": round(levels["raw_rms_dbfs"], 6),
        "calibration": {
            "applied_gain_db": round(levels["applied_gain_db"], 6)
        },
        "non_claims": non_claims,
    }


def prepare_composed(
    asset: ComposedAsset,
    ffmpeg: str,
    ffprobe: str,
    resampler_filter: str,
) -> dict[str, object]:
    source_directory = SOURCE_ROOT / asset.relative_directory
    crossfade_frames = round(asset.crossfade_ms * SAMPLE_RATE_HZ / 1000.0)
    decoded: dict[Path, array] = {}
    source_info: dict[Path, dict[str, float | int]] = {}

    def load(filename: str) -> array:
        source = source_directory / filename
        if source not in decoded:
            if not source.is_file():
                raise RuntimeError(f"Squad source asset is missing: {source}")
            samples, info = decode_trimmed(
                source, ffmpeg, ffprobe, resampler_filter
            )
            decoded[source] = samples
            source_info[source] = info
        return decoded[source]

    composition = array("f")
    burst_report = []
    for burst in asset.bursts:
        for round_index, variant in enumerate(burst.round_variants):
            filename = asset.round_filename.format(variant=variant)
            samples = load(filename)
            if round_index and crossfade_frames:
                append_equal_power_crossfade(
                    composition, samples, crossfade_frames
                )
            else:
                composition.extend(samples)
        tail_filename = asset.tail_filename.format(variant=burst.tail_variant)
        tail = load(tail_filename)
        if crossfade_frames:
            append_equal_power_crossfade(composition, tail, crossfade_frames)
        else:
            composition.extend(tail)
        silence_frames = round(burst.silence_s * SAMPLE_RATE_HZ)
        composition.extend(array("f", [0.0]) * silence_frames)
        burst_report.append(
            {
                "round_variants": list(burst.round_variants),
                "tail_variant": burst.tail_variant,
                "silence_s": burst.silence_s,
            }
        )

    if asset.bursts[-1].silence_s < 1.5:
        raise RuntimeError(f"{asset.asset_id} requires at least 1.5 s final silence")
    samples, levels = normalize_composition(composition)
    duration_s = len(samples) / SAMPLE_RATE_HZ
    if not 25.0 <= duration_s <= 35.0:
        raise RuntimeError(
            f"{asset.asset_id} duration {duration_s:.6f} s is outside 25..=35 s"
        )

    wav_path = WAV_ROOT / f"{asset.asset_id}.wav"
    write_float_wav(wav_path, samples)
    output_descriptor = composed_descriptor(asset, wav_path, samples, levels)
    descriptor_path = DESCRIPTOR_ROOT / f"{asset.asset_id}.json"
    descriptor_path.write_text(
        json.dumps(output_descriptor, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    return {
        "asset_id": asset.asset_id,
        "source_directory": str(source_directory),
        "sources": [
            {
                "path": str(path),
                **source_info[path],
            }
            for path in sorted(source_info)
        ],
        "composition": {
            "bursts": burst_report,
            "crossfade_ms": asset.crossfade_ms,
            "duration_s": output_descriptor["duration_s"],
            "raw_rms_dbfs": round(levels["raw_rms_dbfs"], 6),
            "raw_peak_dbfs": round(levels["raw_peak_dbfs"], 6),
            "applied_gain_db": output_descriptor["calibration"][
                "applied_gain_db"
            ],
            "output_rms_dbfs": output_descriptor["target_rms_dbfs"],
            "output_peak_dbfs": round(levels["output_peak_dbfs"], 6),
            "sha256": output_descriptor["generator"]["wav"]["sha256"],
        },
        "loop": {
            "intended": True,
            "final_silence_s": asset.bursts[-1].silence_s,
            "seam": seam_metrics(samples),
        },
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
    results.extend(
        prepare_composed(asset, ffmpeg, ffprobe, resampler_filter)
        for asset in COMPOSED_ASSETS
    )
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
