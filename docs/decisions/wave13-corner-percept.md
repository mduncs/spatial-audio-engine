# Wave 13 — The corner percept

md's field report, walking the megablock in the workbench: "the attenuation around
corners is unbelievable — three steps around a corner to hearing basically
nothing." The corner diagnostic (`megablock_corner_diagnostic.rs`, 12 listener
positions at 1 m spacing crossing the artillery corner's shadow line) measured the
report exactly: in the state the workbench actually boots into, crossing the
shadow line is a 17 dB step inside one meter (all-stage −55.6 → −72.7 dBFS
between adjacent positions), the reflection stage is −inf at every position, and
the only surviving stem is the baked path at −68 to −72 dBFS. The counterfactual
run — same walk, governor held at its top rung with the source at Full quality —
renders the same crossing as a 1–5 dB slope with the reflection field carrying
about −51 dBFS into the shadow. The corner is not missing physics; it is switched
off by policy. This wave is the policy fix, its measurement gates, and the
observability that would have let us see it from the running session.

The design keeps every frozen surface intact: `backend.rs` untouched, the
library-wide `Raycast` occlusion default untouched (workbench sessions are
already a `Volumetric` opt-in), the §κ degradation order untouched, the safety
limiter untouched.

## 1. Calibration: reference levels must reach the governor

Measured motivation: every workbench source boots at `DirectOnly`, whose stage
targets are `[direct 1.0, path 1.0, reflections 0.0]`
(crates/fightbox-steam-audio/src/multi_source.rs:2047). That zero is the −inf
reflection row in the diagnostic.

The workbench builds descriptors with position, directivity, and extent only
(tools/fightbox-workbench/src/workbench.rs:385-388); it parses the fixture's
`reference_level` and even displays it (workbench.rs:372) but never calls
`with_reference_level` (crates/fightbox-steam-audio/src/lib.rs:956-962). Every
source therefore reports `CreativeDb { db: 0.0 }` (lib.rs:932) and
`is_physically_calibrated() == false` (lib.rs:987-992).

Being honest about what this fixes: calibration alone does not restore
reflections. `degraded_source_quality` returns `DirectOnly` for any audible
source, calibrated or not (governor.rs:1149-1154); the boot snapshot applies it
to every source (governor.rs:415-418). What calibration buys is (a) truthful
audibility ranking, so recovery restores the loudest source first
(`most_audible_degraded_source`, governor.rs:905-907), and (b) the governor's
right to virtualize genuinely inaudible sources instead of carrying them. It is
a prerequisite for sane governor behavior, not the fix itself.

Change: workbench descriptor construction maps the fixture's
`reference_level` (mode `SplAtOneMeter` → `ReferenceLevel::SplAtOneMeter`)
into `with_reference_level`. The CLI/phase-b path already wires descriptor
reference levels (the 2026-07-30 CLI wiring lane); verify rather than change.
Compatibility risk: near zero for captures — stage gains only change when the
governor's delivered state changes, which is the point; the workbench capture
schema records delivered quality truthfully already. The one behavioral edge is
virtualization: a fixture source calibrated below the hearing threshold at its
distance would go `Virtualized` (targets `[0,0,0]`,
multi_source.rs:2048). That is correct behavior, and no current fixture source
is that quiet.

## 2. Governor policy: boot from the measured budget, not from fear

Measured motivation: the boot snapshot is the bottom rung — `Minimum`
reflections (governor.rs:409-414), whose Desktop divisors `(4, 4, i32::MAX,
4.0, 4)` (governor.rs:1168) deliver zero bounces, and `ambisonic_order: 0`,
`validate_paths: false` (governor.rs:419-429). Recovery restores reflection
quality last (`recovery_candidate` order: reverb → ambisonic order → sources →
alternate paths → path validation → reflections, governor.rs:884-924), each
rung behind an 8-evaluation hysteresis. A four-source megablock session climbs
five-plus rungs before the corner fill exists at all — and the per-source
reflection send additionally requires that source to have recovered to `Full`.
Meanwhile the measured cost of the full fixture ask is affordable: one
cinematic + three standard reflection sources convolve at 38.69% median /
53.31% worst of the 2.667 ms render block, with the two simulation passes at
15.4 ms against a 200 ms cadence (docs/diagnostics/reflection-budget-study.md:5,
:150). The governor is defending a 1.33 ms p99 budget (block_period/2,
governor.rs:436) against a load that measurement says fits.

Chosen policy — predicted-cost boot: at construction, compute a predicted
render-block cost for the requested settings from a small measured model (the
budget study's tables give per-source convolution cost as a function of IR
duration and order; the model ships as constants with the study cited as
provenance). Boot at the highest rung whose predicted cost is ≤ 50% of the p99
budget — for the megablock fixture ask this lands at `Reduced` or `Full` with
sources `Full` — and let the existing evidence-driven ladder degrade
immediately if reality disagrees. Degradation remains exactly the frozen §κ
order (EXECUTION.md:1209-1214); nothing about descent changes. First-window
protection: keep the first evaluation window short (the existing 16-block eval
cadence already reacts within ~43 ms at 128-frame blocks), so a wrong
prediction costs at most a few blocks of governed descent, not a dropout spiral
— descent transitions already zero the reflection output gain during the swap
(governor telemetry field `reflection_output_gain`, applied at
multi_source.rs:1299-1311).

Time bound: with boot-at-predicted-fit, the first reflection pass lands within
one reflection cadence (≤ 200 ms at cadence divisor 1) and the corner fill is
audible from the first second of the session. After listener teleports the
motion-triggered staleness is the measured 44–47 ms
(reflection-budget-study.md:163).

Rejected: reordering the recovery ladder to restore reflections first. It
would work, but recovery-as-reverse-of-degradation is the natural reading of
§κ's frozen order, and the predicted-cost boot makes the reorder unnecessary —
recovery from a mispredicted boot is rare rather than the every-session path.
If md later finds the fill too slow to return after real overload, reopening
this is an explicit md-gated amendment, recorded as such here.

Rejected: per-scene manual quality pinning (a fixture "force full" flag). It
hides the governor instead of fixing its prior, and the first fixture that
overruns the budget turns it into a dropout generator.

## 3. The shadow line itself: why it was binary, and what softness is whose job

Measured motivation: raw occlusion went 1.0 → 0.0 in a single 1 m step despite
the 1 m volumetric source sphere (diagnostic rows 5→6).

Diagnosis: Steam Audio's volumetric occlusion samples visibility over a sphere
around the source. The penumbra that sphere paints at the listener scales with
the listener-to-corner and source-to-corner distances: for a listener close to
the occluding edge and a source tens of meters behind it, the zone where the
sphere is partially visible spans centimeters to a few tens of centimeters of
listener travel — under our 1 m sampling it is invisible. The 1 m radius is not
broken; it is geometrically incapable of producing a walkable penumbra for a
distant source, and no defensible radius is (the artillery's extent-driven 3 m
radius, lib.rs:743-751, widens it by 3× — still under a meter of travel).

Policy: the sphere's job is preventing the instantaneous flip for near sources
and extended sources; the believable softness three steps into shadow is the
fill layers' job (reflections restored by §2, path per §4). Accordingly:

- Keep the extent-driven radius mapping (lib.rs:730-766) unchanged.
- Honor the fixture's `occlusion_samples`: the workbench currently hard-codes
  `Volumetric { radius_m: 1.0, sample_count: 32 }` and quietly drops the
  fixture's 64 (tools/fightbox-workbench/src/fixture.rs:225-229). The fixture
  field becomes the sample count (clamped by `max_occlusion_samples`); the
  radius stays the session's 1 m for points. A field that is parsed and
  ignored is worse than either honoring or deleting it; we honor it.
- Rejected: a synthetic distance-into-shadow direct-gain softening (a
  listener-side "diffraction knee"). It is a second gain chain on the direct
  stem, which the one-gain-chain law forbids in spirit and the calibration
  story forbids in practice; direct sound really is blocked; and the
  counterfactual measurement shows a 1–5 dB slope emerges without it once the
  fill layers run. The library `Raycast` default stays untouched.
- The existing 80 ms propagation smoother and its bounded per-block step
  (motion_smoothing.rs; bound test at motion_smoothing.rs:307) remain the
  temporal continuity guarantee at the crossing.

## 4. The path stage: fade the gate, pair the ranges, stay physical

Measured motivation: the path stem survived everywhere on the diagnostic walk
(−68 to −72 dBFS, EQ tilting to [0.626, 0.272, 0.151]) — the hard-zero gate
(multi_source.rs:486-511) did not fire there. But the gate's failure mode is
real: when probe influence is lost at either endpoint it writes exact-zero SH
coefficients with no hysteresis, at the 15 Hz pathing cadence. The 80 ms
smoother slews the resulting send, so the artifact is a fast fade at coverage
edges that can flap probe-by-probe.

- Gate hysteresis: adopt the HYSTERESIS LANE precedent — require N consecutive
  gate misses (N = 3 pathing passes ≈ 200 ms) before publishing the zero
  target, and one hit to restore. The existing smoother remains the only gain
  actuator; no new slew machinery.
- Visibility-range pairing: the runtime ships
  `pathing_visibility_range_m: 6.0` by default (lib.rs:1428), adopted wholesale
  by the workbench (`..S3SimulationConfig::default()`, fixture.rs:238), while
  the megablock bake used 8 m spacing and the repo's own km-sweep sets both
  bake and runtime visibility to 2.5 × spacing (tools/fightbox-cli/src/sweep.rs:1611-1615,
  :1656-1660). Nothing validates the pair; `ProbeBatchMetadata` carries no
  spacing or visibility fields (lib.rs:498-509). Fix: (a) the CLI bake writes
  `spacing_m` and `visibility_range_m` into the bake's evidence sidecar JSON;
  (b) session construction reads the sidecar when present and warns-and-adopts
  `max(configured, 2.5 × spacing)` unless the fixture explicitly sets a new
  optional `pathing.visibility_range_m`; (c) the workbench fixture schema gains
  that optional field. Extending the SDK-owned `ProbeBatchMetadata` struct is
  rejected — it mirrors the serialized SDK object; the sidecar is ours.
- Path level: stays physical — free-field attenuation over path length with the
  SDK's EQ, no deviation-model surcharge (deviation model is null and
  `normalizeEQ` is `IPL_FALSE` today, multi_source.rs:723, :1212). Argument:
  with reflections restored, the fill level is set by simulated physics; adding
  an ad-hoc path attenuation would double-count against a future SDK deviation
  model and turns a physical stage into a tuned one. Revisit only if the §6
  gate measures the shadow too loud with both layers running — which the
  counterfactual says it will not.

## 5. Materials: no masonry transmission in this wave

Motivation to consider it: a real 155 dB artillery piece is audible through a
building — structure-borne low frequency. The synthetic city's brick and
asphalt carry `transmission: [0.0, 0.0, 0.0]`
(crates/fightbox-world/src/material.rs:162-173; assignment at
crates/fightbox-world/src/provider.rs:44-45), so occluded direct is
mathematically zero.

Rejected for this wave. Steam Audio transmission is a surface property applied
to every source equally: coefficients large enough to let artillery thump
through a building would also let Tom's Diner leak through the same wall, which
is the "hearing through walls" corruption the evidence corpus exists to catch.
The corner percept does not need it — fill arrives around the corner, not
through the building. Nonzero masonry LF transmission becomes worth designing
when the interior/open-windows mode arrives, together with a corruption gate
(a quiet source behind a sealed wall must stay inaudible while a 155 dB source
reads as a felt LF thump); recorded as future work, not scoped here. `glass`
already carries honest nonzero transmission (material.rs:186-188) for that
future.

## 6. The percept, quantitatively, and its gates

Target corner envelope, derived from the counterfactual run and ordinary
urban experience: with the source in steady state and the governor at its
delivered rung, walking the diagnostic's 1 m line from LOS to 5.4 m into
shadow —

- Level: every shadow position renders all-stage energy between 3 dB and 15 dB
  below the LOS reference (counterfactual measured 0.7–5.2 dB; pure
  free-field expectation for one corner is nearer 10; the band accepts both and
  rejects the measured cliff's 17–22 dB).
- Continuity: no adjacent-position step greater than 6 dB per meter anywhere on
  the walk (cliff measured 17 dB; counterfactual max ~2 dB), and the existing
  per-block smoothed-step bound stays green.
- Monotony: energy is non-increasing into shadow within a 2 dB tolerance
  (allows the reflection field's local variance, measured ≤ 0.8 dB).
- Timbre: shadow-side spectrum is LF-weighted relative to LOS — band-energy
  ratio (low/high) strictly greater in shadow than at LOS, driven by the path
  EQ tilt and the reflection field; threshold set from the gate's own first
  green run, recorded in the test as a named constant.

Automated gate: promote the diagnostic into
`crates/fightbox-steam-audio/tests/wave13_corner.rs` as an ordinary linked
test: 8 positions instead of 12, single repeat, governor running normally
(no synthetic forcing) with a bounded settle wait asserted via
`quality_governor_telemetry()` — the test also asserts the boot policy itself
(delivered reflections ≥ Reduced and all sources Full within 2 s of
construction on this machine). Estimated runtime ~8–10 s against the diag's
measured 13.9 s. Failure controls, both mandatory: (a) reflections forced off
(the old boot state, reachable through the test seam below) must fail the
level-band and step gates — the startup-bottom table is literally the failure
fixture; (b) an uncalibrated-descriptor session (today's workbench
construction) must fail the boot-policy assertion. A gate that cannot fail is
not evidence.

Test seam: the diagnostic needed 1,779 synthetic timing observations to move
the governor — add a `#[cfg(any(test, feature = "diagnostics"))]` force-quality
seam on the governor for tests and failure controls only. It does not ship in
the production surface.

What only md's ears can sign: the workbench walk itself — artillery and diner
around the megablock corner, after the implementation lands and binaries are
rebuilt: does the corner now sound like a city corner (present, softened,
LF-weighted, continuous), and does LOS restoration feel like emergence rather
than a gate opening.

## 7. Lane split

- W1 governor policy — owns `crates/fightbox-steam-audio/src/governor.rs` and
  its unit tests only: predicted-cost boot model (constants + provenance
  comment), boot-rung selection, test-only force-quality seam, telemetry
  extension (predicted cost, boot rung). Medium (~300–500 lines with tests).
- W2 workbench honesty — owns `tools/fightbox-workbench/**`: descriptor
  `with_reference_level` (workbench.rs:385-388), honor `occlusion_samples`
  (fixture.rs:225-229), optional `pathing.visibility_range_m` field, delete or
  parse the dead `runtime_order` field (delete), and the observability panel:
  surface governor rung / delivered reflections / `reflection_output_gain` /
  per-source quality plus the path diagnostics that
  `acoustic_state.rs:180-186` currently discards (path_sh energy, path_eq) in
  the perf panel (workbench.rs:1173-1181 area). Medium (~300–450 lines).
- W3 path-gate hysteresis + range pairing — owns
  `crates/fightbox-steam-audio/src/multi_source.rs` (gate hysteresis at
  :486-511), session-construction sidecar read + warn-and-adopt, and
  `tools/fightbox-cli` bake-sidecar write (spacing_m, visibility_range_m).
  Small-medium (~200–350 lines). Sequenced after W1 lands if the governor
  telemetry shape moves (both touch fightbox-steam-audio; W1 owns governor.rs,
  W3 owns multi_source.rs — disjoint files, can run parallel).
- W4 corner gate — owns new
  `crates/fightbox-steam-audio/tests/wave13_corner.rs` only; after W1–W3.
  Small (~250 lines).

Papercuts this wave retires: workbench drops reference_level;
`occlusion_samples` silently ignored; perf panel lacks governor telemetry;
`acoustic_state.rs` discards path diagnostics; visRange/bake spacing
unvalidated; `runtime_order` dead field; no governor force-quality test seam.
Left open (recorded, out of scope): no `pathingVisCallback` in live sessions;
private package/bake loader duplication across test modules; Steam reflection
nondeterminism (≤ 0.8 dB) as a permanent gate-tolerance floor.

## md-gated decisions

1. The target envelope band (3–15 dB below LOS, ≤ 6 dB/m step) is a taste
   constant — md ratifies or adjusts it by ear on the workbench walk.
2. Recovery-ladder reordering stays rejected unless post-overload fill return
   proves too slow in practice; reopening it amends §κ's symmetric reading.
3. Masonry LF transmission (hear-through-walls for extreme sources) is
   deferred to the interior/open-windows mode; pulling it forward is md's call.
