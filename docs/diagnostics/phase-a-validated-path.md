# Phase A validated-path diagnosis

Date: 2026-07-29

## Status

Resolved by ADR 0003. The invalid concave fixture described below was replaced
by the exact real-SDK-proven convex exterior corner. The corrected canonical
fixture now produces a validated nonzero path moment within its analytic
azimuth tolerance. This resolves the mechanical fixture blocker only. No
listening judgment has been recorded, and S3 still requires its captured
evidence and completed provisional listening record.

All linked measurements used the verified Steam Audio 4.8.1 SDK and its pinned
`0da1825` source. The wrapper sequence matches the official integration test:
commit the scene and probe batch, bake and serialize path data, load and commit
a fresh probe batch, bind it to a fresh simulator, commit a source, set source
and shared inputs, then run pathing.

## Legacy exact-fixture evidence

The legacy ten-triangle mesh had wall arms from the origin west to
`x = -10` and south to `y = -10`. Its source is in the concave southwest
region at `(-6, -4, 1.5)` and its listener is northeast at `(4, 6, 1.5)`.
A physical route must go around one remote wall end. The floor and uniform-floor
probe coverage stop at `x = -9` and `y = -9`, so neither route exists in the
probe graph.

With the legacy exact one-metre grid, point visibility, six-metre visibility range,
100-metre path range, validation enabled, and alternate lookup enabled:

- bake: 317 probes, 577,104 path-data bytes, 0.052 seconds;
- runtime: 0.031 seconds, all SH coefficients zero and no direction;
- validation trace: 15 graph segments considered, five rejected;
- representative rejected edge: `(-3, -1.0000005, 1.5)` to
  `(-3, 0, 1.5)`, ending on the masonry surface.

The bounded matrix was conclusive:

| Case | Probes | Bake / runtime visibility | Bake s | Render s | Result |
|---|---:|---|---:|---:|---|
| Legacy exact | 317 | 1 point / 1 point, 6 m | 0.052 | 0.031 | zero SH; 15 / 5 traced / rejected |
| Unvalidated control | 317 | 1 point / 1 point, 6 m | 0.051 | 0.030 | azimuth 225.002°, 11.312° analytic delta |
| Validation, alternates off | 317 | 1 point / 1 point, 6 m | 0.051 | 0.029 | same nonzero rejected path; 15 / 5 |
| Matched volumetric | 317 | 4 samples, 0.5 m radius, 0.1 threshold, 6 m | 0.053 | 0.031 | zero SH; 16 / 5 |
| 100 m visibility | 317 | 1 point / 1 point, 100 m | 0.058 | 0.029 | zero SH; 11 / 5 |
| 0.8 m spacing | 476 | 1 point / 1 point, 6 m | 0.128 | 0.062 | zero SH; 12 / 4 |
| 1.2 m spacing | 210 | 1 point / 1 point, 6 m | 0.021 | 0.015 | zero SH; 6 / 2 |
| Endpoints swapped | 317 | 1 point / 1 point, 6 m | 0.051 | 0.030 | zero SH; 15 / 5 |
| Symmetric 0.25 m inset | 288 | 1 point / 1 point, 6 m | 0.025 | 0.017 | zero SH; no graph path |

The generated probe influence radius equals probe spacing. At the legacy
one-metre alignment, both endpoints are exactly on probe centers and adjacent
influence-sphere boundaries. Moving probes off the walls does not create the
missing physical route. One asymmetric floating-point alignment
(`min += 0.001 m`, `max -= 0.0125 m` horizontally) happened to pass through the
shared `(0, 0)` endpoint and emitted 214.064° with no rejected callback
segments. Nearby alignments did not. This corner leak is deliberately not
encoded as an acceptance configuration.

The pinned source explains three misleading controls:

- `path_data.cpp` bakes a directed visibility result once and stores the graph
  edge symmetrically. `path_visibility.cpp` can test that edge in the reverse
  direction at runtime, exposing surface-endpoint asymmetry.
- `path_simulator.cpp` retains the rejected baked sound path when validation is
  enabled but alternate lookup is disabled. That output is not a validated
  path.
- When no path survives, `calcEQForPaths` leaves EQ at `[1, 1, 1]`; the zero SH
  array is the decisive no-path signal.

## Accepted correction

ADR 0003 accepts the proven convex-building-corner correction:

- double-sided six-metre façades from `(0, 0)` east to `(10, 0)` and north to
  `(0, 10)`, plus a floor spanning `[-9, 9]` on both horizontal axes;
- source `(-4, 6, 1.5)`, west of the north façade;
- listener `(6, -4, 1.5)`, south of the east façade;
- probe bounds `(-8.75, -8.75, 0.5)` to `(8.25, 8.25, 2.5)`, one-metre
  spacing and 1.5-metre height.

The line of sight crosses both façades. On the real SDK the executable
regression produced 324 probes, direct occlusion `0`, nonzero SH0
`0.019228581`, 11 validation segments with none rejected, and decoded azimuth
`299.36618°`. The analytic listener-to-corner azimuth is `303.69006°`, a
`4.32388°` delta inside the 15° limit. The focused linked test completed in
0.07 seconds.

The canonical fixture, strict semantic validator, and linked regression now
encode those exact values. The legacy matrix remains in the backend under
explicit `legacy_concave_*` names so the original failure cannot be mistaken
for current-fixture evidence.

## Backend correction and reproduction

`IPLSimulationSettings.maxOrder` now reserves
`max(reflection_order, pathing_order)`. Pinned 4.8.1 uses this one capacity for
both reflection and path SH storage; reserving only the reflection order could
under-allocate whenever pathing used a higher order.

The SDK callback trace is opt-in through
`S3SimulationConfig::trace_path_validation` and is cleared immediately after
the blocking path run. Reproduce the focused cases and full bounded matrix with:

```sh
export STEAM_AUDIO_SDK_DIR="$PWD/.cache/steam-audio/steamaudio-4.8.1/steamaudio"
rustup run stable cargo test -p fightbox-steam-audio --features linked-sdk \
  legacy_concave_s3_corner_exposes_validation_limit_and_unvalidated_path_moment
rustup run stable cargo test -p fightbox-steam-audio --features linked-sdk \
  accepted_s3_convex_fixture_validates_nonzero_path_within_analytic_tolerance \
  -- --nocapture
rustup run stable cargo test -p fightbox-steam-audio --features linked-sdk \
  legacy_concave_s3_validated_path_diagnostic_matrix -- --ignored --nocapture
```
