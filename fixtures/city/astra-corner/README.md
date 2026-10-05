# One repeatable corner

First Astra listening-reproduction slice. One fixed Tom’s Diner source, one
18-second northbound listener route, then a two-second endpoint hold. Normal
Workbench startup keeps the source off. Explicit headless replay enables it,
starts the same live convolution graph, and opens only the requested device.

This is **baseline instrumentation, not an acoustic fix**. The route uses the
existing megablock package/bake and the corner already identified by the
megablock stage-energy diagnostic. No map rebuild is needed.

## Background run

From the canonical repository root, build once (no playback):

```sh
CARGO_BUILD_JOBS=2 STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1/steamaudio" \
  cargo build --release -p fightbox-workbench --features linked-sdk,live-output
```

After authorizing BlackHole playback, use a **fresh absolute** evidence directory:

```sh
python3 scripts/run-corner-replay.py --start-audio \
  --output /absolute/outside-repository/corner-run --seconds 20 --repeat 2
```

This wrapper selects exactly `BlackHole 2ch`, records its input with ffmpeg, never
changes default devices, launches no window, and stops only its own child
processes. It requires ffmpeg and the local private music asset. Its `run.json`
records process completion, **not** agreement between capture and loopback.

The equivalent Workbench CLI is `--headless-replay --seconds 20 --capture-root
<absolute outside-repository path> --start-audio --device 'BlackHole 2ch'` plus
the existing `--package`, `--baked`, and this `--fixture` path. Headless mode
requires one source (static or authored moving trajectory) and a valid listener trajectory. Saved mixes are
ignored; source and master trims are 0 dB, stages retain normal defaults. The
source starts at 45 seconds into its declared recording on every run.

## Inspect without playing sound

Each capture bundle contains `capture.wav`, `manifest.json`, and `replay.json`.
The latter records block-indexed listener commands, publication brackets,
occlusion/path observations and existing direct+path/reflection energy. It does
not claim sample-exact asynchronous simulation adoption or identical stochastic
reflection output between runs.

```sh
python3 tools/analyze-corner-replay.py \
  --replay /absolute/bundle/replay.json \
  --capture /absolute/bundle/capture.wav \
  --loopback /absolute/run/blackhole.wav \
  --fixture fixtures/city/astra-corner/fixture.json \
  --geojson ../spatial-audio-engine-runs/megablock-seed1/megablock.geojson \
  --output /absolute/new-report-directory
```

The generated `report.html` is self-contained and silent: no audio element,
WebAudio, autoplay, server, or network dependency. Scrub or play the visual
timeline, mark a change in ordinary words, and export notes. Notes bind time,
position and exact capture/replay hashes; they are not saved until exported.
This is a report prototype, not yet a new Workbench map mode.

Waves use an illustrative 2D visibility/corner-distance sketch over the matching
building footprints. Their direction around geometry is suggestive, not Steam
Audio ray/path evidence, a simulated reflection field, or physical wave speed.
Only listener position and the plotted observations come from the recorded run.

When a loopback is supplied, analysis fits one constant time offset, without
resampling or time warping. A relative RMS discrepancy above 1% returns exit 2
after retaining the failed report. This is an integrity check, not a perceptual
threshold. Audio traces include the source program’s own dynamics; the 250 Hz
first-order lowpass is a descriptive bass trace, not source-normalized transfer.

## First-run state, September 4, 2026

Two repaired 20-second Workbench runs finished and held the route endpoint;
the internal output contains finite audio. A shutdown defect in the new replay
path was found and repaired: destroying the unique callback capture endpoint
now releases its producer-active state before finalization.

BlackHole input returned silence with both AVFoundation and an independent
native HAL recorder, including an independent tone probe. Normal mute/volume,
capture authorization and microphone-mode readings did not explain it. Device
loopback is **unverified**; no system-device reset, driver installation, default
output change, or headphone fallback was attempted. Failed probes are retained.

Evidence: `../spatial-audio-engine-runs/astra-corner-20260904/`.

## What we test next, rather than ask the listener to design

The previous conversations already supply the questions. This crossing is the
first reproduction, not the entire suite:

| Observation | Controlled next comparison |
| --- | --- |
| Sound vanishes around an edge | Same route and source phase; separate direct/path/reflection contribution around the marked interval |
| Bass/body disappears | Source-normalized or steady broadband control alongside familiar music; distinguish spectral loss from simple level loss |
| Artillery lacks scale or tail | One onset, fixed near/shadow/far positions, uncut decay and a common gain reference |
| Vehicles jump or muffle abruptly | Same moving trajectory at walking, vehicle and aircraft speeds; inspect delay/pitch and indirect continuity separately |
| Sources sound physically misplaced | Fixed orientation/position A/B; compare direction, width and raw source material |
| Geometry and audio disagree | Interior/exterior/roofline poses against the actual compiled mesh and probe coverage |
| Streaming reveals a boundary | Repeat a bounded handoff with an already-playing event and preserved tail |
| UI or route says audio works when it does not | Internal capture plus verified device loopback; no silence or process exit relabelled as a pass |

First establish a trustworthy reproduction. Then isolate the responsible stage,
change one thing, and keep a before/after pair. Add a regression only for a
demonstrated failure. Human judgment can be a short note attached to a moment;
it does not require acoustic jargon or completion of a large worksheet.

## Eight-bounce candidate

`candidate-eight-bounce.json` preserves the baseline route, music phase, source
level, rays and IR duration, changing only the reflection bounce limit from
three to eight (plus identity and explanatory metadata). It remains default-off.
Use it as the Workbench `--fixture` with the same package/bake; the wrapper still
runs the unchanged baseline.

Fresh-session shadow controls reproduced weak reflections at full quality.
Quadrupling rays with three bounces did not restore them; eight bounces did, in
two independent runs. This is a scene-specific fidelity candidate, not a global
quality change or human realism pass. Replay traces now include source quality
and delivered governor reflection settings; unavailable quality is null.
Evidence: `../spatial-audio-engine-runs/astra-corner-20260904/controlled/`.
