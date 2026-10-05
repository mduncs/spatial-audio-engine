#!/usr/bin/env python3
"""Bind prepared private Squad thunder candidates into offline γ4 evidence.

This tool reads and hashes media but never opens an audio device.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import shutil
import struct
import sys
from array import array
from pathlib import Path

REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
EXPECTED_IDS = ("squad-thunder-distant-03", "squad-thunder-distant-15")
SAMPLE_RATE_HZ = 48_000


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def decode_float_mono_wav(path: Path) -> array:
    payload = path.read_bytes()
    if len(payload) < 12 or payload[:4] != b"RIFF" or payload[8:12] != b"WAVE":
        raise RuntimeError(f"{path}: malformed RIFF/WAVE header")
    fmt = None
    data = None
    offset = 12
    while offset + 8 <= len(payload):
        chunk_id = payload[offset : offset + 4]
        size = struct.unpack_from("<I", payload, offset + 4)[0]
        offset += 8
        end = offset + size
        if end > len(payload):
            raise RuntimeError(f"{path}: truncated WAV chunk")
        if chunk_id == b"fmt " and size >= 16:
            fmt = struct.unpack_from("<HHIIHH", payload, offset)
        elif chunk_id == b"data":
            data = payload[offset:end]
        offset = end + (size & 1)
    if fmt is None or data is None:
        raise RuntimeError(f"{path}: missing fmt or data chunk")
    tag, channels, sample_rate, _, _, bits = fmt
    if (tag, channels, sample_rate, bits) != (3, 1, SAMPLE_RATE_HZ, 32):
        raise RuntimeError(f"{path}: expected 48 kHz mono float32 WAV")
    if len(data) % 4:
        raise RuntimeError(f"{path}: partial float sample")
    samples = array("f")
    samples.frombytes(data)
    if sys.byteorder != "little":
        samples.byteswap()
    if not samples or any(not math.isfinite(sample) for sample in samples):
        raise RuntimeError(f"{path}: empty or non-finite PCM")
    return samples


def metrics(samples: array) -> dict[str, float | int | bool]:
    peak = max(abs(float(sample)) for sample in samples)
    rms = math.sqrt(math.fsum(float(sample) ** 2 for sample in samples) / len(samples))
    threshold = max(peak * 10.0 ** (-48.0 / 20.0), 1.0e-7)
    first_active = next(
        index for index, sample in enumerate(samples) if abs(sample) >= threshold
    )
    last_active = next(
        len(samples) - 1 - index
        for index, sample in enumerate(reversed(samples))
        if abs(sample) >= threshold
    )
    maximum_step = max(
        abs(float(right) - float(left)) for left, right in zip(samples, samples[1:])
    )
    return {
        "sample_rate_hz": SAMPLE_RATE_HZ,
        "channels": 1,
        "frame_count": len(samples),
        "duration_s": len(samples) / SAMPLE_RATE_HZ,
        "sample_peak": peak,
        "rms_dbfs": 20.0 * math.log10(rms),
        "crest_db": 20.0 * math.log10(peak / rms),
        "first_active_frame_at_minus_48db_peak": first_active,
        "last_active_frame_at_minus_48db_peak": last_active,
        "active_span_s_at_minus_48db_peak": (last_active - first_active + 1) / SAMPLE_RATE_HZ,
        "maximum_adjacent_step": maximum_step,
        "finite": True,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--preparation-report", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output
    if not output.is_absolute() or output.exists() or not output.parent.is_dir():
        raise RuntimeError("--output must be one absent absolute directory with an existing parent")
    prep = json.loads(args.preparation_report.read_text(encoding="utf-8"))
    by_id = {entry["asset_id"]: entry for entry in prep["assets"]}
    if set(by_id) != set(EXPECTED_IDS):
        raise RuntimeError(f"preparation report must contain exactly {EXPECTED_IDS}")

    temporary = output.parent / f".{output.name}.tmp-{__import__('os').getpid()}"
    temporary.mkdir()
    try:
        candidates = []
        for asset_id in EXPECTED_IDS:
            descriptor_path = REPOSITORY_ROOT / "fixtures/assets" / f"{asset_id}.json"
            descriptor_bytes = descriptor_path.read_bytes()
            descriptor = json.loads(descriptor_bytes)
            wav_value = descriptor["generator"]["wav"]
            wav_path = REPOSITORY_ROOT / wav_value["path"]
            if (
                descriptor["asset_id"] != asset_id
                or descriptor["kind"] != "wav"
                or descriptor["channels"] != 1
                or descriptor["sample_rate_hz"] != SAMPLE_RATE_HZ
                or wav_value["loop"] is not False
            ):
                raise RuntimeError(f"{asset_id}: incompatible descriptor")
            actual_wav_sha = sha256(wav_path)
            if actual_wav_sha != wav_value["sha256"] or actual_wav_sha != by_id[asset_id]["output"]["sha256"]:
                raise RuntimeError(f"{asset_id}: prepared WAV identity mismatch")
            samples = decode_float_mono_wav(wav_path)
            candidate_metrics = metrics(samples)
            if abs(candidate_metrics["duration_s"] - descriptor["duration_s"]) > 1.0 / SAMPLE_RATE_HZ:
                raise RuntimeError(f"{asset_id}: descriptor duration mismatch")
            copied_wav = temporary / f"{asset_id}.wav"
            copied_descriptor = temporary / f"{asset_id}.json"
            shutil.copyfile(wav_path, copied_wav)
            copied_descriptor.write_bytes(descriptor_bytes)
            candidates.append(
                {
                    "asset_id": asset_id,
                    "role": "alternate-pressure-edge" if asset_id.endswith("03") else "preferred-long-weather-tail",
                    "descriptor": str(output / copied_descriptor.name),
                    "descriptor_sha256": hashlib.sha256(descriptor_bytes).hexdigest(),
                    "stem": str(output / copied_wav.name),
                    "stem_sha256": actual_wav_sha,
                    "original_source": by_id[asset_id]["source"],
                    "original_source_sha256": by_id[asset_id]["input"]["sha256"],
                    "metrics": candidate_metrics,
                }
            )
        report = {
            "schema_version": "fightbox.gamma4-authored-thunder-capture.v1",
            "status": "artifact_generated",
            "evidence_class": "private_authored_mono_source_and_offline_stems",
            "preparation_report": str(args.preparation_report.resolve()),
            "preparation_report_sha256": sha256(args.preparation_report),
            "preferred_asset_id": "squad-thunder-distant-15",
            "selection_reason": "candidate 15 retains the longer authored weather-tail program; candidate 03 remains an exact alternate for pressure-edge comparison",
            "candidates": candidates,
            "resources": None,
            "listening": {"status": "pending", "listener_id": "", "outcome": "pending"},
            "claims": [
                "two seekable authored mono thunder candidates are identity-bound and finite at 48 kHz",
                "prepared media are equal-power mono folds with deterministic hashes",
                "no audio device opened",
            ],
            "non_claims": [
                "private Squad-derived bytes are not licensed for redistribution",
                "authored weather recordings are not measured physical lightning channels or distributed-channel synthesis",
                "source binding is not playback, human listening, runtime DSP, linked callback, timing/RSS, device, thermal, true-peak, or delivered-ear-SPL evidence",
                "artifact_generated is not gamma-card captured or passed status",
            ],
        }
        (temporary / "report.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        temporary.rename(output)
    except BaseException:
        shutil.rmtree(temporary, ignore_errors=True)
        raise
    report_path = output / "report.json"
    print(json.dumps({"status": "artifact_generated", "report": str(report_path), "sha256": sha256(report_path)}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"qualify-gamma4-thunder: {error}", file=sys.stderr)
        raise SystemExit(1)
