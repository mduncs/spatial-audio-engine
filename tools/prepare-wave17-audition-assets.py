#!/usr/bin/env python3
"""Prepare mono restartable Workbench assets from retained Wave 17 stems.

The exact stereo/mono inputs remain the evidence authority. These local audition
adaptations are downmixed/padded copies for the Workbench's mono source seam and
never open an audio device.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import os
import shutil
import struct
import sys
from array import array
from dataclasses import dataclass
from pathlib import Path

REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
SAMPLE_RATE_HZ = 48_000
TARGET_RMS_DBFS = -24.0
TARGET_PEAK_DBFS = -1.0
FOLD = math.sqrt(0.5)


@dataclass(frozen=True)
class InputSpec:
    asset_id: str
    source: Path
    source_sha256: str
    minimum_duration_s: float
    gamma_id: str
    role: str


def static_inputs() -> tuple[InputSpec, ...]:
    runs = Path("/path/to/spatial-audio/evidence")
    return (
        InputSpec("wave17-gamma1-artillery-200m", runs / "impulse-strip/b_morph_d0200m.wav", "a6c12d07b398d300b1121d18d9d33025c5ece28af7dd562d89a097749022cd64", 8.0, "gamma1", "artillery-distance-morph-200m"),
        InputSpec("wave17-gamma2-firework", runs / "listening-pack/LISTEN-firework-scene.wav", "1112394906b309b6587db60dbbe7aaed911d4a6ed6d402659b4d981614a84b40", 8.0, "gamma2", "elevated-firework-scene"),
        InputSpec("wave17-gamma3-snap-boom", runs / "impulse-strip/a_snap_then_boom_d030m.wav", "b5963dd4455c3ad45e6450162fc00e7d5afdffe5db9f4d7704d84a81ec7721eb", 8.0, "gamma3", "supersonic-snap-then-boom-30m"),
        InputSpec("wave17-gamma5-fast-mover-110mps", runs / "wave17-moving-golden-20260804T205357Z-ae69c4e/wave17-moving-point-110mps-ratification-candidate.LISTEN.wav", "51f1ff458e5c8c9dfda52fd1d06bc8befafebf943758ab72749b0c9fb49d1795", 6.0, "gamma5", "110mps-ratification-candidate"),
        InputSpec("wave17-gamma6-contention-mix", runs / "listening-pack/LISTEN-s6a-mix.wav", "30ee615b1515006e466bf54185036f5967516da56d12a715f41f869a52ad4950", 4.0, "gamma6", "four-source-contention-mix"),
        InputSpec("wave17-gamma7-owner-home", runs / "wave17-gamma7-owner-home-stem-20260811T224134Z/gamma7-owner-home-aperture.wav", "b9e37ea371b10f1ff6c60ba943118b2d805501c7338ff53088c883028adf6ca0", 20.0, "gamma7", "owner-home-aperture-states"),
        InputSpec("wave17-gamma8-toms-diner-walk", runs / "listening-pack/LISTEN-toms-diner-walk.wav", "bb2d21935e2d0d6a7d101ec05e5d2a68549c010b61591879d4e93a68f9524ed8", 0.0, "gamma8", "toms-diner-city-walk"),
        InputSpec("wave17-gamma9-neutral", runs / "wave17-gamma9-spectral-stems-20260811T234738Z/gamma9-neutral.wav", "ba014e93042c381e4dd639e98d6092fb42da7288df165d7cca45fe749ad4bcfb", 6.0, "gamma9", "neutral-ab"),
        InputSpec("wave17-gamma9-combined-once", runs / "wave17-gamma9-spectral-stems-20260811T234738Z/gamma9-combined-once.wav", "4ddbfa459b3e469281b0d3911388e457fe07319e85d6dd791a9787fea0093f37", 6.0, "gamma9", "five-stage-combined-once-ab"),
        InputSpec("wave17-gamma10-oracle", runs / "wave17-seam-exact-frame-stitch-20260811T125850Z/renders/oracle/stem-1-wave17-seam-pink.wav", "3a2748df2125a4621919b7b5bf19cbf1b37604c0051f2c7d264e0cb504c93ba1", 0.0, "gamma10", "monolithic-oracle-ab"),
        InputSpec("wave17-gamma10-cell-e1-n0", runs / "wave17-seam-exact-frame-stitch-20260811T125850Z/renders/e1-n0/stem-1-wave17-seam-pink.wav", "de38b1bd5100a0a31f9c17e4c3564b119c15d56772313ecb8faac58f6e77630c", 0.0, "gamma10", "production-cell-e1-n0-ab"),
    )


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def gamma0_inputs(root: Path) -> tuple[InputSpec, ...]:
    report_path = root / "report.json"
    report = json.loads(report_path.read_text(encoding="utf-8"))
    if report.get("schema_version") != "fightbox.gamma0-transport-pulse-capture.v1":
        raise RuntimeError("--gamma0-root does not contain the expected capture report")
    by_label = {stem["label"]: stem for stem in report["stems"]}
    result = []
    for label, minimum_duration in (("100m", 6.0), ("1km", 8.0), ("10km", 0.0)):
        stem = by_label[label]
        result.append(
            InputSpec(
                f"wave17-gamma0-transport-{label}",
                Path(stem["file"]),
                stem["sha256"],
                minimum_duration,
                "gamma0",
                f"macro-transport-{label}-physical-path-stem",
            )
        )
    return tuple(result)


def read_wav(path: Path) -> tuple[int, array]:
    payload = path.read_bytes()
    if len(payload) < 12 or payload[:4] != b"RIFF" or payload[8:12] != b"WAVE":
        raise RuntimeError(f"{path}: malformed RIFF/WAVE")
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
        raise RuntimeError(f"{path}: missing fmt/data chunk")
    tag, channels, sample_rate, _, _, bits = fmt
    if channels not in (1, 2) or sample_rate != SAMPLE_RATE_HZ:
        raise RuntimeError(f"{path}: expected 48 kHz mono/stereo WAV")
    samples = array("f")
    if tag == 3 and bits == 32 and len(data) % 4 == 0:
        samples.frombytes(data)
        if sys.byteorder != "little":
            samples.byteswap()
    elif tag == 1 and bits == 16 and len(data) % 2 == 0:
        integers = array("h")
        integers.frombytes(data)
        if sys.byteorder != "little":
            integers.byteswap()
        samples = array("f", (sample / 32768.0 for sample in integers))
    else:
        raise RuntimeError(f"{path}: expected PCM16 or float32 WAV")
    if not samples or len(samples) % channels or any(not math.isfinite(x) for x in samples):
        raise RuntimeError(f"{path}: invalid PCM")
    return channels, samples


def mono_adaptation(channels: int, interleaved: array, minimum_duration_s: float) -> tuple[array, float]:
    if channels == 1:
        mono = array("f", interleaved)
    else:
        mono = array(
            "f",
            (FOLD * (float(interleaved[i]) + float(interleaved[i + 1])) for i in range(0, len(interleaved), 2)),
        )
    raw_peak = max(abs(float(sample)) for sample in mono)
    safety_gain = min(1.0, 10.0 ** (TARGET_PEAK_DBFS / 20.0) / raw_peak) if raw_peak else 1.0
    if safety_gain < 1.0:
        mono = array("f", (float(sample) * safety_gain for sample in mono))
    target_frames = round(minimum_duration_s * SAMPLE_RATE_HZ)
    if len(mono) < target_frames:
        mono.extend(array("f", [0.0]) * (target_frames - len(mono)))
    return mono, safety_gain


def write_wav(path: Path, samples: array) -> None:
    pcm = array("f", samples)
    if sys.byteorder != "little":
        pcm.byteswap()
    data = pcm.tobytes()
    fmt = struct.pack("<HHIIHH", 3, 1, SAMPLE_RATE_HZ, SAMPLE_RATE_HZ * 4, 4, 32)
    body = b"WAVE" + b"fmt " + struct.pack("<I", len(fmt)) + fmt
    body += b"data" + struct.pack("<I", len(data)) + data
    path.write_bytes(b"RIFF" + struct.pack("<I", len(body)) + body)


def levels(samples: array) -> tuple[float, float, float]:
    peak = max(abs(float(sample)) for sample in samples)
    rms = math.sqrt(math.fsum(float(sample) ** 2 for sample in samples) / len(samples))
    peak_dbfs = 20.0 * math.log10(max(peak, 1.0e-12))
    rms_dbfs = 20.0 * math.log10(max(rms, 1.0e-12))
    target = min(TARGET_RMS_DBFS, rms_dbfs + TARGET_PEAK_DBFS - peak_dbfs)
    return peak_dbfs, rms_dbfs, target


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gamma0-root", type=Path, required=True)
    parser.add_argument("--wav-root", type=Path, default=REPOSITORY_ROOT / "fixtures/assets/wave17-audition")
    parser.add_argument("--descriptor-root", type=Path, default=REPOSITORY_ROOT / "fixtures/assets")
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    specs = gamma0_inputs(args.gamma0_root) + static_inputs()
    if not args.descriptor_root.is_dir() or args.report.exists():
        raise RuntimeError("descriptor root must exist and report must be absent")
    args.wav_root.mkdir(parents=True, exist_ok=False)
    created_descriptors: list[Path] = []
    try:
        entries = []
        for spec in specs:
            if not spec.source.is_file() or sha256(spec.source) != spec.source_sha256:
                raise RuntimeError(f"{spec.asset_id}: source identity mismatch")
            channels, source_pcm = read_wav(spec.source)
            mono, safety_gain = mono_adaptation(channels, source_pcm, spec.minimum_duration_s)
            wav_path = args.wav_root / f"{spec.asset_id}.wav"
            descriptor_path = args.descriptor_root / f"{spec.asset_id}.json"
            if descriptor_path.exists():
                raise RuntimeError(f"refusing to overwrite {descriptor_path}")
            write_wav(wav_path, mono)
            peak_dbfs, rms_dbfs, target_rms_dbfs = levels(mono)
            output_sha = sha256(wav_path)
            descriptor = {
                "schema_version": "fightbox.asset-descriptor.v1",
                "asset_id": spec.asset_id,
                "kind": "wav",
                "generator": {"wav": {"path": wav_path.relative_to(REPOSITORY_ROOT).as_posix() if wav_path.is_relative_to(REPOSITORY_ROOT) else str(wav_path), "sha256": output_sha, "start_frame": 0, "loop": True}},
                "channels": 1,
                "sample_rate_hz": SAMPLE_RATE_HZ,
                "duration_s": round(len(mono) / SAMPLE_RATE_HZ, 9),
                "target_rms_dbfs": round(target_rms_dbfs, 6),
                "expected_reference_rms_dbfs": round(rms_dbfs, 6),
                "calibration": {"applied_gain_db": round(target_rms_dbfs - rms_dbfs, 6)},
                "non_claims": [
                    "This descriptor makes no delivered-ear-SPL claim without output calibration.",
                    f"Audition adaptation of exact {spec.gamma_id} source SHA-256 {spec.source_sha256}; the original retained artifact remains evidence authority.",
                    "Stereo inputs are equal-power folded to mono and may be peak-safety attenuated/padded; this derivative is not a replacement canonical gamma stem.",
                    "The looping Workbench copy is private preparation for explicit human listening and is not itself a listening judgment.",
                ],
            }
            descriptor_path.write_text(json.dumps(descriptor, indent=2) + "\n", encoding="utf-8")
            created_descriptors.append(descriptor_path)
            entries.append({
                "asset_id": spec.asset_id,
                "gamma_id": spec.gamma_id,
                "role": spec.role,
                "source": str(spec.source),
                "source_sha256": spec.source_sha256,
                "source_channels": channels,
                "output": str(wav_path),
                "output_sha256": output_sha,
                "output_frame_count": len(mono),
                "output_duration_s": len(mono) / SAMPLE_RATE_HZ,
                "output_peak_dbfs": peak_dbfs,
                "output_rms_dbfs": rms_dbfs,
                "loader_target_rms_dbfs": target_rms_dbfs,
                "equal_power_fold": channels == 2,
                "peak_safety_gain": safety_gain,
                "minimum_duration_s": spec.minimum_duration_s,
                "descriptor": str(descriptor_path),
                "descriptor_sha256": sha256(descriptor_path),
            })
        report = {
            "schema_version": "fightbox.wave17-audition-assets.v1",
            "status": "prepared",
            "source_count": len(entries),
            "all_finite_mono_48khz": True,
            "assets": entries,
            "nonclaims": [
                "Audition derivatives do not replace exact retained gamma artifacts.",
                "No audio device was opened and no human listening occurred.",
                "Preparation does not prove callback, route, resource, device, or promotion gates.",
            ],
        }
        args.report.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    except BaseException:
        shutil.rmtree(args.wav_root, ignore_errors=True)
        for path in created_descriptors:
            path.unlink(missing_ok=True)
        raise
    print(json.dumps({"status": "prepared", "report": str(args.report), "sha256": sha256(args.report), "asset_count": len(specs)}))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"prepare-wave17-audition-assets: {error}", file=sys.stderr)
        raise SystemExit(1)
