#!/usr/bin/env python3
"""Validate a provisional S3 listening record.

Dependency-free (stdlib only). This enforces the two honest states the JSON
schema cannot express by itself:

  * an ``undecided`` template is allowed to carry null fixture/bundle hashes and
    an empty sign-off, because it is an incomplete, human-required record;
  * a ``pass`` or ``fail`` record must be a *completed* provisional record:
    lowercase 64-hex fixture and bundle hashes, at least one non-placeholder
    observation, a nonempty listener signature, and a valid ISO-8601 sign-off
    date.

The structural shape (known fields, comparison_order, the human-required
non-claim, ``requires_human_completion: true``) is checked first so a structural
mistake is reported on its own.

Exit status is 0 when the record is valid in its state, 1 otherwise. Run with a
path to a record JSON file, or no argument to validate the bundled template.
"""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path

HEX64 = re.compile(r"^[0-9a-f]{64}$")
ISO_DATE = re.compile(r"^[0-9]{4}-[0-9]{2}-[0-9]{2}$")
HUMAN_REQUIRED_NON_CLAIM = (
    "Human completion is required; this template alone is not a pass."
)
PLACEHOLDER_MARKERS = ("REPLACE", "REPLACE_WITH", "TODO", "TBD")
COMPARISON_ORDER = ["pathing_on", "pathing_off"]


class Invalid(Exception):
    """A validation failure with a human-readable reason."""


def _is_placeholder(text: str) -> bool:
    upper = text.upper()
    return any(marker in upper for marker in PLACEHOLDER_MARKERS)


def _check_structure(record: object) -> None:
    if not isinstance(record, dict):
        raise Invalid("record must be a JSON object")

    required = [
        "schema_version",
        "record_id",
        "fixture_id",
        "gate",
        "fixture_sha256",
        "bundle_manifest_sha256",
        "listener",
        "hrtf",
        "equipment",
        "comparison_order",
        "observations",
        "result",
        "date_iso",
        "sign_off",
        "requires_human_completion",
        "claims",
        "non_claims",
    ]
    missing = [key for key in required if key not in record]
    if missing:
        raise Invalid(f"missing required fields: {', '.join(missing)}")

    unknown = sorted(set(record) - set(required))
    if unknown:
        raise Invalid(f"unknown fields: {', '.join(unknown)}")

    if record["schema_version"] != "fightbox.listening.v1":
        raise Invalid("schema_version must be 'fightbox.listening.v1'")
    if record["gate"] != "S3":
        raise Invalid("gate must be 'S3'")
    if record["requires_human_completion"] is not True:
        raise Invalid("requires_human_completion must be true")

    fixture_id = record["fixture_id"]
    if not isinstance(fixture_id, str) or not re.match(
        r"^[a-z0-9][a-z0-9-]*$", fixture_id
    ):
        raise Invalid("fixture_id must match ^[a-z0-9][a-z0-9-]*$")

    if record["comparison_order"] != COMPARISON_ORDER:
        raise Invalid(
            "comparison_order must be exactly "
            f"{COMPARISON_ORDER}; got {record['comparison_order']!r}"
        )

    non_claims = record["non_claims"]
    if not isinstance(non_claims, list) or HUMAN_REQUIRED_NON_CLAIM not in non_claims:
        raise Invalid(
            "non_claims must contain the human-required statement: "
            f"{HUMAN_REQUIRED_NON_CLAIM!r}"
        )

    for key in ("listener", "hrtf", "equipment", "sign_off"):
        if not isinstance(record[key], dict):
            raise Invalid(f"{key} must be an object")
    for key in ("observations", "claims", "non_claims"):
        if not isinstance(record[key], list):
            raise Invalid(f"{key} must be an array")


def _hash_or_null(value: object, field: str) -> None:
    if value is None:
        return
    if not isinstance(value, str) or not HEX64.match(value):
        raise Invalid(
            f"{field} must be null (template) or a lowercase 64-hex SHA-256 string"
        )


def _check_undecided(record: dict) -> None:
    # An undecided template is an incomplete, human-required record: null hashes
    # and empty sign-off are the honest state.
    _hash_or_null(record["fixture_sha256"], "fixture_sha256")
    _hash_or_null(record["bundle_manifest_sha256"], "bundle_manifest_sha256")


def _check_completed(record: dict) -> None:
    # A pass/fail record must be a completed provisional record bound to a real
    # capture and a real human sign-off.
    for field in ("fixture_sha256", "bundle_manifest_sha256"):
        value = record[field]
        if not isinstance(value, str) or not HEX64.match(value):
            raise Invalid(
                f"a {record['result']} record requires {field} to be a lowercase "
                "64-hex SHA-256 over the actual capture"
            )

    observations = record["observations"]
    if not observations or not isinstance(observations, list):
        raise Invalid("a completed record needs at least one observation")
    real = [
        obs
        for obs in observations
        if isinstance(obs, dict)
        and obs.get("stimulus")
        and obs.get("observation")
        and not _is_placeholder(str(obs.get("observation", "")))
        and not _is_placeholder(str(obs.get("stimulus", "")))
    ]
    if not real:
        raise Invalid(
            "a completed record needs at least one non-placeholder observation"
        )

    sign_off = record["sign_off"]
    if not sign_off.get("listener_signed"):
        raise Invalid("sign_off.listener_signed must be populated for a completed record")
    if not ISO_DATE.match(str(sign_off.get("date_iso", ""))):
        raise Invalid(
            "sign_off.date_iso must be a valid ISO-8601 date for a completed record"
        )


def validate_record(record: object) -> None:
    _check_structure(record)
    result = record["result"]
    if result not in ("undecided", "pass", "fail"):
        raise Invalid("result must be 'undecided', 'pass', or 'fail'")
    if result == "undecided":
        _check_undecided(record)
    else:
        _check_completed(record)


def _load(path: Path) -> object:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise Invalid(f"{path}: not valid JSON ({exc.msg} at line {exc.lineno})") from exc
    except OSError as exc:
        raise Invalid(f"{path}: {exc.strerror}") from exc


def main(argv: list[str]) -> int:
    here = Path(__file__).resolve().parent
    # argv is sys.argv: argv[0] is the script path, argv[1] (if any) is the record.
    path = Path(argv[1]) if len(argv) > 1 else here / "s3-listening-record.template.json"
    try:
        record = _load(path)
        validate_record(record)
    except Invalid as exc:
        print(f"INVALID {path}: {exc}", file=sys.stderr)
        return 1
    print(f"OK      {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
