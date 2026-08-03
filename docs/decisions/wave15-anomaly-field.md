# Wave 15 acoustic-anomaly field feasibility and cost study

**Status:** ratified and implemented as Wave 15 v1 on 2026-08-03. The cheap
proxy field, persistent live trail, and adaptive intersection sampling are in
scope. The full-simulation sweep and bounded validator were explicitly
rejected on cost grounds and are not implemented.

**Decision:** build an 8 m **shadow/weak-path proxy field** as v1, plus a
persistent trail of the live renderer's visited positions. The proxy must not
be labeled a reflection-inversion measurement: it finds places where an
inversion is plausible because direct sound is deeply shadowed and baked path
fill is weak, but it does not predict whether reflections are strong. The
query-only session remains a clean seam where a future experiment could attach,
but v1 does not expose or schedule a full-render sweep.

The selected metric for a later full-simulation field is

```text
reflection_excess_db = 10 log10(E_reflections / E_(direct+path))
```

where `E_(direct+path)` is measured from Direct and Pathing rendered together,
not inferred by adding dB values. Both energies use the selected source's real
decoded program over the same deterministic window. A small declared energy
floor prevents infinities and is stored in the field metadata. Positive values
mean reflected energy exceeds the direct-plus-path bus over that window.

## α. Evidence and map size

The megablock fixture declares a `[0, 0]` to `[585, 585]` metre probe volume.
A cell-centred raster therefore has 147 by 147 = **21,609 cells at 4 m**, or 74
by 74 = **5,476 cells at 8 m**. Eight metres is the natural first resolution:
the supplied `megablock-tall-tx` bake actually uses an 8 m floor lattice plus
16 m elevated layers, even though the fixture's generic probe-volume spacing
field is 4 m.

The supplied bake contains 9,723 probes. `probe-batch.bin` is 465,548,752 bytes
(444 MiB), of which the metadata attributes 422,678,172 bytes to path data.
Its manifest records a 25.49 second, ten-thread bake. The loaded megablock mesh
has 964 vertices and 1,442 triangles.

The prior reflection-inversion diagnostic took about 4.7 wall-clock minutes for
roughly 12 pose rows on this M4 Mac (four performance and six efficiency cores,
16 GiB). That gives the planning observation **282 / 12 = 23.5 seconds per
pose**. The current diagnostic uses the same retained Steam Audio session and
stage renderer as the workbench: for each configuration it runs Direct,
Pathing, and Reflections simulation and then renders three separately gated
buses. Each bus receives 800 settling plus 750 measured 128-frame blocks, or
4.13 seconds of program. This is intentionally conservative evidence, not a
microbenchmark.

Earlier retained-session measurements on the same machine provide the other
cost anchor: direct simulation was about 0.04 microseconds per source and baked
path simulation about 2.3 microseconds per source in the controlled corner
scene. A full 4,096-ray reflection tick at nearby settings costs a few
milliseconds, while the long per-bus render-and-settle windows dominate the
23.5 second pose observation.

## β. Option 1: full-simulation sweep

### Method and fidelity

Create one retained one-source session per worker, move the listener to each
cell at the chosen listener height and yaw, then run the workbench's actual
Direct, Pathing, and Reflections simulation. Render two deterministic windows:
Reflections only, then Direct plus Pathing. This follows
`build_multi_source_session` and `SteamAudioRenderGraph`, including the same
HRTF, direct effect, path effect, reflection convolution, material model, bake,
and stage gains as live playback.

The sweep should deliberately stop before `RuntimeGraph`'s monitor gain and
true-peak limiter. A nonlinear final limiter would make the ratio depend on
monitor volume instead of the acoustic buses. It should use the selected
asset's decoded mono and source calibration, not generic noise, because the
relative result depends on program spectrum. It must pin the delivered quality
and record listener height and orientation; binaural bus energy can change with
orientation.

This is the highest-fidelity option, but it is still a stationary-window test.
It does not model a moving listener's propagation smoothing, recent reflection
history, source motion, or a governor transition unless those are explicitly
part of the sweep protocol.

### Cost

The directly supported planning cost is 23.5 seconds per cell:

| Grid | Cells | Single worker | Ideal 10-way | Conservative 8x realized |
|---:|---:|---:|---:|---:|
| 8 m | 5,476 | 35.7 h | 3.58 h | 4.47 h |
| 4 m | 21,609 | 141.1 h (5.88 d) | 14.1 h | 17.6 h |

The desired ratio needs two gated render windows rather than the diagnostic's
three. If build/simulation overhead is small and the existing settle/measure
lengths remain necessary, a two-thirds estimate is 15.7 seconds per cell:

| Grid | Single worker | Ideal 10-way | Conservative 8x realized |
|---:|---:|---:|---:|
| 8 m | 23.8 h | 2.38 h | 2.98 h |
| 4 m | 94.0 h | 9.40 h | 11.8 h |

That optimization is not evidence until a convergence test shows the shorter
protocol preserves cell ordering and the known inversion. A diagnostic-only
tap of the already-computed stage buffers could measure both energies in one
program pass and approximately halve the baseline again, but it is a new seam.

Ten separate processes would duplicate at least 4.34 GiB of serialized bake
payload before SDK expansion and effect buffers. On this 16 GiB machine,
parallelism is likely memory-limited before all ten cores are useful. Start at
four workers, measure resident memory, and admit more only while the machine
stays out of memory pressure; the 8x column is a CPU upper planning case, not a
promise.

### Storage, display, and invalidation

Store a small JSON manifest beside a row-major binary raster. Four `f32` values
per cell (reflections energy, direct-plus-path energy, excess dB, confidence)
use about 86 KiB at 8 m or 338 KiB at 4 m; a validity bitset and the manifest
are negligible. Do not retain intermediate audio by default.

In the existing workbench, `draw_scene` paints the map into the picture-in-
picture `MAP VIEW`, using `Camera::project`; probe/acoustic badges themselves
currently live in the source rows. A field overlay can add one translucent
`egui::Mesh` of projected cell quads after the city faces and before source,
trajectory, and listener markers. Use a diverging legend centred at 0 dB,
alpha-mask invalid or inaudible cells, and keep markers legible. Build one mesh,
not one widget or draw call per cell.

The cache key must include package mesh and material hashes, bake hash, fixture
hash, source ID/pose/height/directivity/extent/reference level, decoded asset
hash, all simulation and delivered-quality settings, Steam Audio version and
upstream commit, engine commit, HRTF identity, grid bounds/spacing/listener
height/yaw, signal window, energy floor, and metric schema. Any change produces
a new layer; old layers remain viewable only with a visible **STALE** mark.

## γ. Option 2: shadow/weak-path proxy field

### Method and fidelity

For each cell, run a single direct ray from the listener cell to the chosen
source, query the loaded baked path result, and compute the analytic free-field
level:

```text
free_field_db = source_spl_at_1m_db - 20 log10(max(distance_m, source_radius_m))
direct_loss_db = -20 log10(max(direct_audibility, epsilon))
path_strength_db = 10 log10(max(path_sh_energy * mean(path_eq^2), epsilon))
```

The v1 score should require a meaningful unoccluded free-field level, at least
18 dB of direct loss, and weak path strength. Thresholds for path strength and
the final colour ramp should be calibrated against 30–50 full-simulation cells,
including the known `toms-diner above rooves` inversion, clear controls, and
deep shadows with weak reflections. Until that calibration exists, expose the
raw direct loss and path strength alongside the score.

The query-only session should reuse Steam Audio's retained scene and baked
pathing code, so direct audibility and path SH/EQ come from the same simulator
code used by the workbench. It omits HRTF/effects, propagation smoothing,
transmission detail beyond the chosen binary ray, reflection simulation,
reflection convolution, monitor gain, and limiting. It therefore predicts the
**susceptibility signature**, not `reflection_excess_db`. It can miss a
volumetric edge occlusion and can flag a cell where every bus, including
reflections, is negligible.

### Cost

The controlled retained benchmark's direct-plus-path kernel cost is about 2.34
microseconds per pose. At that rate the kernel arithmetic alone is 0.013 seconds
for the 8 m map or 0.051 seconds for the 4 m map. Those figures do not include
moving inputs through the retained wrapper, reading diagnostics, serializing
the raster, or cold loading and deserializing the 444 MiB bake.

A deliberately conservative implementation budget of **0.1–1.0 ms per cell**
allows 0.55–5.5 seconds at 8 m or 2.2–21.6 seconds at 4 m, plus cold session
startup. The v1 acceptance budget should therefore be **under 30 seconds at
8 m and under 60 seconds at 4 m** on this machine. If profiling exceeds that,
the likely problem is repeated construction or I/O, not the per-pose query:
one session must load once and sweep every cell.

This work belongs on one low-priority background worker. More workers duplicate
the large bake for little benefit. It must never share mutable simulator handles
with the live simulation worker.

### Storage, display, and invalidation

Use the same manifest-plus-raster container. Four `f32` planes (direct loss,
path strength, free-field level, score) are about 86 KiB at 8 m and 338 KiB at
4 m. The overlay should use a sequential risk ramp and the explicit legend
**SHADOW + WEAK PATH**, not the full field's reflection-excess legend. Clicking
or hovering a cell should reveal the three contributing values and whether
source/listener probe coverage was valid.

Invalidate on mesh or material hash, bake hash, fixture/source pose and level,
direct-ray policy, pathing order/validation/alternate-path/range settings,
source radius, grid geometry, SDK identity, and score schema. Material changes
invalidate even though the binary ray itself is geometric, because they change
the actual-render interpretation against which this proxy is calibrated.

## δ. Option 3: live incremental trail

There are two very different interpretations of live sampling.

An independent full-simulation shadow session can evaluate arbitrary nearby
poses, but the measured 23.5 seconds per pose sustains only **0.043 poses/s** on
one worker. Even ideal ten-way throughput is 0.43 poses/s, with several GiB of
duplicate bake state and unacceptable competition with live audio. This is not
a viable live feature.

The viable interpretation is to accumulate cells actually visited by the
listener. The Steam backend already computes direct, path, and reflection stage
buffers for live playback. A callback-safe energy tap can accumulate fixed-size
counters into a snapshot; a control/background thread reads a completed window,
attaches the current listener pose and delivered-quality generation, and merges
it into the persistent raster. The callback does no allocation, locks, I/O, or
field simulation.

Fresh reflection state normally arrives at 5 Hz, with motion-triggered updates
capped at 25 Hz. Publishing more field samples than reflection generations only
duplicates the same field state. Cap persistence at **5 fresh poses/s** for v1;
at the 6 m/s workbench autopilot speed that is a 1.2 m trail pitch. A fast-motion
diagnostic mode may accept up to 25 poses/s only after callback timing proves it
harmless. Five 32-byte records per second consume about 0.55 MiB/hour before
compaction into the raster.

This trail has the best fidelity for visited positions: it sees the actual
asset, stage DSP, delivered governor quality, and recent render state. It does
not cover unvisited streets or a neighborhood around a stationary listener,
and its values are window/history dependent. It should be a later validation
layer over the proxy, not the first systematic finder.

Generation records must include the same package, bake, fixture, source, asset,
SDK/HRTF, quality, and metric identities as the full sweep. Never merge samples
across generations. A material, fixture, bake, selected-source, source-height,
or metric-window change closes the current trail and starts a visibly separate
layer.

## ε. Implemented v1

Implement the 8 m proxy first. It covers the entire 585 m map in an operational
wait, directly targets the observed direct-shadow/weak-path signature, occupies
well under 1 MiB on disk, and gives the user places to audition today. No
**Validate candidates** action is planned: even the bounded full-render form
was rejected as too expensive after this study.

Concrete seams and ownership:

1. Add `crates/fightbox-steam-audio/src/anomaly_field.rs`: a control-thread-only,
   query-only retained Direct/Pathing session. It loads scene and baked probes
   once, accepts fixed source plus listener poses, runs no reflections and
   creates no render effects, then returns direct audibility, distance, path
   EQ/SH energy, and probe-validity flags. Do not change
   `fightbox-runtime/src/backend.rs`.
2. Add `tools/fightbox-workbench/src/anomaly_field.rs`: grid specification,
   score calibration constants, low-priority worker, bounded progress channel,
   cancellation on scene rebuild, manifest/cache key, binary raster I/O, and
   stale-layer handling. The worker owns its SDK session; the UI and audio
   threads never touch it.
3. Update `tools/fightbox-workbench/src/workbench.rs`: one compact map-overlay
   toggle/legend, progress text, cell hover details, and a single batched
   projected mesh inside `draw_scene`. Keep source/listener markers above it.
4. Add focused tests: controlled clear/occluded/weak-path geometry, the known
   megablock cell as an environment-gated calibration case, deterministic grid
   ordering and score output, cache invalidation for every provenance family,
   cancellation/scene rebuild, and a measured 8 m/4 m performance report.

Estimate: **5–7 person-days**: 1.5–2 days for the query-only SDK seam and
benchmark, 1–1.5 for worker/storage/invalidation, 1 for overlay interaction,
1–1.5 for controlled and megablock calibration tests, and 0.5–1 for profiling,
polish, and documentation. The full-simulation candidate validator is another
2–3 person-days after v1 because it needs convergence evidence and bounded
multi-session scheduling.

The implementation adds seven stable anomaly-class IDs with tunable threshold
constants, a configurable 2–32 m grid (8 m default), manifest plus row-major
binary output, a batched map mesh, visible stale state, and hover diagnostics.
The live trail reads a wait-free stage-energy snapshot outside the callback and
persists at no more than 5 Hz. A negative obstruction derivative transition
queues eight nearby query-only samples on a separate bounded worker, also
limited to 5 Hz.

The go/no-go gates are: supplied-bake identity verified; the known inversion is
flagged by an exact-position query; 8 m completion under 30 seconds; the golden
direct-render fingerprint remains bit-identical; field identity changes become
visibly stale; and the audio callback performs only fixed-buffer arithmetic and
wait-free snapshot publication. Live deadline behavior while a sweep is active
remains an operator test because this implementation lane was forbidden from
launching or disturbing the running workbench.

## ζ. Static-solid endpoint exclusion

The 2026-08-03 thoroughfare-corner scan exposed 2,695
`zero_path_with_coverage` dots at both the 1.5 m and 63 m Tom's Diner source
heights. This was not a second baked lookup in the query-only session. An
offline retained-render comparison at `[275,280,1.5]`, `[310,281,1.5]`,
`[279,310,1.5]`, and `[305,310,1.5]` produced the same exact-zero SH field and
silent path-only render as the query. Changing the source height selected a
different source probe neighborhood but left every zero in the same listener
cell.

Those cells are inside the provider-generated building prisms. Upward geometry
tests from the four exemplars hit roofs at 9.6 m, 45 m, 59 m, and 48 m. Adjacent
street-side controls such as `[280,280,1.5]` and `[303,281,1.5]` have no static
surface overhead and return nonzero baked paths. The corner scanner deliberately
samples complete squares around each facade corner, including non-walkable
building interiors. A probe influence sphere can extend through a wall into
one of those cells, but that geometric coverage test does not prove that the
endpoint can reach its influencing probe. Steam Audio consequently has no
valid endpoint-to-probe path and returns silence.

`zero_path_with_coverage` now requires both covered endpoints to be outside
static solids. The query session precomputes the non-vertical scene triangles
and rejects the exact-zero class when an upward ray from either endpoint meets
a static surface. This rule is intentionally limited to the exact-zero
coverage class; it does not rewrite the raw direct/path measurements or hide
the real facade discontinuity from the other classes.

The boundary remains audible-relevant on its outdoor side: direct visibility
and path fill can change sharply as a listener rounds a facade corner. The
interior side is not an auditionable listener pose, so its exact-zero result is
not evidence of a bake hole or an engine failure. The rule changes anomaly
classification only. It does not change simulation, rendering, smoothing,
stage gain, or limiter behavior. The workbench classifier cache key advances
to schema 2 so a pre-fix field or adaptive trail is not restored under the new
rule.

Same-configuration 190,260-dot rescans reduced
`zero_path_with_coverage` from 2,695 to 0 at each source height. Direct/path
samples and every other class count were unchanged; only the invalid endpoint
interpretation was removed.
