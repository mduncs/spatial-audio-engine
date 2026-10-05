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
KINDS = ("sine", "multitone", "pink_like", "wav", "song")
SOURCE_SCHEMA = "fightbox.source-asset.v1"
CANONICAL_ARTIFACT_PREFIX = "fightbox.canonical-audio.v1:sha256:"


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

    unknown = sorted(set(record) - set(required) - {"onsets_s"})
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

    onsets = record.get("onsets_s")
    if onsets is not None:
        if not isinstance(onsets, list):
            raise Invalid("onsets_s must be an array")
        previous = None
        for index, onset in enumerate(onsets):
            if not isinstance(onset, (int, float)) or isinstance(onset, bool):
                raise Invalid(f"onsets_s[{index}] must be a JSON number")
            if not math.isfinite(onset) or onset < 0.0 or onset >= duration:
                raise Invalid(
                    f"onsets_s[{index}] must be finite and in [0, duration_s)"
                )
            if previous is not None and onset <= previous:
                raise Invalid("onsets_s must be strictly ascending")
            previous = onset

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
    if record["kind"] in ("wav", "song"):
        if "module" in generator:
            raise Invalid("file-backed kind must not declare generator.module")
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
    elif kind == "song":
        if set(block) != {"path"} or not isinstance(block["path"], str) or not block["path"].strip():
            raise Invalid("generator.song requires only a non-empty path")
        if rate != 48000 or record["target_rms_dbfs"] != -14:
            raise Invalid("song uses decoded 48 kHz and nominal -14 dBFS")
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


def _exact_object(value: object, path: str, required: set[str], optional: set[str] = set()) -> dict:
    if not isinstance(value, dict):
        raise Invalid(f"{path} must be an object")
    missing = sorted(required - set(value))
    unknown = sorted(set(value) - required - optional)
    if missing:
        raise Invalid(f"{path} missing fields: {', '.join(missing)}")
    if unknown:
        raise Invalid(f"{path} unknown fields: {', '.join(unknown)}")
    return value


def _sha(value: object, path: str) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise Invalid(f"{path} must be 64 lowercase hex characters")
    return value


def _canonical_artifact_id(value: object, path: str) -> str:
    if not isinstance(value, str) or not value.startswith(CANONICAL_ARTIFACT_PREFIX):
        raise Invalid(f"{path} must begin with {CANONICAL_ARTIFACT_PREFIX}")
    digest = value[len(CANONICAL_ARTIFACT_PREFIX):]
    _sha(digest, f"{path} hash")
    return value


def _finite(value: object, path: str) -> float:
    if not isinstance(value, (int, float)) or isinstance(value, bool) or not math.isfinite(value):
        raise Invalid(f"{path} must be a finite JSON number")
    return float(value)


def _check_source_format(value: object, path: str, canonical: bool = False) -> dict:
    record = _exact_object(
        value, path, {"container", "codec", "sample_rate_hz", "layout", "lossy"}
    )
    if record["layout"] not in ("mono", "stereo_lr"):
        raise Invalid(f"{path}.layout must be mono or stereo_lr")
    rate = record["sample_rate_hz"]
    if not isinstance(rate, int) or isinstance(rate, bool) or rate < 1:
        raise Invalid(f"{path}.sample_rate_hz must be a positive integer")
    if not isinstance(record["lossy"], bool):
        raise Invalid(f"{path}.lossy must be boolean")
    combination = (record["container"], record["codec"], record["lossy"])
    supported = combination in {
        ("deterministic_generator", "sine_generator", False),
        ("deterministic_generator", "multitone_generator", False),
        ("deterministic_generator", "pink_like_generator", False),
        ("wav", "pcm_integer", False),
        ("wav", "pcm_float", False),
        ("aiff", "pcm_integer", False),
        ("aiff", "pcm_float", False),
        ("caf", "pcm_integer", False),
        ("caf", "pcm_float", False),
        ("flac", "flac", False),
        ("m4a", "aac", True),
        ("mp3", "mp3", True),
    }
    if canonical:
        supported = combination == (
            "fightbox_planar_chunks_v1",
            "pcm_f32_planar_le",
            False,
        ) and rate == 48_000
    if not supported:
        raise Invalid(f"{path} has an unsupported container/codec/lossy combination")
    return record


def _check_levels(value: object, path: str) -> tuple[float, float]:
    record = _exact_object(value, path, {"rms_dbfs", "true_peak_dbtp"})
    rms = _finite(record["rms_dbfs"], f"{path}.rms_dbfs")
    peak = _finite(record["true_peak_dbtp"], f"{path}.true_peak_dbtp")
    if rms > peak:
        raise Invalid(f"{path}.rms_dbfs must not exceed true_peak_dbtp")
    return rms, peak


def _check_source_asset(record: dict) -> None:
    required = {
        "schema_version", "asset_id", "layout", "presentation_provenance",
        "compatible_geometries", "original", "canonical", "canonicalization",
        "measurements", "motion", "rights", "seekability",
    }
    _exact_object(record, "descriptor", required, {"derivation"})
    if record["schema_version"] != SOURCE_SCHEMA:
        raise Invalid(f"schema_version must be {SOURCE_SCHEMA!r}")
    asset_id = record["asset_id"]
    if not isinstance(asset_id, str) or not asset_id or any(
        character not in "abcdefghijklmnopqrstuvwxyz0123456789-" for character in asset_id
    ) or asset_id[0] == "-":
        raise Invalid("asset_id must match ^[a-z0-9][a-z0-9-]*$")
    layout = record["layout"]
    provenance = record["presentation_provenance"]
    expected_geometries = {
        ("mono", "native_mono"): ["point", "multi_point", "line_segment"],
        ("mono", "mono_expanded"): ["stereo_image"],
        ("stereo_lr", "authored_stereo"): ["stereo_image"],
    }.get((layout, provenance))
    if expected_geometries is None:
        raise Invalid(f"unsupported layout/presentation_provenance combination {layout}/{provenance}")
    if record["compatible_geometries"] != expected_geometries:
        raise Invalid(f"compatible_geometries must be exactly {expected_geometries!r}")

    original = _exact_object(record["original"], "original", {"content_sha256", "format"})
    _sha(original["content_sha256"], "original.content_sha256")
    original_format = _check_source_format(original["format"], "original.format")
    if original_format["layout"] != layout:
        raise Invalid("original.format.layout must match descriptor layout")

    canonical = _exact_object(
        record["canonical"], "canonical",
        {"artifact_id", "canonical_pcm_sha256", "format", "frame_count", "chunk_frames", "chunk_storage"},
    )
    canonical_hash = _sha(canonical["canonical_pcm_sha256"], "canonical.canonical_pcm_sha256")
    expected_identity = f"fightbox.canonical-audio.v1:sha256:{canonical_hash}"
    if canonical["artifact_id"] != expected_identity:
        raise Invalid(f"canonical.artifact_id must be {expected_identity}")
    canonical_format = _check_source_format(canonical["format"], "canonical.format", canonical=True)
    if canonical_format["layout"] != layout:
        raise Invalid("canonical.format.layout must match descriptor layout")
    if not isinstance(canonical["frame_count"], int) or isinstance(canonical["frame_count"], bool) or canonical["frame_count"] < 1:
        raise Invalid("canonical.frame_count must be a positive integer")
    if canonical["chunk_frames"] != 48_000:
        raise Invalid("canonical.chunk_frames must be 48000")
    if canonical["chunk_storage"] != "raw_or_zstd_if_smaller":
        raise Invalid("canonical.chunk_storage must be raw_or_zstd_if_smaller")

    toolchain = _exact_object(record["canonicalization"], "canonicalization", {"decoder", "resampler"})
    for name in ("decoder", "resampler"):
        tool = _exact_object(toolchain[name], f"canonicalization.{name}", {"implementation", "revision", "settings_sha256"})
        if not isinstance(tool["implementation"], str) or not tool["implementation"].strip():
            raise Invalid(f"canonicalization.{name}.implementation must not be empty")
        if not isinstance(tool["revision"], str) or not tool["revision"].strip():
            raise Invalid(f"canonicalization.{name}.revision must not be empty")
        _sha(tool["settings_sha256"], f"canonicalization.{name}.settings_sha256")

    measurements = _exact_object(record["measurements"], "measurements", {"analysis_revision", "per_channel", "aggregate"}, {"stereo"})
    if not isinstance(measurements["analysis_revision"], str) or not measurements["analysis_revision"].strip():
        raise Invalid("measurements.analysis_revision must not be empty")
    channels = measurements["per_channel"]
    if not isinstance(channels, list):
        raise Invalid("measurements.per_channel must be an array")
    expected_labels = ["mono"] if layout == "mono" else ["left", "right"]
    levels = []
    labels = []
    for index, value in enumerate(channels):
        channel = _exact_object(value, f"measurements.per_channel[{index}]", {"channel", "levels"})
        labels.append(channel["channel"])
        levels.append(_check_levels(channel["levels"], f"measurements.per_channel[{index}].levels"))
    if labels != expected_labels:
        raise Invalid(f"measurements.per_channel labels must be {expected_labels!r}")
    aggregate = _check_levels(measurements["aggregate"], "measurements.aggregate")
    expected_rms = 10.0 * math.log10(sum(10.0 ** (item[0] / 10.0) for item in levels) / len(levels))
    if abs(aggregate[0] - expected_rms) > 0.001 or abs(aggregate[1] - max(item[1] for item in levels)) > 0.001:
        raise Invalid("measurements.aggregate must be the channel-energy RMS and maximum true peak")
    stereo = measurements.get("stereo")
    if layout == "mono" and stereo is not None:
        raise Invalid("mono measurements must not carry stereo analysis")
    if layout == "stereo_lr":
        stereo = _exact_object(stereo, "measurements.stereo", {"pca", "correlation", "mono_compatibility"})
        pca = _exact_object(stereo["pca"], "measurements.stereo.pca", {"center_weights_lr", "width_weights_lr", "center_energy", "width_energy"})
        center, width = pca["center_weights_lr"], pca["width_weights_lr"]
        if not isinstance(center, list) or not isinstance(width, list) or len(center) != 2 or len(width) != 2:
            raise Invalid("PCA center/width vectors must each contain two numbers")
        values = [_finite(item, "PCA vector") for item in center + width]
        c0, c1, w0, w1 = values
        if c0 < 0.0 or (c0 == 0.0 and c1 < 0.0):
            raise Invalid(
                "PCA center sign must use the deterministic first-nonzero-positive convention"
            )
        if abs(c0*c0 + c1*c1 - 1) > 1e-6 or abs(w0*w0 + w1*w1 - 1) > 1e-6 or abs(c0*w0 + c1*w1) > 1e-6 or abs(w0 + c1) > 1e-6 or abs(w1 - c0) > 1e-6:
            raise Invalid("PCA vectors must use the deterministic orthonormal convention")
        center_energy = _finite(pca["center_energy"], "pca.center_energy")
        width_energy = _finite(pca["width_energy"], "pca.width_energy")
        if width_energy < 0 or center_energy < width_energy:
            raise Invalid("PCA energies require center >= width >= 0")
        correlation = _finite(stereo["correlation"], "measurements.stereo.correlation")
        if not -1 <= correlation <= 1:
            raise Invalid("stereo correlation must be in [-1, 1]")
        mono = _exact_object(stereo["mono_compatibility"], "measurements.stereo.mono_compatibility", {"status", "score", "short_lag_comb_correlation_delta", "regular_notch_depth_delta_db"})
        score = _finite(mono["score"], "mono_compatibility.score")
        comb = _finite(mono["short_lag_comb_correlation_delta"], "mono_compatibility.short_lag_comb_correlation_delta")
        notch = _finite(mono["regular_notch_depth_delta_db"], "mono_compatibility.regular_notch_depth_delta_db")
        if mono["status"] != "compatible" or not 0 <= score <= 1 or not 0 <= comb <= 0.10 or not 0 <= notch <= 6.0:
            raise Invalid("authored stereo fails the mono-compatibility admission limits")

    motion = _exact_object(record["motion"], "motion", {"recording_carries_motion", "evidence", "description"})
    if not isinstance(motion["recording_carries_motion"], bool) or not isinstance(motion["description"], str) or not motion["description"].strip():
        raise Invalid("motion must carry a boolean recording_carries_motion and non-empty description")
    allowed_motion = {(True, "recorded_motion"), (False, "authored_dry"), (False, "legacy_unspecified")}
    if (motion["recording_carries_motion"], motion["evidence"]) not in allowed_motion:
        raise Invalid("motion.recording_carries_motion conflicts with motion.evidence")
    rights = _exact_object(record["rights"], "rights", {"status", "license", "evidence"})
    if rights["status"] not in ("generated", "public_domain", "licensed", "restricted", "legacy_unverified") or not all(isinstance(rights[key], str) and rights[key].strip() for key in ("license", "evidence")):
        raise Invalid("rights status/license/evidence are invalid")
    if record["seekability"] not in ("sample_accurate", "deterministic_generator"):
        raise Invalid("canonical assets must be seekable")
    derivation = record.get("derivation")
    if provenance == "mono_expanded":
        derivation = _exact_object(derivation, "derivation", {"source_artifact_id", "recipe_sha256"})
        _canonical_artifact_id(
            derivation["source_artifact_id"], "derivation.source_artifact_id"
        )
        _sha(derivation["recipe_sha256"], "derivation.recipe_sha256")
    elif derivation is not None:
        raise Invalid("derivation is only valid for mono_expanded")


def validate_descriptor(record: object) -> None:
    if not isinstance(record, dict):
        raise Invalid("descriptor must be a JSON object")
    if record.get("schema_version") == SOURCE_SCHEMA:
        _check_source_asset(record)
        return
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
        targets = sorted(p for p in here.glob("*.json") if not p.name.endswith(".schema.json"))
    ok = True
    for path in targets:
        if not _validate_path(path):
            ok = False
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
