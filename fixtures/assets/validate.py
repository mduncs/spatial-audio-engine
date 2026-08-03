#!/usr/bin/env python3
"""Validate a deterministic asset descriptor.

Dependency-free (stdlib only). Enforces the structural contract in
``asset.schema.json`` plus the cross-field rules JSON Schema cannot express:

  * the selected ``kind`` must carry *exactly* its matching generator block, and
    no other generator block (no mismatched kind/generator);
  * file-backed WAV descriptors must carry the pinned path/hash/frame/loop
    contract and the workbench's mono 48 kHz decode format;
  * generator frequencies must be finite, positive, below Nyquist for the
    declared sample rate, and unique;
  * ``target_rms_dbfs`` must be a finite JSON number strictly below 0 dBFS;
  * duration, sample rate, and channels must be in the supported ranges;
  * the mandatory no-delivered-ear-SPL non-claim must be present.

Run with a path to a descriptor JSON file, or no argument to validate every
``*.json`` descriptor in this directory (except the schema itself).
"""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

MANDATORY_NON_CLAIM = (
    "This descriptor makes no delivered-ear-SPL claim without output calibration."
)
KINDS = ("sine", "multitone", "pink_like", "wav")


class Invalid(Exception):
    """A validation failure with a human-readable reason."""


def _check_common(record: dict) -> None:
    required = [
        "schema_version",
        "asset_id",
        "kind",
        "generator",
        "channels",
        "sample_rate_hz",
        "duration_s",
        "target_rms_dbfs",
        "expected_reference_rms_dbfs",
        "calibration",
        "non_claims",
    ]
    missing = [key for key in required if key not in record]
    if missing:
        raise Invalid(f"missing required fields: {', '.join(missing)}")

    unknown = sorted(set(record) - set(required))
    if unknown:
        raise Invalid(f"unknown fields: {', '.join(unknown)}")

    if record["schema_version"] != "fightbox.asset-descriptor.v1":
        raise Invalid("schema_version must be 'fightbox.asset-descriptor.v1'")

    if record["kind"] not in KINDS:
        raise Invalid(f"kind must be one of {KINDS}; got {record['kind']!r}")

    if record["channels"] not in (1, 2):
        raise Invalid("channels must be 1 or 2")

    rate = record["sample_rate_hz"]
    if not isinstance(rate, int) or isinstance(rate, bool) or rate < 1:
        raise Invalid("sample_rate_hz must be a positive integer")

    duration = record["duration_s"]
    if not isinstance(duration, (int, float)) or isinstance(duration, bool):
        raise Invalid("duration_s must be a number")
    if not math.isfinite(duration) or duration <= 0.0:
        raise Invalid("duration_s must be finite and positive")

    target = record["target_rms_dbfs"]
    if not isinstance(target, (int, float)) or isinstance(target, bool):
        raise Invalid("target_rms_dbfs must be a JSON number")
    if not math.isfinite(target):
        raise Invalid("target_rms_dbfs must be finite (no NaN/Infinity)")
    if target >= 0.0:
        raise Invalid("target_rms_dbfs must be strictly below 0 dBFS")

    non_claims = record["non_claims"]
    if not isinstance(non_claims, list) or MANDATORY_NON_CLAIM not in non_claims:
        raise Invalid(
            "non_claims must contain the mandatory statement: "
            f"{MANDATORY_NON_CLAIM!r}"
        )

    generator = record["generator"]
    if not isinstance(generator, dict):
        raise Invalid("generator must be an object")
    if record["kind"] == "wav":
        if "module" in generator:
            raise Invalid("wav kind must not declare generator.module")
    elif generator.get("module") != "fightbox_evidence::signal":
        raise Invalid("generator.module must be 'fightbox_evidence::signal'")


def _check_frequencies(rate: int, frequencies: list) -> None:
    nyquist = rate / 2.0
    seen: set[float] = set()
    for f in frequencies:
        if not isinstance(f, (int, float)) or isinstance(f, bool):
            raise Invalid("multitone frequencies must be JSON numbers")
        if not math.isfinite(f) or f <= 0.0:
            raise Invalid(f"frequency must be finite and positive; got {f!r}")
        if f >= nyquist:
            raise Invalid(
                f"frequency {f} must be below Nyquist ({nyquist}) for "
                f"sample_rate_hz {rate}"
            )
        if f in seen:
            raise Invalid(f"frequencies must be unique; {f} repeats")
        seen.add(f)


def _check_generator(record: dict) -> None:
    kind = record["kind"]
    generator = record["generator"]
    present = [k for k in KINDS if k in generator]
    if present != [kind]:
        raise Invalid(
            f"kind {kind!r} requires exactly the generator.{kind} block; "
            f"found generator blocks {present!r}"
        )

    block = generator[kind]
    if not isinstance(block, dict):
        raise Invalid(f"generator.{kind} must be an object")

    rate = record["sample_rate_hz"]
    if kind == "sine":
        freq = block.get("frequency_hz")
        if freq is None:
            raise Invalid("generator.sine.frequency_hz is required")
        _check_frequencies(rate, [freq])
    elif kind == "multitone":
        freqs = block.get("frequencies_hz")
        if not isinstance(freqs, list) or not freqs:
            raise Invalid("generator.multitone.frequencies_hz must be a non-empty array")
        _check_frequencies(rate, freqs)
    elif kind == "pink_like":
        seed = block.get("seed")
        if not isinstance(seed, int) or isinstance(seed, bool) or seed < 0:
            raise Invalid("generator.pink_like.seed must be a non-negative integer")
    elif kind == "wav":
        required = {"path", "sha256", "start_frame", "loop"}
        missing = sorted(required - set(block))
        unknown = sorted(set(block) - required)
        if missing:
            raise Invalid(f"generator.wav missing fields: {', '.join(missing)}")
        if unknown:
            raise Invalid(f"generator.wav unknown fields: {', '.join(unknown)}")
        path = block["path"]
        sha256 = block["sha256"]
        start_frame = block["start_frame"]
        loop = block["loop"]
        if not isinstance(path, str) or not path:
            raise Invalid("generator.wav.path must be a non-empty string")
        if (
            not isinstance(sha256, str)
            or len(sha256) != 64
            or any(character not in "0123456789abcdef" for character in sha256)
        ):
            raise Invalid("generator.wav.sha256 must be 64 lowercase hex characters")
        if (
            not isinstance(start_frame, int)
            or isinstance(start_frame, bool)
            or start_frame < 0
        ):
            raise Invalid("generator.wav.start_frame must be a non-negative integer")
        if not isinstance(loop, bool):
            raise Invalid("generator.wav.loop must be boolean")
        if record["channels"] != 1:
            raise Invalid("wav assets must declare channels=1")
        if record["sample_rate_hz"] != 48_000:
            raise Invalid("wav assets must declare sample_rate_hz=48000")


def validate_descriptor(record: object) -> None:
    if not isinstance(record, dict):
        raise Invalid("descriptor must be a JSON object")
    _check_common(record)
    _check_generator(record)


def _load(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise Invalid(f"{path}: not valid JSON ({exc.msg} at line {exc.lineno})") from exc
    except OSError as exc:
        raise Invalid(f"{path}: {exc.strerror}") from exc


def _validate_path(path: Path) -> bool:
    try:
        record = _load(path)
        validate_descriptor(record)
    except Invalid as exc:
        print(f"INVALID {path}: {exc}", file=sys.stderr)
        return False
    print(f"OK      {path}")
    return True


def main(argv: list[str]) -> int:
    here = Path(__file__).resolve().parent
    # argv is sys.argv: argv[0] is the script path, argv[1:] are targets.
    targets = [Path(arg) for arg in argv[1:]]
    if not targets:
        targets = sorted(p for p in here.glob("*.json") if p.name != "asset.schema.json")
    ok = True
    for path in targets:
        if not _validate_path(path):
            ok = False
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
