# Reflection budget study

**Status:** measurement and design only. No production implementation is included.

**Headline:** this Mac can render one 3 s, second-order cinematic reflection source plus three 1 s, first-order standard sources inside the 128-frame deadline. The measured reflection branch used 1.032 ms median and 1.422 ms worst, or 38.69% and 53.31% of the 2.667 ms block. The corresponding two simulation passes cost 15.42 ms median and 19.18 ms when their observed maxima are summed, comfortably below the 200 ms periodic reflection interval. Render convolution, not the simulation tick, is the first scaling limit.

There is a more important perceptual result. A 3 s IR allocates and processes the expected 2.331 s bin, while a 1 s IR truncates it exactly. Steam Audio 4.8.1's realtime reflection model did **not** produce a usable return from the controlled 400 m facade, though: the 3 s result was at the effect's roughly `1e-9` numerical floor even at 131,072 rays. The same test correctly placed a 100 m facade at 0.589 s with a `2.86e-4` peak. Therefore 3 s IR support is necessary for the desired downtown slapback, but the duration knob alone does not buy the flagship percept. Long-distance energy/gain behavior needs its own design and verification before implementation.

## α. Machine and method

Measurements were taken on 2026-08-01 from source revision `c87d78b` on a Mac mini (Mac16,10), Apple M4 with 4 performance and 6 efficiency cores, 16 GB memory, macOS 26.5.2, Rust 1.91.1, and Steam Audio 4.8.1. Tests were optimized release tests linked to the locally acquired SDK. Audio was 48 kHz with 128-frame blocks, so the callback must sustain 375 blocks/s and each block has 2.667 ms.

All reflection simulation used one Steam Audio simulation thread, 32 diffuse samples, and a ray batch size of 64. The controlled canyon contains double-sided masonry ground, parallel canyon walls, a rear wall, and a facade at 400 m; its geometry extends to 500 m and has 20 triangles. Each matrix row used one warm-up tick and nine measured ticks. The harness verified returned IR channel count and sample count on every row.

The simulation timer covers setting shared and per-source inputs, `iplSimulatorRunReflections`, and reading one source output. It reuses a max-capacity simulator so the matrix measures steady-state ticks rather than construction. The retained wrapper's small Rust validation/publication step is outside this matrix timer; the worker staleness measurement later in this report is end to end.

The convolution timer exercises the engine's actual reflection-side sequence: one `IPLReflectionEffect` call per source into the shared mixer, one mixer apply, one Ambisonic binaural decode, and output interleave. Each row has 96 warm-up blocks and 1,000 individually timed nonzero blocks. It excludes direct/path DSP, final stereo accumulation, the audio host, and unrelated application work. Consequently the reported deadline fractions are the reflection branch's budget consumption, not whole-callback utilization.

The diagnostic source is [`reflection_budget_diagnostics.rs`](../../crates/fightbox-steam-audio/src/reflection_budget_diagnostics.rs). Run it with:

```sh
export STEAM_AUDIO_SDK_DIR=/Users/md/code/spatial-audio-engine/.cache/steam-audio/steamaudio-4.8.1/steamaudio
export FIGHTBOX_DIAG_REPEATS=9
cargo test --release -p fightbox-steam-audio --features linked-sdk reflection_budget_ -- --ignored --nocapture --test-threads=1
```

## β. Reflection simulation matrix

Times are milliseconds. `max` is the largest of nine measured steady-state ticks, not a percentile. Duration maps to 48,000, 96,000, or 144,000 samples per channel; order 1 has 4 channels and order 2 has 9.

### 2,048 rays

| Bounces | Duration | Order | Channels | Median ms | Max ms |
|---:|---:|---:|---:|---:|---:|
| 4 | 1 s | 1 | 4 | 1.734 | 1.920 |
| 4 | 1 s | 2 | 9 | 2.466 | 2.552 |
| 4 | 2 s | 1 | 4 | 2.512 | 2.622 |
| 4 | 2 s | 2 | 9 | 4.345 | 4.410 |
| 4 | 3 s | 1 | 4 | 3.131 | 3.349 |
| 4 | 3 s | 2 | 9 | 6.081 | 6.186 |
| 8 | 1 s | 1 | 4 | 1.845 | 1.864 |
| 8 | 1 s | 2 | 9 | 2.700 | 2.782 |
| 8 | 2 s | 1 | 4 | 2.783 | 2.844 |
| 8 | 2 s | 2 | 9 | 4.562 | 4.743 |
| 8 | 3 s | 1 | 4 | 3.571 | 3.681 |
| 8 | 3 s | 2 | 9 | 6.092 | 6.236 |
| 16 | 1 s | 1 | 4 | 1.818 | 1.892 |
| 16 | 1 s | 2 | 9 | 2.683 | 2.732 |
| 16 | 2 s | 1 | 4 | 2.716 | 2.783 |
| 16 | 2 s | 2 | 9 | 4.648 | 4.900 |
| 16 | 3 s | 1 | 4 | 3.792 | 3.929 |
| 16 | 3 s | 2 | 9 | 6.097 | 6.327 |

### 4,096 rays

| Bounces | Duration | Order | Channels | Median ms | Max ms |
|---:|---:|---:|---:|---:|---:|
| 4 | 1 s | 1 | 4 | 2.742 | 2.972 |
| 4 | 1 s | 2 | 9 | 3.606 | 3.779 |
| 4 | 2 s | 1 | 4 | 3.563 | 3.647 |
| 4 | 2 s | 2 | 9 | 5.541 | 5.692 |
| 4 | 3 s | 1 | 4 | 4.145 | 4.279 |
| 4 | 3 s | 2 | 9 | 6.787 | 7.008 |
| 8 | 1 s | 1 | 4 | 3.391 | 3.442 |
| 8 | 1 s | 2 | 9 | 4.236 | 4.311 |
| 8 | 2 s | 1 | 4 | 4.433 | 4.665 |
| 8 | 2 s | 2 | 9 | 6.026 | 6.306 |
| 8 | 3 s | 1 | 4 | 4.955 | 5.082 |
| 8 | 3 s | 2 | 9 | 7.600 | 7.746 |
| 16 | 1 s | 1 | 4 | 3.338 | 3.393 |
| 16 | 1 s | 2 | 9 | 4.333 | 4.410 |
| 16 | 2 s | 1 | 4 | 4.560 | 4.943 |
| 16 | 2 s | 2 | 9 | 6.656 | 6.768 |
| 16 | 3 s | 1 | 4 | 5.665 | 5.769 |
| 16 | 3 s | 2 | 9 | 8.198 | 8.348 |

### 8,192 rays

| Bounces | Duration | Order | Channels | Median ms | Max ms |
|---:|---:|---:|---:|---:|---:|
| 4 | 1 s | 1 | 4 | 4.918 | 5.221 |
| 4 | 1 s | 2 | 9 | 5.783 | 6.063 |
| 4 | 2 s | 1 | 4 | 5.711 | 6.074 |
| 4 | 2 s | 2 | 9 | 7.242 | 7.402 |
| 4 | 3 s | 1 | 4 | 6.355 | 6.623 |
| 4 | 3 s | 2 | 9 | 8.876 | 9.718 |
| 8 | 1 s | 1 | 4 | 6.378 | 6.499 |
| 8 | 1 s | 2 | 9 | 7.209 | 7.458 |
| 8 | 2 s | 1 | 4 | 7.070 | 7.602 |
| 8 | 2 s | 2 | 9 | 9.314 | 9.951 |
| 8 | 3 s | 1 | 4 | 8.185 | 8.876 |
| 8 | 3 s | 2 | 9 | 11.190 | 11.690 |
| 16 | 1 s | 1 | 4 | 6.245 | 6.614 |
| 16 | 1 s | 2 | 9 | 7.154 | 7.550 |
| 16 | 2 s | 1 | 4 | 8.296 | 8.795 |
| 16 | 2 s | 2 | 9 | 9.764 | 10.785 |
| 16 | 3 s | 1 | 4 | 9.226 | 9.664 |
| 16 | 3 s | 2 | 9 | 11.789 | 11.993 |

### 16,384 rays

| Bounces | Duration | Order | Channels | Median ms | Max ms |
|---:|---:|---:|---:|---:|---:|
| 4 | 1 s | 1 | 4 | 9.171 | 9.616 |
| 4 | 1 s | 2 | 9 | 10.152 | 10.708 |
| 4 | 2 s | 1 | 4 | 10.266 | 10.528 |
| 4 | 2 s | 2 | 9 | 11.640 | 12.515 |
| 4 | 3 s | 1 | 4 | 10.453 | 10.755 |
| 4 | 3 s | 2 | 9 | 13.230 | 14.006 |
| 8 | 1 s | 1 | 4 | 11.610 | 12.038 |
| 8 | 1 s | 2 | 9 | 12.583 | 12.708 |
| 8 | 2 s | 1 | 4 | 13.636 | 13.926 |
| 8 | 2 s | 2 | 9 | 15.411 | 15.745 |
| 8 | 3 s | 1 | 4 | 14.018 | 14.749 |
| 8 | 3 s | 2 | 9 | 16.938 | 17.780 |
| 16 | 1 s | 1 | 4 | 11.652 | 11.817 |
| 16 | 1 s | 2 | 9 | 12.778 | 13.011 |
| 16 | 2 s | 1 | 4 | 14.953 | 15.276 |
| 16 | 2 s | 2 | 9 | 16.506 | 16.756 |
| 16 | 3 s | 1 | 4 | 16.191 | 16.747 |
| 16 | 3 s | 2 | 9 | 19.141 | 20.294 |

Ray count is the main simulation cost. Duration and order also matter because the simulator must produce a larger, more directional IR. Additional bounces had modest cost in this open canyon once rays escaped the geometry. The most expensive row still completes in about one tenth of the ordinary 200 ms reflection period, and in about half of the 40 ms motion-trigger eligibility interval.

## γ. Render convolution matrix

`Deadline` is the fraction of the 2.667 ms block consumed. `Headroom` is measured blocks/s divided by the required 375 blocks/s.

| IR | Order | Ch | Sources | Median µs | Max µs | Median deadline | Max deadline | Blocks/s | Headroom |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 s | 1 | 4 | 1 | 71.625 | 173.750 | 2.69% | 6.52% | 13,962 | 37.23× |
| 1 s | 1 | 4 | 4 | 260.750 | 476.000 | 9.78% | 17.85% | 3,835 | 10.23× |
| 1 s | 2 | 9 | 1 | 155.708 | 301.167 | 5.84% | 11.29% | 6,422 | 17.13× |
| 1 s | 2 | 9 | 4 | 598.916 | 1,268.334 | 22.46% | 47.56% | 1,670 | 4.45× |
| 2 s | 1 | 4 | 1 | 134.917 | 265.750 | 5.06% | 9.97% | 7,412 | 19.77× |
| 2 s | 1 | 4 | 4 | 523.875 | 746.458 | 19.65% | 27.99% | 1,909 | 5.09× |
| 2 s | 2 | 9 | 1 | 301.791 | 459.458 | 11.32% | 17.23% | 3,314 | 8.84× |
| 2 s | 2 | 9 | 4 | 1,180.083 | 1,421.375 | 44.25% | 53.30% | 847 | 2.26× |
| 3 s | 1 | 4 | 1 | 201.083 | 348.041 | 7.54% | 13.05% | 4,973 | 13.26× |
| 3 s | 1 | 4 | 4 | 783.375 | 1,105.292 | 29.38% | 41.45% | 1,277 | 3.40× |
| 3 s | 2 | 9 | 1 | 446.959 | 604.291 | 16.76% | 22.66% | 2,237 | 5.97× |
| 3 s | 2 | 9 | 4 | 1,761.250 | 2,148.833 | 66.05% | 80.58% | 568 | 1.51× |

Duration is close to linear in the render path. Moving from order 1 to order 2 changes 4 channels to 9, and measured cost rises by roughly 2.2×. Four 3 s/order-2 sources technically remain under the isolated reflection deadline, but 19.42% worst-case margin is not enough for the rest of a real callback. A later full diagnostic verification run reached 2.221 ms, or 83.29% of the deadline, leaving only 16.71%. This is where the design must stop treating all sources equally.

One diagnostic used selective per-source reflection flags and two shared-quality passes to model the intended workload:

| Workload | Simulation median | Simulation observed-max sum | Render median | Render max | Median deadline | Max deadline | Render headroom |
|---|---:|---:|---:|---:|---:|---:|---:|
| 1 cinematic (`8192/8/3 s/o2`) + 3 standard (`2048/4/1 s/o1`) | 15.421 ms | 19.178 ms | 1,031.792 µs | 1,421.500 µs | 38.69% | 53.31% | 2.58× |

The render figures leave 1.635 ms median and 1.245 ms at the observed reflection maximum for direct sound, pathing, graph overhead, the host, and scheduling noise. The four per-source effects and mixer were constructed at the cinematic maximum, matching the capacity behavior a mixed-tier production session would need.

## δ. Staleness

The runtime's default scheduler is 5 Hz for periodic reflections, with a 1 m displacement trigger capped at 25 Hz ([`workers.rs`](../../crates/fightbox-runtime/src/workers.rs#L21)). The diagnostic published listener moves to the real simulation worker and counted an update only after `run_reflections` completed and the next 128-frame render block consumed the published snapshot.

| Change | Samples | Median apply lag | Max apply lag |
|---|---:|---:|---:|
| 0.4 m, below displacement trigger | 7 | 199.094 ms | 213.160 ms |
| 2.0 m, above displacement trigger | 7 | 44.325 ms | 47.334 ms |

This is acceptable for a stationary or slowly moving cinematic impulse, but the ordinary periodic case can leave a moved listener hearing the old field for about 0.2 s. The motion trigger is doing useful perceptual work: its measured lag is the 40 ms rate cap plus the reflection tick and block quantization, with some overlap from worker scheduling.

## ε. Long-arrival result

The money test used a controlled, double-sided 200 m by 200 m masonry facade centered 400 m from a source at 0.5 m and a listener at 0 m. The shortest source-to-wall-to-listener path is 799.5 m, or 2.330904 s at 343 m/s. It rendered the opaque Steam Audio IR through a standalone convolution effect and measured the max-absolute Ambisonic-channel envelope in a ±60 ms window.

| Facade | Expected round trip | Global peak time | Expected-window energy | Expected-window peak |
|---:|---:|---:|---:|---:|
| 100 m, 3 s IR | 0.581633 s | 0.588896 s | `9.301e-6` | `2.861e-4` |
| 200 m, 3 s IR | 1.164723 s | effect floor | `5.760e-15` | `1.000e-9` |
| 400 m, 1 s IR | 2.330904 s | truncated | `0` | `0` |
| 400 m, 3 s IR | 2.330904 s | effect floor | `5.760e-15` | `1.000e-9` |

The 400 m test used 131,072 rays, 16 bounces, a 3 s/order-1 IR, and otherwise default source propagation. Raising rays from 16,384 to 131,072 did not change the floor. Checking the listener-to-wall one-way bins also found only the floor, while the 100 m control localized at the correct round trip. The negative result is therefore not truncation, insufficient rays, signed Ambisonic cancellation, or a one-way/round-trip bookkeeping mistake.

The defensible conclusion is narrow: a 1 s IR cannot hold a 2.3 s echo; a 3 s IR can hold and convolve that bin; this realtime Steam Audio setup supplies no perceptually meaningful energy there. Possible causes include the SDK's long-distance irradiance/energy thresholds, reconstruction behavior, or a geometry/source configuration requirement not exposed by this study. A creative reflection-send gain might reveal a weak return, but a gain of useful magnitude must be measured against nearer reflections and noise. Baked reflection data may share the same energy model, so it must pass this exact bin test rather than being assumed to fix it.

## ζ. Per-source budget seam

The important correction is that Steam Audio's *source record* is per source, but the requested quality knobs are not. `IPLSimulationInputs` can vary source flags, pose, attenuation models, directivity, `reverbScale`, hybrid transition/overlap, baked state, and baked layer identifier ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3958)). Today the backend hardcodes `reverbScale = [1,1,1]`, `baked = false`, and one session-level hybrid configuration for every source ([`multi_source.rs`](../../crates/fightbox-steam-audio/src/multi_source.rs#L636)).

Rays, bounces, duration, and Ambisonic order live in `IPLSimulationSharedInputs`, so they are shared by every enabled source in one simulator pass ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L4067)). They can change between passes up to the construction-time `IPLSimulationSettings` maxima ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3880)). The current backend sets the same flags on every source and performs one reflection pass ([`multi_source.rs`](../../crates/fightbox-steam-audio/src/multi_source.rs#L350)).

The effect type is also a construction-time choice. `IPLReflectionEffectSettings` establishes maximum IR size and channel count, while each `IPLReflectionEffectParams` may process fewer channels and fewer samples, explicitly reducing CPU cost ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L2488)). The retained graph already owns one effect per source and passes each source's returned `numChannels`/`irSize` into it ([`multi_source.rs`](../../crates/fightbox-steam-audio/src/multi_source.rs#L1102)). That render-side seam is almost ready for mixed budgets; simulation scheduling is the missing piece.

A minimal API shape would be:

```rust
pub struct SourceReflectionBudget {
    pub rays: i32,
    pub bounces: i32,
    pub duration_s: f32,
    pub order: i32,
    pub cadence_divisor: u8,
    pub delivery: ReflectionDelivery,
}

pub enum ReflectionDelivery {
    Realtime,
    Baked { layer: BakedReflectionLayer },
    Off,
}

impl MultiSourceDescriptor {
    pub fn with_reflection_budget(self, budget: SourceReflectionBudget) -> Self;
}
```

Internally, session construction takes maxima across source budgets and allocates simulator/effects once. At each reflection call, the backend groups due realtime sources by identical `(rays, bounces, duration, order)`, enables only that group's per-source reflection flags, applies the group's shared inputs, and runs one pass. The mixed diagnostic proved the SDK can retain a cinematic source's IR while a later standard-source pass updates the other three sources. Each source snapshot already carries its returned IR shape, so the render effects can process only that source's channels and samples. Cadence belongs in the policy because a 3 s tail does not need to be regenerated at audio rate, and the measured 5/25 Hz worker behavior is part of the budget.

The public API could expose named `Standard` and `Cinematic` constructors to avoid arbitrary invalid combinations, but the budget should remain a value type. The backend will need to validate construction maxima, total per-call grouped work, and the frozen runtime worker's single `run_reflections()` entry point without changing that trait.

## η. Baked reflections

Steam Audio 4.8.1 can bake convolution IR data, parametric reverb data, or both into probe-batch layers ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3480)). The identifier supports listener-centric reverb, a static source with listeners at probes, a static listener with sources at probes, and dynamic probe-pair variation ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3252)). `IPLReflectionsBakeParams` independently specifies rays, diffuse samples, bounces, simulated duration, saved duration, order, and threads ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3490)). Only one reflection bake can run at a time ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3600)).

For the flagship impulsive emitter, `IPL_BAKEDDATAVARIATION_STATICSOURCE` is the direct fit if the emitter is tied to a fixed world position. Listener-centric `REVERB` is cheaper in layer count but cannot preserve a unique static source-to-listener slapback. A moving source with a static listener can use `STATICLISTENER`; fully moving source/listener behavior remains the least attractive bake target.

The diagnostic called `iplReflectionsBakerBake` on the actual megablock mesh and eight representative probes using a static-source convolution layer at the recommended cinematic setting, one CPU thread, and 3 s simulated/saved duration:

| Mesh | Probes sampled | Sample time | Time/probe | Sample layer | Linear 19,881-probe estimate | Linear layer estimate |
|---|---:|---:|---:|---:|---:|---:|
| 964 vertices, 1,442 triangles | 8 | 36.610 ms | 4.576 ms | 259,282 B | 1.52 min | 0.600 GiB |

This is a linear estimate from a tiny sample, not a full bake. Full-grid cache behavior, probe placement, serialization, and progress overhead may change it. Each additional static-source layer is another roughly 0.60 GiB at this setting if scaling remains linear. The existing pathing batch is 19,881 probes and 84 MiB serialized, so a single cinematic reflection layer would dominate the package.

Baking removes realtime ray tracing for sources that select a baked layer through `IPLSimulationInputs.baked` and `bakedDataIdentifier` ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L4008)). It does not remove runtime IR work. Steam stores compact energy fields because convolution still needs reconstructed IRs ([`phonon.h`](../../.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h#L3139)), and the source output is still an `IPLReflectionEffectParams` consumed by the same convolution/mixer/decode graph. The render costs in §γ therefore remain. Probe interpolation/reconstruction cost and its update cadence were not isolated here.

## θ. Recommendation

Use this as the proposed desktop cinematic budget for further implementation work:

| Source class | Rays | Bounces | IR | Order | Periodic/motion cadence |
|---|---:|---:|---:|---:|---:|
| One selected impulsive source | 8,192 | 8 | 3 s | 2 | 5 Hz / up to 25 Hz |
| Up to three standard sources | 2,048 | 4 | 1 s | 1 | 5 Hz / up to 25 Hz, with later cadence reduction allowed |

This tier spends order 2 only where directionality has cinematic value. On this Mac its measured mixed reflection path uses 38.69% of the deadline median and 53.31% at the observed maximum. Sequential grouped simulation consumes 15.42 ms median, and listener-motion application lag stays below 47.34 ms in the measured case. A 3 s/order-2 IR is 4.94 MiB of raw float payload per source; four max-capacity source IRs are 19.78 MiB before SDK workspaces and effect state.

Do not describe this tier as delivering the Heat-style 400 m slapback yet. It delivers the temporal and compute capacity for it. The next implementation gate must make a 400 m controlled return exceed a perceptual threshold without blowing up nearer reflections, then repeat the measurement through the full retained render graph and a real capture. Until that gate passes, increasing rays beyond 8,192 or bounces beyond 8 spends simulation budget without evidence of buying the missing percept.

## ι. Open risks and candidate papercuts

Open risks:

- The render study isolates reflections. The recommended mixed row leaves 1.245 ms at the observed reflection maximum, but only a live full-callback soak can prove the direct/path/host work and OS jitter fit there without dropouts.
- The full simulation matrix uses controlled 20-triangle geometry. The bake sample uses the real 1,442-triangle megablock, but a realtime megablock tick matrix was not run.
- The 400 m return is at the effect floor. The cutoff appears between the 100 m and 200 m controls and survived 131,072 rays. Its SDK/model cause is unresolved.
- The nine-sample matrix maxima are not hard worst-case bounds. An earlier exploratory five-sample pass caught one 34.690 ms scheduler outlier at `16384/16/2 s/o1`; the reported nine-sample rerun's maximum for that row was 15.276 ms.
- Two 1,000-block convolution runs put the four-source 3 s/order-2 maximum at 80.58% and 83.29% of deadline. Even 1,000 blocks is a short soak, so neither is a hard callback bound.
- The mixed simulation test uses selective per-source flags directly against the SDK. Production scheduling, governor interaction, snapshot retention, source activation, and recovery behavior still need design review.
- The 0.600 GiB bake estimate is one eight-probe static-source sample extrapolated linearly. Full bake time, serialized size, load time, reconstruction cost, and interpolation artifacts remain unmeasured.
- Baked reflections may reproduce the same long-distance energy floor. A full bake is not justified until a small baked probe test passes the 400 m tail-bin gate.

Candidate papercuts, reported only:

- Reflection budget discussions can easily call rays/bounces/duration/order "per-source inputs," but Steam Audio places them in `IPLSimulationSharedInputs`. Documenting that distinction beside the backend seam would prevent a misleading API design.
- The crate's audited FFI exposes path baking but not `IPLReflectionsBakeParams`/`iplReflectionsBakerBake`; this diagnostic had to transcribe the pinned 4.8.1 ABI test-locally.
- The usual linked-SDK diagnostic command is easy to run without `--release`, producing meaningless timing numbers. The diagnostic now asserts release mode, but the command should be canonical in contributor docs.
- `IPLReflectionEffectIR` is intentionally opaque, so bin-level evidence requires rendering an impulse through an effect. A small reusable diagnostic helper would avoid repeating that careful setup.
