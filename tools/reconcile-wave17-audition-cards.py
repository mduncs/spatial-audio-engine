#!/usr/bin/env python3
"""Emit a preserved all-planned γ-card successor for the Wave 17 audition scene.

This binds offline γ0/γ4 stems and the shared default-off Workbench preparation.
It never changes a card to captured/passed, fabricates resources, or records a
human judgment. The predecessor directory and manifest are never modified.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
from datetime import datetime, timezone
from pathlib import Path

REPOSITORY_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_PREDECESSOR = Path(
    "/path/to/spatial-audio/evidence/"
    "wave17-gamma-promotion-cards-20260812T004749Z"
)
DEFAULT_PREDECESSOR_MANIFEST = Path(
    "/path/to/spatial-audio/evidence/"
    "wave17-gamma-promotion-manifest-20260812T004749Z.json"
)
EXPECTED_PREDECESSOR_ROOT = (
    "198511265423c3032964207c04912ef9633bc324e4911ded5cc5518822bfb261"
)
CARD_ORDER = (
    "gamma0_transport_pulse",
    "gamma1_explosion_artillery",
    "gamma2_firework",
    "gamma3_supersonic_shot",
    "gamma4_thunder",
    "gamma5_fast_mover",
    "gamma6_contention",
    "gamma7_owner_home_aperture",
    "gamma8_toms_diner",
    "gamma9_spectral_composition",
    "gamma10_cell_boundary",
)
CARD_TO_FIXTURE_PREFIX = {
    card_id: card_id.split("_")[0].replace("gamma", "gamma") + "-"
    for card_id in CARD_ORDER
}
CARD_TO_FIXTURE_PREFIX["gamma10_cell_boundary"] = "gamma10-"


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load(path: Path) -> dict:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise RuntimeError(f"{path}: expected a JSON object")
    return value


def compact_bytes(value: dict) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def verify_predecessor(
    root: Path, manifest_path: Path, expected_root: str
) -> tuple[dict, dict[str, dict]]:
    manifest = load(manifest_path)
    if manifest.get("card_root_sha256") != expected_root:
        raise RuntimeError("predecessor manifest does not match --expected-predecessor-root")
    entries = manifest.get("cards")
    if not isinstance(entries, list) or tuple(entry.get("card_id") for entry in entries) != CARD_ORDER:
        raise RuntimeError("predecessor card order is not canonical")
    cards = {}
    root_lines = []
    for entry in entries:
        path = root / entry["file"]
        digest = sha256(path)
        if digest != entry.get("sha256"):
            raise RuntimeError(f"predecessor hash mismatch: {path}")
        card = load(path)
        if card.get("card_id") != entry["card_id"] or card.get("status") != "planned":
            raise RuntimeError(f"predecessor card is not the expected planned card: {path}")
        cards[card["card_id"]] = card
        root_lines.append(f"{card['card_id']} {digest}\n")
    computed = hashlib.sha256("".join(root_lines).encode("utf-8")).hexdigest()
    if computed != expected_root:
        raise RuntimeError("predecessor ordered root mismatch")
    return manifest, cards


def verify_inputs(
    gamma0_root: Path,
    gamma4_root: Path,
    audition_report_path: Path,
    fixture_path: Path,
    workbench_path: Path,
) -> tuple[dict, dict, dict, dict, str, str]:
    gamma0_report_path = gamma0_root / "report.json"
    gamma4_report_path = gamma4_root / "report.json"
    gamma0 = load(gamma0_report_path)
    gamma4 = load(gamma4_report_path)
    audition = load(audition_report_path)
    fixture = load(fixture_path)
    if gamma0.get("schema_version") != "fightbox.gamma0-transport-pulse-capture.v1" or gamma0.get("status") != "artifact_generated":
        raise RuntimeError("invalid γ0 capture report")
    if gamma4.get("schema_version") != "fightbox.gamma4-authored-thunder-capture.v1" or gamma4.get("status") != "artifact_generated":
        raise RuntimeError("invalid γ4 qualification report")
    if audition.get("schema_version") != "fightbox.wave17-audition-assets.v1" or audition.get("status") != "prepared":
        raise RuntimeError("invalid audition adaptation report")
    sources = fixture.get("sources")
    if not isinstance(sources, list) or len(sources) != 16:
        raise RuntimeError("audition fixture must contain exactly 16 sources")
    if any(source.get("default_enabled") is not False or source.get("restart_on_enable") is not True for source in sources):
        raise RuntimeError("every audition source must be default-off and restart-on-enable")
    ids = [source.get("id") for source in sources]
    asset_ids = [source.get("asset_id") for source in sources]
    if len(set(ids)) != 16 or len(set(asset_ids)) != 16:
        raise RuntimeError("audition fixture source and asset IDs must be unique")
    report_asset_ids = {entry.get("asset_id") for entry in audition.get("assets", [])}
    gamma4_asset_ids = {entry.get("asset_id") for entry in gamma4.get("candidates", [])}
    fixture_asset_ids = set(asset_ids)
    extra_asset_ids = fixture_asset_ids - report_asset_ids - gamma4_asset_ids
    if (
        not gamma4_asset_ids <= fixture_asset_ids
        or report_asset_ids & gamma4_asset_ids
        or any(not asset_id.startswith("squad-") for asset_id in extra_asset_ids)
    ):
        raise RuntimeError("fixture assets do not match prepared audition, γ4, and private Squad inputs")
    for asset_id in extra_asset_ids:
        descriptor = REPOSITORY_ROOT / "fixtures/assets" / f"{asset_id}.json"
        value = load(descriptor)
        if value.get("asset_id") != asset_id or value.get("kind") != "wav":
            raise RuntimeError(f"invalid private Squad descriptor: {asset_id}")
        wav = value.get("generator", {}).get("wav", {})
        path = Path(wav.get("path", ""))
        if not path.is_absolute():
            path = REPOSITORY_ROOT / path
        if sha256(path) != wav.get("sha256"):
            raise RuntimeError(f"private Squad WAV identity mismatch: {asset_id}")
    for stem in gamma0.get("stems", []):
        if sha256(Path(stem["file"])) != stem["sha256"]:
            raise RuntimeError(f"γ0 stem identity mismatch: {stem['label']}")
    gamma0_source = gamma0["source"]
    if sha256(Path(gamma0_source["descriptor"])) != gamma0_source["descriptor_sha256"] or sha256(Path(gamma0_source["wav"])) != gamma0_source["wav_sha256"]:
        raise RuntimeError("γ0 source identity mismatch")
    for entry in audition["assets"]:
        source = Path(entry["source"])
        output = Path(entry["output"])
        descriptor = Path(entry["descriptor"])
        if sha256(source) != entry["source_sha256"]:
            raise RuntimeError(f"audition source identity mismatch: {entry['asset_id']}")
        if sha256(output) != entry["output_sha256"] or sha256(descriptor) != entry["descriptor_sha256"]:
            raise RuntimeError(f"audition adaptation identity mismatch: {entry['asset_id']}")
    preparation_path = Path(gamma4["preparation_report"])
    if sha256(preparation_path) != gamma4["preparation_report_sha256"]:
        raise RuntimeError("γ4 preparation-report identity mismatch")
    for entry in gamma4["candidates"]:
        stem = Path(entry["stem"])
        if sha256(stem) != entry["stem_sha256"]:
            raise RuntimeError(f"γ4 stem identity mismatch: {entry['asset_id']}")
    if not workbench_path.is_file():
        raise RuntimeError("release Workbench binary is absent")
    return gamma0, gamma4, audition, fixture, sha256(fixture_path), sha256(workbench_path)


def append_scene_note(card: dict, text: str) -> None:
    listening = card["listening"]
    if listening.get("outcome") not in {"pending", "pass", "fail", "not_required"} or not isinstance(listening.get("listener_id"), str):
        raise RuntimeError(f"invalid predecessor listening identity/outcome for {card['card_id']}")
    listening["notes"] = listening["notes"].rstrip() + " " + text


def update_gamma0(card: dict, report: dict, report_sha: str) -> None:
    source = report["source"]
    atmosphere = report["atmosphere"]
    card["artifacts"]["source"] = {"status": "bound", "sha256": source["descriptor_sha256"]}
    card["atmosphere"] = {
        "status": "frozen",
        "observation_id": atmosphere["observation_id"],
        "temperature_c": atmosphere["temperature_c"],
        "relative_humidity_percent": atmosphere["relative_humidity_percent"],
        "pressure_pa": atmosphere["pressure_pa"],
        "coefficient_sha256": atmosphere["coefficient_sha256"],
    }
    card["quality_states"] = ["portable_runtime_offline_macro_transport_physical_path_stems"]
    card["voice_assignments"] = [{
        "source_id": report["source"]["source_id"],
        "logical_voice": 0,
        "detail_state": "offline_macro_schedule_and_physical_path",
        "event_role": "transport_pulse_100m_1km_10km",
    }]
    card["isolated_stems"] = [{
        "label": stem["label"],
        "content_sha256": stem["sha256"],
        "channels": stem["channels"],
        "frame_count": stem["frame_count"],
    } for stem in report["stems"]]
    card["resources"] = None
    card["listening"]["notes"] = (
        f"Pending/not_run. Public planner/scheduler physical-path stems cover 100 m, 1 km, and 10 km; "
        f"capture report SHA-256 {report_sha}. The atmosphere is an explicit diagnostic observation, not current weather. "
        "The 10 km stem is an offline macro-ingress proxy, not a live 10 km Workbench world or ordinary 2,048 m local-delay-ring path. "
        "Resources remain null; no device, playback, human listener, or promotion judgment occurred."
    )


def update_gamma4(card: dict, report: dict, report_sha: str) -> None:
    card["artifacts"]["source"] = {"status": "bound", "sha256": report["preparation_report_sha256"]}
    card["atmosphere"] = {
        "status": "not_applicable",
        "reason": "Private authored thunder candidates are audition sources, not a measured atmosphere observation or physical weather model.",
    }
    card["quality_states"] = ["private_authored_thunder_candidate_stems"]
    preferred = next(entry for entry in report["candidates"] if entry["asset_id"] == "squad-thunder-distant-15")
    card["voice_assignments"] = [{
        "source_id": preferred["asset_id"],
        "logical_voice": 0,
        "detail_state": "private_authored_long_weather_tail_candidate",
        "event_role": "thunder_event_candidate",
    }]
    card["isolated_stems"] = [{
        "label": entry["asset_id"],
        "content_sha256": entry["stem_sha256"],
        "channels": entry["metrics"]["channels"],
        "frame_count": entry["metrics"]["frame_count"],
    } for entry in report["candidates"]]
    card["resources"] = None
    card["listening"]["notes"] = (
        f"Pending/not_run. Two private authored-mono Squad-derived candidates are bound by qualification report SHA-256 {report_sha}: "
        "03 is the pressure-edge alternate and 15 is the proposed longer weather-tail source. Exact identity is provenance, not a redistribution grant or physical thunder measurement. "
        "Resources remain null; no device, playback, human listener, or promotion judgment occurred."
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--predecessor", type=Path, default=DEFAULT_PREDECESSOR)
    parser.add_argument("--predecessor-manifest", type=Path, default=DEFAULT_PREDECESSOR_MANIFEST)
    parser.add_argument("--expected-predecessor-root", default=EXPECTED_PREDECESSOR_ROOT)
    parser.add_argument("--gamma0-root", type=Path, required=True)
    parser.add_argument("--gamma4-root", type=Path, required=True)
    parser.add_argument("--audition-report", type=Path, required=True)
    parser.add_argument("--fixture", type=Path, default=REPOSITORY_ROOT / "fixtures/city/wave17-gamma-audition/fixture.json")
    parser.add_argument("--workbench", type=Path, default=REPOSITORY_ROOT / "target/release/fightbox-workbench")
    parser.add_argument("--run-tag", required=True, help="UTC tag in YYYYMMDDTHHMMSSZ form")
    parser.add_argument("--output-root", type=Path, required=True)
    parser.add_argument("--manifest", type=Path, required=True)
    args = parser.parse_args()
    try:
        generated_at = datetime.strptime(args.run_tag, "%Y%m%dT%H%M%SZ").replace(tzinfo=timezone.utc)
    except ValueError as error:
        raise RuntimeError("--run-tag must be UTC YYYYMMDDTHHMMSSZ") from error
    if args.output_root.exists() or args.manifest.exists():
        raise RuntimeError("refusing to overwrite card output or manifest")
    if len(args.expected_predecessor_root) != 64 or any(
        character not in "0123456789abcdef" for character in args.expected_predecessor_root
    ):
        raise RuntimeError("--expected-predecessor-root must be lowercase SHA-256")

    predecessor_manifest, cards = verify_predecessor(
        args.predecessor, args.predecessor_manifest, args.expected_predecessor_root
    )
    predecessor_listening = {
        card_id: (card["listening"]["listener_id"], card["listening"]["outcome"])
        for card_id, card in cards.items()
    }
    gamma0, gamma4, audition, fixture, fixture_sha, workbench_sha = verify_inputs(
        args.gamma0_root, args.gamma4_root, args.audition_report, args.fixture, args.workbench
    )
    gamma0_report_sha = sha256(args.gamma0_root / "report.json")
    gamma4_report_sha = sha256(args.gamma4_root / "report.json")
    audition_report_sha = sha256(args.audition_report)
    update_gamma0(cards["gamma0_transport_pulse"], gamma0, gamma0_report_sha)
    update_gamma4(cards["gamma4_thunder"], gamma4, gamma4_report_sha)

    fixture_sources = fixture["sources"]
    for card_id in CARD_ORDER:
        card = cards[card_id]
        if card.get("status") != "planned" or card.get("resources") is not None:
            raise RuntimeError(f"{card_id}: successor must remain planned with resources null")
        card["run_id"] = f"wave17-{card_id.replace('_', '-')}-audition-prepared-{args.run_tag}"
        prefix = CARD_TO_FIXTURE_PREFIX[card_id]
        source_ids = [source["id"] for source in fixture_sources if source["id"].startswith(prefix)]
        if not source_ids:
            raise RuntimeError(f"{card_id}: no fixture source binding")
        append_scene_note(
            card,
            "Shared default-off/restart-on-enable Workbench audition binding: "
            f"fixture SHA-256 {fixture_sha}; adaptation report SHA-256 {audition_report_sha}; "
            f"release Workbench SHA-256 {workbench_sha}; fixture source IDs {', '.join(source_ids)}. "
            "This preparation is not playback, a resource observation, or a new human judgment; any predecessor listening identity/outcome is retained unchanged.",
        )
        if (card["listening"]["listener_id"], card["listening"]["outcome"]) != predecessor_listening[card_id]:
            raise RuntimeError(f"{card_id}: listening identity/outcome changed during reconciliation")
        card["failure_reason"] = None

    try:
        import jsonschema
    except ImportError as error:
        raise RuntimeError("jsonschema is required; run with `uv run --with jsonschema`") from error
    schema_path = REPOSITORY_ROOT / "fixtures/gamma-card.schema.json"
    schema = load(schema_path)
    validator = jsonschema.Draft202012Validator(schema)
    temp_root = args.output_root.with_name(args.output_root.name + f".tmp-{os.getpid()}")
    temp_manifest = args.manifest.with_name(args.manifest.name + f".tmp-{os.getpid()}")
    temp_root.mkdir(parents=True, exist_ok=False)
    try:
        card_entries = []
        root_lines = []
        for card_id in CARD_ORDER:
            card = cards[card_id]
            validator.validate(card)
            payload = compact_bytes(card)
            filename = f"{card_id}.json"
            path = temp_root / filename
            path.write_bytes(payload)
            digest = sha256(path)
            card_entries.append({"card_id": card_id, "status": "planned", "file": filename, "sha256": digest})
            root_lines.append(f"{card_id} {digest}\n")
        card_root_sha = hashlib.sha256("".join(root_lines).encode("utf-8")).hexdigest()
        manifest = {
            "schema_version": "fightbox.gamma-promotion-manifest.v1",
            "generated_at_utc": generated_at.strftime("%Y-%m-%dT%H:%M:%SZ"),
            "repository": str(REPOSITORY_ROOT),
            "card_schema": str(schema_path),
            "card_schema_sha256": sha256(schema_path),
            "engine_revision": predecessor_manifest["engine_revision"],
            "canonical_encoding": "UTF-8 compact JSON, schema field order, no trailing newline",
            "card_root_algorithm": "sha256(UTF-8 bytes of one line per required card in schema order: card_id + U+0020 + card_sha256 + U+000A)",
            "card_root_sha256": card_root_sha,
            "predecessor_card_root_sha256": args.expected_predecessor_root,
            "cards": card_entries,
        }
        temp_manifest.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
        args.output_root.parent.mkdir(parents=True, exist_ok=True)
        args.manifest.parent.mkdir(parents=True, exist_ok=True)
        temp_root.rename(args.output_root)
        try:
            temp_manifest.rename(args.manifest)
        except BaseException:
            shutil.rmtree(args.output_root, ignore_errors=True)
            raise
    except BaseException:
        shutil.rmtree(temp_root, ignore_errors=True)
        temp_manifest.unlink(missing_ok=True)
        raise
    print(json.dumps({
        "status": "all_planned_successor_emitted",
        "output_root": str(args.output_root),
        "manifest": str(args.manifest),
        "manifest_sha256": sha256(args.manifest),
        "card_root_sha256": card_root_sha,
        "predecessor_card_root_sha256": args.expected_predecessor_root,
    }, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except RuntimeError as error:
        print(f"reconcile-wave17-audition-cards: {error}", file=os.sys.stderr)
        raise SystemExit(1)
