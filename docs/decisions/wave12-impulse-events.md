# Wave 12 design: impulse events — ballistic crack sidecar and distance-keyed impulse shaping

Status: design only, no implementation. Scope confirmed 2026-08-01: items 2 and 3 of the
Heat-gap analysis. Item 4 (the safety limiter) is out of scope and untouchable; item 1
(reflection budget) is a separate measurement lane reporting into
`docs/diagnostics/reflection-budget-study.md`.

Both designs obey one rule: the frozen propagation core is never taught about supersonic
motion or nonlinear acoustics. The sidecar computes *when, where, and how loud*; the
shaping stage filters a dry stem *before* the kernel. Everything downstream — delay head,
HRTF, occlusion, air absorption, reflections, the gain chain, the limiter — runs
unmodified.

## Part A — ballistic crack sidecar

### A1. The closed-form model

A bullet leaves the muzzle at `P0`, flying along unit vector `u` at speed `v = M·c`
(`c = 343 m/s`, `M > 1`). The shock it drags is a cone of half-angle `θ_M` with
`sin θ_M = 1/M`. A listener `L` does not hear the cone as geometry; they hear the
wavefront when it sweeps over them, and that wavefront was radiated from one specific
emission point on the trajectory.

Let `d` be the listener's perpendicular (miss) distance from the trajectory line and
`s0` the arc length from the muzzle to the foot of that perpendicular. Sound radiated
when the bullet is at arc length `s` arrives at time `T(s) = s/v + |X(s) − L|/c`. The
first-arrival (envelope) condition `dT/ds = 0` gives the tangent point:

```
s* = s0 − d / sqrt(M² − 1)          (emission point, before the perpendicular foot)
r* = |X(s*) − L| = d · M / sqrt(M² − 1)
T_crack = s*/v + r*/c
```

The line from the emission point to the listener makes angle `φ = arccos(1/M)`
(equivalently `90° − θ_M`) with the flight direction — that is the crack's arrival
direction. The muzzle blast independently arrives at `T_blast = |L − P0|/c` from the
muzzle direction.

Existence conditions (the "no-crack zone"):

- `M ≤ 1` anywhere relevant → no crack at all.
- `s* < 0` → the tangent point is behind the muzzle; the listener sits outside the
  region the cone ever sweeps (too far beside/behind the shooter). No crack.
- `s* > s_end`, where `s_end` is where the bullet impacts or decelerates through
  Mach 1 → the shock was never generated that far downrange. No crack.

Deceleration is handled piecewise: split the trajectory into segments of constant Mach
`M_i` (schema-level table, two or three segments suffice for rifle-class problems),
apply the tangent condition per segment with the segment's local `M_i` and cumulative
flight time, accept the candidate that falls inside its own segment, take the earliest
arrival if more than one qualifies.

**Worked example.** `M = 2.5`, `v = 857.5 m/s`, listener at `d = 30 m` perpendicular,
`s0 = 60 m` downrange. `sqrt(M²−1) = sqrt(5.25) ≈ 2.2913`.

```
s*      = 60 − 30/2.2913            = 46.91 m
r*      = 30 · 2.5/2.2913           = 32.73 m
T_crack = 46.91/857.5 + 32.73/343   = 54.7 ms + 95.4 ms = 150.1 ms
T_blast = sqrt(60² + 30²)/343       = 67.08/343          = 195.6 ms
```

The crack arrives **45.4 ms before** the muzzle blast — the signature snap-then-boom of
near-miss supersonic fire — from `φ = arccos(0.4) ≈ 66.4°` off the flight direction
(the blast arrives from the muzzle, only `atan(30/60) ≈ 26.6°` off the same axis). The
two events are far enough apart in both time and direction that the engine renders them
as what they are: two sources.

Level: weak-shock (Whitham) theory gives N-wave peak overpressure decaying roughly as
`d^(−3/4)` in miss distance with duration growing as `d^(1/4)` — different from the
blast's spherical `1/r`. The sidecar computes the *target received level* from this
model, then back-solves the virtual source's `SplAtOneMeter` so the engine's own
untouched spherical chain delivers it. One physical gain chain, no exceptions — the
ballistic law lives entirely in the sidecar's choice of source power. The reference peak
at reference miss distance is a parameter (documented, class-based), not derived from
first principles; deriving it from muzzle energy is not worth the modeling risk.

### A2. The seam — a scheduled generator source

The engine already has the exact pattern. The firework is a *generator source*: the
fixture declares `"generator": {"kind": "firework_burst", "fixed_seed": ...}`
(fixtures/s-firework/fixture.json:16), the CLI's asset layer validates the
kind/generator contract (tools/fightbox-cli/src/asset.rs:101, :156–168), the signal is
synthesized deterministically by `fightbox_evidence::firework_burst`
(crates/fightbox-evidence/src/signal.rs:235–242), and the result feeds the *normal*
render chain as an ordinary dry stem (crates/fightbox-steam-audio/tests/firework_scene.rs:60–77).
A generator source is just a deterministic dry stem plus a position.

The crack is the same thing with a scheduler in front:

1. A shot event carries shooter pose, trajectory `u`, Mach segment table, and asset
   references (blast asset, crack asset or synth spec).
2. The sidecar evaluates the closed form against the current listener position and, if
   a crack exists, schedules a one-shot **virtual source at the static point `X(s*)`**,
   triggered at emission time `t* = s*/v` after the trigger.
3. Both blast (at `P0`, triggered at 0) and crack (at `X*`, triggered at `t*`) flow
   through the standard per-source path — `render_source` in
   crates/fightbox-steam-audio/src/multi_source.rs:922, dry stem entering as
   `BackendSourceBlock::input_mono` (crates/fightbox-runtime/src/backend.rs:81–84,
   frozen, unchanged) and delayed by the propagation line at multi_source.rs:994.

**What the delay head sees:** a static source with zero published velocity. That
deliberately selects the wave-6 position-only path (multi_source.rs:891–899) — no
Doppler, no slew stress, no supersonic number anywhere in the core. The arrival time is
correct by construction: trigger at `t*` plus the chain's own `r*/c` flight delay equals
`T_crack` exactly. The Mach number exists only in sidecar arithmetic.

The listener moves during the ~150 ms flight, but at walking speed that is under
0.3 m — recompute at trigger time and accept the error; it is far below the direction
and timing JNDs involved. (The virtual source is listener-dependent, which is fine:
this engine is an audience of one by thesis.)

phonon.h was checked for any native ballistic/supersonic/Mach primitive: none exists
(no hits for those terms in
.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h). The sidecar is not
duplicating kernel capability.

### A3. Occlusion correctness for free

Occlusion is per-source simulation state (per-source direct fields,
multi_source.rs:254, smoothed and applied at :1030), raycast from each source's
position. Because the crack's source *is* the tangent point `X*` and the blast's source
is the muzzle `P0`, a listener behind a wall relative to `X*` but exposed to `P0` gets
blast-without-crack, and the converse holds — with zero new occlusion machinery. The
same applies to wave-9 extent occlusion and reflections: the crack participates in the
canyon answer (a real shock reflects off facades too) simply by being a source.

Rejected alternative: rendering the crack as a directional cue glued onto the blast
source. Killed by exactly this — one source position cannot occlude two ways.

### A4. Gate design (all programmatic, linked-SDK harness)

Following the ignored env-parameterized diagnostic pattern
(crates/fightbox-steam-audio/src/multi_source_teleport_tests.rs):

1. **Arrival-time delta:** render blast+crack for the worked-example geometry; onset
   detection on the rendered output must place the crack `45.4 ± 2` ms before the blast
   (the closed form is the oracle, computed in-test).
2. **Direction:** ITD/ILD sign and magnitude of the crack onset window consistent with
   `φ = 66.4°`, distinct from the blast's direction — the S1 ITD-trajectory machinery
   already discriminates azimuth sign.
3. **Level ordering:** three listeners at `d = 10, 30, 90 m` (same `s0`): crack peak
   level strictly decreasing; blast level obeying the existing spherical ordering.
4. **No-crack zone:** listener with `s* < 0`, and a subsonic (`M = 0.9`) shot: crack
   source must never be scheduled (assert at sidecar output, then assert rendered
   output contains exactly one onset).
5. **Determinism:** fixed seed, fixed geometry → bitwise-identical schedule, same
   discipline as `firework_burst_is_deterministic_impulsive_and_finite`
  (crates/fightbox-evidence/src/signal.rs:451).

## Part B — distance-keyed impulse shaping

### B1. The target and the law it must respect

Perceptual target: the thunder curve. Overhead, an impulse is a rip — a pressure edge
with near-instant rise; at kilometers it is a rounded rumble. The physical cause is
nonlinear steepening and subsequent rounding of the shock front, which a linear kernel
cannot produce. What the engine has today is Steam Audio's 3-band air absorption
(default exponential model, phonon.h:3707–3713; coefficients/callback at :3819–3822),
computed per-source and applied through the direct chain
(multi_source.rs:442, applied from smoothed state at :1030;
`apply_air_absorption` flag at crates/fightbox-steam-audio/src/lib.rs:317). Three
coarse bands of gentle EQ round everything uniformly — the *snap-to-thump contrast* is
what goes missing.

The law it composes under: one physical gain chain (`SplAtOneMeter` drives PCM;
invariant 6). The shaper is a *spectral/temporal morph at constant band energy* — it
must redistribute, never add. Concretely: the stage is normalized so its broadband
energy gain is unity (asserted in test), which also guarantees it cannot become a
hidden loudness stage feeding the limiter (item 4 stays untouched in both letter and
spirit).

### B2. Mechanism choice

**(a) N-layer asset crossfade** (near/mid/far recordings, distance-keyed equal-power
fade). Industry standard. Honest assessment: the layers are *different recordings*, so
crossfading them is decorrelated (no comb risk), and continuity holds if keyed off the
smoothed distance. But it is asset-hungry (3+ takes per sound class), untestable in the
strong sense (the "morph law" lives in whatever the recordings happen to be), and — the
decisive point — it needs no engine feature at all: a host can already ship
distance-appropriate assets. Building engine machinery for it would be machinery
without a contract.

**(b) Parametric morph** — single dry asset plus a distance-keyed minimum-phase stage:
a one-pole/biquad low-pass whose cutoff falls with distance (edge rounding), a spectral
tilt, and an attack-softening envelope follower, all driven by the same smoothed
per-source distance that already drives occlusion/air-absorption smoothing
(the 80 ms acoustic smoother, multi_source.rs:952–961), so it cannot zipper. Group
delay is tiny and asserted (the spec's minimum-phase discipline). Deterministic:
same asset, same distance trajectory → same samples, so gates can hold a curve to a
tolerance.

**Recommendation: (b) as the engine feature; (a) remains an asset-pipeline option that
needs nothing from us.** Determinism and testability decide it — the builders cannot
listen, so the morph must be assertable, and only (b) has an assertable law. Rejected:
true nonlinear propagation (waveform steepening in-engine) — killed by the linear
kernel and by the authority note's standing rejection of forking the vendored kernel
(§ι reasoning: we do not fork Steam Audio for per-path phase; the same holds here).
Editing Steam Audio for "special source kinds" is the wrong layer categorically: the
exception belongs in our per-source chain, in front of the kernel.

### B3. Where it lives

Per-source, pre-delay, pre-kernel: in `render_source`
(multi_source.rs:922), operating on the source's dry block before the
`process_sample` delay loop at :994. Order matters conceptually — shaping models what
the *emitted-then-propagated* wave looks like, so it sits with the source, before time
of flight; the frozen `BackendSourceBlock` seam (backend.rs:81–84) is upstream and
untouched.

Opt-in via the API, following the Directivity precedent
(crates/fightbox-api/src/lib.rs:540–549): a new `SourceProfile` field, sketch only:

```rust
pub enum ImpulseClass {
    /// Default: stage bypassed, bit-identical to today.
    None,
    /// Distance-keyed snap-to-thump morph for impulsive sources.
    Impulsive { near_distance_m: f32, far_distance_m: f32, intensity: f32 },
}
```

`None` must render bit-identical to the pre-wave-12 engine (same discipline as
wave-8 directivity's weight-0 identity and wave-9's Point-extent passthrough). Schema
expression follows the extent/directivity pattern when the workbench needs it.

### B4. Gate design

Existing extractors cover most of it (crates/fightbox-evidence/src/ears/extractors.rs):
`spectral_tilt_db` (:81), `spectral_flux` (:82, :237), `click_derivative_z` (:85, :259)
as a transient-sharpness proxy, `schroeder_decay` (:402); plus the firework test's
crest-factor and onset/program-RMS assertions as impulse-metric precedent
(crates/fightbox-steam-audio/tests/firework_scene.rs:22–24).

1. **Morph monotonicity:** one impulse asset rendered at 5, 50, 500 m: crest factor,
   `click_derivative_z`, and high/low tilt must decrease strictly monotonically with
   distance; assert against the parametric curve within tolerance.
2. **Energy identity:** stage output broadband energy equals input energy within
   0.1 dB at every distance (the no-hidden-gain assertion).
3. **Bypass identity:** `ImpulseClass::None` → bitwise-identical render.
4. **Continuity:** an approach walk (existing walk-harness pattern) shows no
   `spectral_flux` spike attributable to the stage — the morph must not step.
5. **ABX-only, marked as such:** whether the chosen curve *feels* like thunder — the
   curve's shape parameters are taste, settled by md's ears, not extractors.

Smallest experiment before implementation: an offline strip — render one dry gunshot
asset through a prototype of the (b) chain at five distances into WAVs md can audition
back-to-back (no engine change; a standalone scratch binary or even a notebook). If the
morph does not read as thunder-like offline, no engine work proceeds.

## Interaction with the other lanes

The Heat percept needs Part A + Part B **and** item 1's reflection budget — the canyon
answer is what makes the blast enormous. This doc deliberately does not size reflection
settings; the measurement lane owns that. A wide source (wave 11) and an impulsive
source are orthogonal classes; no shared machinery is proposed.

Budget note: a crack event transiently consumes one of the eight source slots
(`MAX_ACTIVE_SOURCES`, crates/fightbox-runtime/src/render.rs:16) for the transient plus
tail. The slot lifecycle (allocate on trigger, release after audibility fade, governor
priority during contention) is the one genuinely open design question below.

## Proposed implementation lane split (disjoint ownership)

1. **Ballistics math lane** — new pure-math module (no SDK dependency): cone tangent
   solver, piecewise-Mach segments, N-wave level model, unit gates against the closed
   form (the worked example above becomes a test vector). New files only. Small.
2. **Impulse shaping lane** — `ImpulseClass` on `SourceProfile`
   (crates/fightbox-api/src/lib.rs) + the pre-delay stage and its tests in
   multi_source.rs. Medium. **Sequencing: after wave-11 width implementation lands**,
   since both touch multi_source.rs.
3. **Event/scheduler + workbench lane** — shot-event plumbing (generator-source
   extension `"kind": "ballistic_shot"`), fixture/schema expression, a workbench
   trigger key that fires a scripted shot across the megablock so md hears
   snap-then-boom, direction split, and the no-crack zone by walking. Owns tools/** +
   fixtures/**. Medium.

Workbench audibility milestone: stand near the megablock canyon, trigger a shot passing
30 m to one side — hear the crack lead the blast by ~45 ms from a visibly different
direction, walk behind a building relative to the tangent point and lose only the
crack.

## Open questions

- Crack slot lifecycle under the governor (allocation priority, fade-out rule when all
  eight slots are contended). Needs a small design round with the governor's
  audibility-priority law.
- Crack signal itself: recorded asset vs synthesized N-wave (duration `d^(1/4)` growth
  suggests synthesis; determinism suggests it too — the firework precedent supports a
  `ballistic_crack` generator in fightbox-evidence). Leaning synthesis; settle in the
  offline strip.
- Reference peak level for the N-wave class table (published measurement data to cite
  before picking numbers).
