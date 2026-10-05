# Wave 17 focused γ0–γ10 Squad tasting

This is the user-requested consolidated desktop listening scene. It exposes every
still-unlistened γ0–γ10 feature in one Workbench fixture. **All 16 sources are
`default_enabled: false` and `restart_on_enable: true`.** Enabling a source starts
its program at the declared phase; disabling rewinds it. No source is intended to
play automatically.

The fixture reuses the retained 585 m megablock package/bake and positions local
audition proxies on the open central crossroads around `[292.5, 292.5, 1.5]` m.
There is no new dense 10 km world or probe bake. γ0’s 100 m/1 km/10 km physical
macro clocks are pre-rendered through the public planner/scheduler, then auditioned
through nearby local proxies. This avoids falsely stretching the ordinary 2,048 m
local propagation ring to 10 km.

## Sources

- `gamma0-100m-macro-ingress` → `wave17-gamma0-transport-100m` — γ0 100 m transport pulse
- `gamma0-1km-macro-ingress` → `wave17-gamma0-transport-1km` — γ0 1 km transport pulse
- `gamma0-10km-macro-ingress` → `wave17-gamma0-transport-10km` — γ0 10 km transport pulse
- `gamma1-artillery-200m` → `wave17-gamma1-artillery-200m` — γ1 artillery 200 m distance morph
- `gamma2-elevated-firework` → `wave17-gamma2-firework` — γ2 elevated firework
- `gamma3-snap-then-boom` → `wave17-gamma3-snap-boom` — γ3 supersonic snap then boom
- `gamma4-thunder-pressure-edge` → `squad-thunder-distant-03` — γ4 thunder candidate 03 pressure edge
- `gamma4-thunder-long-tail` → `squad-thunder-distant-15` — γ4 thunder candidate 15 long weather tail
- `gamma5-a10-fast-mover` → `squad-a10-pass` — γ5 authored A-10 pass through a static 125 m local proxy (motion already in the recording)
- `gamma6-m2-contention` → `squad-m2-burst-loop` — γ6 Squad M2 contention texture (not the retained four-source contention proof)
- `gamma7-owner-home-aperture` → `wave17-gamma7-owner-home` — γ7 owner-home aperture states
- `gamma8-toms-diner-walk` → `wave17-gamma8-toms-diner-walk` — γ8 Tom’s Diner city walk
- `gamma9-neutral-ab` → `wave17-gamma9-neutral` — γ9 neutral A/B
- `gamma9-combined-once-ab` → `wave17-gamma9-combined-once` — γ9 composed-once A/B
- `gamma10-monolithic-oracle-ab` → `wave17-gamma10-oracle` — γ10 monolithic oracle A/B
- `gamma10-production-cell-e1-n0-ab` → `wave17-gamma10-cell-e1-n0` — γ10 production cell e1-n0 A/B

The γ4, γ9, and γ10 pairs are sequential A/B sources: enable only one row at a time. The focused UI separates the local 585 m ENU scene from macro transport context, reports the exact selected CPAL output device, defaults audition monitor gain to 0 dB, and moves generic capture/safety diagnostics behind progressive disclosure. Chicago Loop is contextual naming only: the megablock has no geodetic anchor and is not a surveyed or visually faithful Chicago reconstruction.
`listening-template.json` pins the current safe binary, fixture, report, source
order, route fields, and eleven still-unconsolidated card prompts. It records the explicit audio authorization and selected-route fields but keeps every listener judgment blank/pending until the named listener supplies it.
γ4 candidates 03 and 15, the γ5 A-10 pass, and the γ6 M2 texture are private mono preparations from the user’s
local Squad v8.1 assets. Candidate 03 is retained as a pressure-edge alternate;
candidate 15 is the proposed longer weather-tail sound. Their hashes establish
identity only and grant no redistribution rights.

## Preparation (no playback)

The one-shot command `./scripts/prepare-wave17-audition.sh` was authorized and run
for the retained `20260812T003140Z` preparation. It runs ten labeled gates, writes
a progress log and completion marker on Jupiter, builds the binary, and never
launches the Workbench or opens an audio device. Do not rerun it blindly now: the
tracked descriptors and private prepared media intentionally already exist.

For reference, its media preparation stages are:

```sh
python3 tools/prepare-squad-assets.py \
  --asset squad-thunder-distant-03 \
  --asset squad-thunder-distant-15 \
  --report /path/to/spatial-audio/evidence/wave17-gamma4-thunder-preparation-<UTC>.json

python3 tools/qualify-gamma4-thunder.py \
  --preparation-report /path/to/spatial-audio/evidence/wave17-gamma4-thunder-preparation-<UTC>.json \
  --output /path/to/spatial-audio/evidence/wave17-gamma4-thunder-stems-<UTC>

python3 tools/prepare-wave17-audition-assets.py \
  --gamma0-root /path/to/spatial-audio/evidence/wave17-gamma0-transport-pulse-<UTC> \
  --report /path/to/spatial-audio/evidence/wave17-audition-assets-<UTC>.json
```

Prepared WAVs live under gitignored `fixtures/assets/squad/` and
`fixtures/assets/wave17-audition/`; tracked descriptors pin the local bytes.
Exact retained stereo/mono source artifacts remain evidence authority. Downmixes and the common 105 dB `SplAtOneMeter` declarations are private
monitoring adaptations, not physical source measurements or delivered-ear SPL.

## Build and launch boundary

Do not build silently. Before compiling, report the exact command and expected
CPU/duration/storage to the user. No new package/bake is required; the intended
incremental build is only the Workbench binary. After it is built, the dedicated
launcher is:

```sh
./run-workbench-wave17-audition.command
```

Launching that command is device-free and cannot start audio because it deliberately
omits `--start-audio`. After the human pause separately authorizes playback, the
same binary must be invoked with explicit `--start-audio` (and optionally an exact
`--device`); default-off restartable sources still remain silent until individually
enabled. The currently authorized default-route session resolves as `External Headphones`; name the listener and record exact per-card judgments. Finite descriptors now play once and re-arm after disable/re-enable, while looping descriptors remain loops. The γ10
oracle/cell A/B is useful coloration comparison but does not itself reproduce the
full four-world live crossfade callback.
