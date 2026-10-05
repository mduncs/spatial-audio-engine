#!/bin/zsh
# Prepare and compile the Wave 17 all-unlistened audition scene.
# This never launches the Workbench or opens an audio device.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RUNS="/path/to/spatial-audio/evidence"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
JOB="$RUNS/wave17-audition-preparation-$STAMP"
G0="$RUNS/wave17-gamma0-transport-pulse-$STAMP"
G4="$RUNS/wave17-gamma4-thunder-stems-$STAMP"
G4_PREP="$JOB/gamma4-preparation.json"
AUDITION_REPORT="$JOB/audition-assets.json"
CARDS="$RUNS/wave17-gamma-promotion-cards-$STAMP"
CARD_MANIFEST="$RUNS/wave17-gamma-promotion-manifest-$STAMP.json"
SDK="$ROOT/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
mkdir "$JOB"
LOG="$JOB/progress.log"
exec > >(tee -a "$LOG") 2>&1

PATH_CLEAN="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
clean() {
  env -i \
    HOME="$HOME" \
    USER="${USER:-$(id -un)}" \
    TMPDIR="${TMPDIR:-/tmp}" \
    PATH="$PATH_CLEAN" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
    RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    STEAM_AUDIO_SDK_DIR="$SDK" \
    "$@"
}

cd "$ROOT"
print -r -- "[1/10] source formatting and whitespace gates"
clean cargo +stable fmt --all -- --check
clean git diff --check

print -r -- "[2/10] focused gamma0 and Workbench source tests"
clean cargo +stable test -p fightbox-cli --example gamma0_capture
clean cargo +stable test -p fightbox-workbench \
  restartable_loop_is_silent_while_disabled_and_rewinds_on_enable
clean cargo +stable test -p fightbox-workbench \
  wave17_gamma_audition_fixture_starts_silent_and_fills_one_scene
clean cargo +stable test -p fightbox-workbench \
  restartable_default_off_source_ignores_saved_enabled_state_at_startup
clean cargo +stable test -p fightbox-workbench \
  device_selection_requires_explicit_audio_start

print -r -- "[3/10] gamma0 release capture (100 m, 1 km, 10 km; no device)"
clean cargo +stable run --release -p fightbox-cli --example gamma0_capture -- \
  --output "$G0"

print -r -- "[4/10] selected private Squad thunder preparation (no playback)"
clean python3 tools/prepare-squad-assets.py \
  --asset squad-thunder-distant-03 \
  --asset squad-thunder-distant-15 \
  --report "$G4_PREP" > "$JOB/gamma4-preparation.stdout.json"

print -r -- "[5/10] gamma4 source/stem qualification (no playback)"
clean python3 tools/qualify-gamma4-thunder.py \
  --preparation-report "$G4_PREP" \
  --output "$G4"

print -r -- "[6/10] exact-source-bound mono Workbench adaptations"
clean python3 tools/prepare-wave17-audition-assets.py \
  --gamma0-root "$G0" \
  --report "$AUDITION_REPORT"

print -r -- "[7/10] descriptor/schema and all-16-asset load gates"
clean python3 fixtures/assets/validate.py
clean uv run --with jsonschema python -c \
  'import json,sys,jsonschema; s=json.load(open(sys.argv[1])); d=json.load(open(sys.argv[2])); jsonschema.Draft202012Validator(s).validate(d); print("PASS sources="+str(len(d["sources"])))' \
  fixtures/workbench.schema.json fixtures/city/wave17-gamma-audition/fixture.json
clean cargo +stable test -p fightbox-workbench \
  prepared_wave17_audition_scene_assets_load_as_finite_mono -- --ignored --nocapture

print -r -- "[8/10] linked live-output Workbench release build"
clean cargo +stable build --release -p fightbox-workbench \
  --features linked-sdk,live-output

print -r -- "[9/10] emit preserved all-planned γ-card successor"
clean uv run --with jsonschema python tools/reconcile-wave17-audition-cards.py \
  --gamma0-root "$G0" \
  --gamma4-root "$G4" \
  --audition-report "$AUDITION_REPORT" \
  --run-tag "$STAMP" \
  --output-root "$CARDS" \
  --manifest "$CARD_MANIFEST"

print -r -- "[10/10] final non-playback gates and completion marker"
clean cargo +stable fmt --all -- --check
clean git diff --check
clean shasum -a 256 \
  "$G0/report.json" "$G4/report.json" "$AUDITION_REPORT" \
  "$CARD_MANIFEST" \
  fixtures/city/wave17-gamma-audition/fixture.json \
  target/release/fightbox-workbench > "$JOB/SHA256SUMS"
clean du -sh "$JOB" "$G0" "$G4" "$CARDS" fixtures/assets/wave17-audition target/release/fightbox-workbench
print -r -- "COMPLETE wave17_audition_preparation job=$JOB gamma0=$G0 gamma4=$G4 cards=$CARDS"
