# Wave 11 width rendering

Status: design decision, with two bounded prototype constants still to be measured before implementation.

Date: 2026-08-01.

## Decision

`StereoImage` and `LineSegment` will remain one logical source, one Steam Audio simulation source, one propagation trajectory, and one occlusion/pathing result. Width will be rendered inside that source's dry stem by a common-transfer center/width renderer. It will not be rendered as endpoint sources, satellites, or several delayed copies.

For a stereo asset, an offline, fixed two-channel analysis produces a center component and an orthogonal width component. A zero-delay, equal-power presentation matrix reconstructs the original stereo image at full angular width and continuously rotates it toward one centered component as the source's angular subtense falls. For a mono `LineSegment`, and for a coherent or mono `StereoImage`, an explicitly delayed quadrature pair supplies the missing width cue. The quadrature pair is applied as opposite serial phase rotations to the two presented channels, never as a delayed wet copy mixed back with dry audio.

Both presented channels then receive the same source direction, propagation history, physical direct-effect parameters, and point-HRTF update. Within either ear, all program components are mixed before that ear's one direction-dependent transfer. This is the property that rules out a walking comb: listener motion never creates two renderer-owned copies of the same waveform with a changing path-length difference.

The topology above is decided. The maximum quadrature angle and the causal quadrature transform's order are prototype constants, not matters to guess in this document. The implementation gate below chooses the smallest transform that meets the asserted magnitude and group-delay bounds, then a short ABX chooses the least width angle that clears the near-width percept. Failure of that experiment stops the implementation; it does not license a fallback to satellites.

## Governing assertions

The design is subordinate to these assertions:

> 1. credible width up close (a mix, not a point),
> 2. smooth monotonic collapse of width with distance,
> 3. NEVER a comb filter anywhere on an approach-orbit-recede walk.

For the Brown Line, the source must be an extended moving line with true pitch movement. Replacing it with a moving point is not an approximation of the feature.

The third assertion determines the topology. It is stronger than “usually does not sound phasey.” A design that generates correlated endpoint signals and hopes a decorrelator hides their changing delay difference is rejected even if one fixture happens to pass.

## What Steam Audio 4.8.1 provides

Steam Audio does not provide a source-extent or rendering-spread control. Its panning effect is documented as a single-channel point-source effect, and its only spatial parameter is a direction (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1466-1480`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1513-1526`). Its Ambisonics encoder is likewise documented as encoding a point source, with direction and order but no spread, and it accepts a mono input (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1788-1808`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1841-1853`). The Ambisonics decoders consume an already encoded Ambisonic field; their parameters rotate and decode that field but do not add a source-extent primitive (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1989-2010`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:2192-2223`).

The binaural effect accepts one or two input channels, but the header explicitly says that every input channel is spatialized from the same point. Its controls are direction, interpolation, `spatialBlend`, HRTF, and optional peak-delay reporting (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1563-1565`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1586-1611`). `spatialBlend = 0` is described as unspatialized and close to the input, while `1` is fully spatialized; it is a 2D/3D blend, not a physical angular-width control (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1594-1605`). The apply call requires one or two input channels and a two-channel output (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1639-1651`).

The direct effect models attenuation and filtering between a point source and listener. It can process an asserted number of channels with common distance attenuation, air absorption, directivity, occlusion, and transmission parameters (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:2308-2343`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:2345-2367`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:2400-2412`). Direct simulation exposes those same physical options and no width option (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:3671-3687`). “Volumetric” occlusion is the one apparent extent primitive in the header, but it models the source as a sphere for visibility sampling only (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:3727-3739`). The per-source simulation input contains pose, attenuation, air/directivity inputs, an occlusion type, an occlusion radius, and an occlusion sample count; it has no line, stereo spread, or panning-extent field (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:3958-3986`).

Steam Audio's utility downmix is the average of all input channels, and the header tells callers to perform a different downmix manually when that behavior is not wanted (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1338-1350`). Wave 11 must do that manual analysis. A blind average would erase anti-phase or strongly asymmetric program material before the width renderer ever sees it.

The kernel therefore gives this design a multichannel direct filter, a same-position binaural effect, a point-source simulation, and Ambisonic building blocks. It does not give the product a native source-width kernel. In particular, an invented coherent Ambisonic “spread” would still sum several direction-dependent HRTF responses. That sum can acquire direction-varying notches, so the Ambisonic construction is not accepted merely because it uses one decode call.

## What reaches the renderer today

The current Tom's Diner descriptor is not a stereo reference. It names `toms-diner-48k-mono.wav` and declares one channel (`fixtures/assets/toms-diner.json:1-15`). The CLI asset path also enforces that state: descriptor regeneration rejects any channel count other than one, creates a mono WAV specification, and the loader rejects decoded WAVs that are not mono (`tools/fightbox-cli/src/asset.rs:252-268`, `tools/fightbox-cli/src/asset.rs:306-363`, `tools/fightbox-cli/src/asset.rs:401-460`). The workbench's `PreparedAsset` is one `Vec<f32>`; it requires a one-channel descriptor and decoder output (`tools/fightbox-workbench/src/asset.rs:61-88`, `tools/fightbox-workbench/src/asset.rs:195-226`, `tools/fightbox-workbench/src/asset.rs:237-269`).

That mono assumption continues through the live renderer. `SourceBlock` contains `decoded_mono`, runtime validation checks its mono length, and the scene-calibration scalar is applied to that one slice before the backend call (`crates/fightbox-runtime/src/render.rs:59-63`, `crates/fightbox-runtime/src/render.rs:512-530`, `crates/fightbox-runtime/src/render.rs:632-664`). The backend seam names its input `input_mono` and its output as left and right slices (`crates/fightbox-runtime/src/backend.rs:79-101`). Live staging and callback construction are mono as well (`crates/fightbox-runtime/src/live.rs:109-145`, `crates/fightbox-runtime/src/live.rs:394-420`).

The linked backend has the same shape. Each source state owns mono input and direct buffers plus stereo result buffers; render validates `input_mono`, passes it through one propagation-delay line, and every direct, pathing, and reflection branch reads that delayed mono stem (`crates/fightbox-steam-audio/src/multi_source.rs:749-790`, `crates/fightbox-steam-audio/src/multi_source.rs:810-828`, `crates/fightbox-steam-audio/src/multi_source.rs:993-1014`, `crates/fightbox-steam-audio/src/multi_source.rs:1016-1121`). Its direct effect is currently created for one channel (`crates/fightbox-steam-audio/src/multi_source.rs:1880-1967`).

The evidence analyzer is less constrained than playback. It recognizes mono and stereo layouts and computes aggregate energy over decoded interleaved samples without first forcing a playback downmix (`crates/fightbox-evidence/src/analysis.rs:1-10`, `crates/fightbox-evidence/src/analysis.rs:33-58`, `crates/fightbox-evidence/src/analysis.rs:137-214`). Stereo ingestion is therefore an asset/runtime/workbench gap, not a limitation of the basic evidence file reader.

The public semantics already say that `Point`, `MultiPoint`, `LineSegment`, and `StereoImage` extents affect volumetric occlusion and that width rendering is not implemented (`crates/fightbox-api/src/lib.rs:422-460`). Wave 11 changes the second half of that statement while preserving the first.

## Semantics that remain frozen

The source-level contract derives one calibration gain from `SplAtOneMeter` and the asset analysis; decoded PCM is to be scaled by that gain exactly once before rendering branches (`crates/fightbox-api/src/lib.rs:107-187`, `docs/decisions/0002-scene-calibration-one-gain-chain.md:17-49`). The runtime presently performs that multiplication once before constructing the backend source block (`crates/fightbox-runtime/src/render.rs:632-664`). Wave 11 does not add a “width gain,” a second distance law, or one calibration per presented channel. The same scalar is applied once to every decoded asset channel, after which all center/width operations are power-normalized spatial transforms.

The linked propagation-delay head is also unchanged. Its published contract uses the exact Doppler ratio

```text
r = 1 / (1 + v_radial / c)
```

with the source's published velocity, bounds delay slew to 0.5 samples per output sample, clamps impossible reads, and uses a 50 ms teleport threshold with a 50 ms equal-power crossfade (`crates/fightbox-steam-audio/src/propagation_delay.rs:1-68`, `crates/fightbox-steam-audio/src/propagation_delay.rs:79-101`). The implementation carries those clamp, slew, reset, and teleport rules in one state machine (`crates/fightbox-steam-audio/src/propagation_delay.rs:169-297`), and its Doppler helper documents the velocity clamp and exact ratio (`crates/fightbox-steam-audio/src/propagation_delay.rs:299-323`). A unit test already holds the textbook ratio within 0.1 percent (`crates/fightbox-steam-audio/src/propagation_delay.rs:516-576`).

The current linked renderer computes its physical delay target from the unsmoothed source/listener positions and the radial component of published velocity, then sends the resulting delayed mono to every downstream branch (`crates/fightbox-steam-audio/src/multi_source.rs:958-1014`). It deliberately distinguishes this shared source transport from later path/reflection approximations (`crates/fightbox-steam-audio/src/multi_source.rs:882-919`). The generic runtime also owns a delay line, but linked-SDK construction sets its target to zero so the Steam backend head is the physical authority (`tools/fightbox-cli/src/phase_b.rs:1509-1524`, `crates/fightbox-runtime/src/render.rs:632-664`). Wave 11 makes both seams channel-aware while retaining one control state, one read trajectory, one slew decision, and one teleport crossfade for the logical source. In the linked graph, the backend head remains authoritative. It is forbidden for left, right, center, side, quadrature, or notional segment endpoints to calculate their own propagation targets.

Existing acoustic motion smoothing is 80 ms, and the backend reads its smoothed endpoints before direct rendering (`crates/fightbox-steam-audio/src/motion_smoothing.rs:1-15`, `crates/fightbox-steam-audio/src/multi_source.rs:950-957`). Width controls and presentation coefficients use those same smoothed endpoints and are ramped sample by sample over each block. They do not introduce a second motion smoother with a different time constant.

The smoothness law is judged on the summed stereo output. The evidence continuity extractor measures inter-block steps and click-like second differences on stereo PCM (`crates/fightbox-evidence/src/metrics.rs:546-640`), and the existing Tom's Diner linked-SDK walk intentionally renders direct, pathing, and reflections together before applying its continuity, pump, and comb assertions (`crates/fightbox-steam-audio/tests/toms_diner_walk.rs:1-7`, `crates/fightbox-steam-audio/tests/toms_diner_walk.rs:59-162`, `crates/fightbox-steam-audio/tests/toms_diner_walk.rs:211-235`). Width gates retain that summed-output rule. A clean isolated width stem is useful diagnostics, but it is not passing evidence.

Finally, the renderer is per source. Source state and rendering are independently indexed (`crates/fightbox-steam-audio/src/multi_source.rs:749-790`, `crates/fightbox-steam-audio/src/multi_source.rs:842-878`). No source may borrow another source's decorrelator, phase seed, delay state, or width budget. Static transform metadata may be cached per asset, but runtime filter state belongs to one logical source instance.

## Rendering model

### StereoImage analysis and collapse

Asset preparation computes a fixed 2 by 2 covariance matrix from the decoded stereo program after DC removal. Its principal unit eigenvector defines `C`, the center component, and the orthogonal vector defines `W`, the natural width component. Signs and the equal-eigenvalue tie are deterministic; ordinary positively correlated stereo resolves to the familiar mid/side basis. Choosing the principal component guarantees `E_C >= E_W` and avoids the catastrophic “anti-phase mid is zero” edge case of an unconditional `(L + R) / 2` fold.

This is an offline, asset-static transform. There is no adaptive PCA in the audio callback and therefore no moving eigenvector, channel swap, or program-driven width pump. The asset manifest records the matrix, `E_C`, `E_W`, a mono-compatibility score, and the exact analysis revision. A stereo asset whose candidate center has a coherent-comb or destructive mono-collapse signature fails ingestion for `StereoImage`; the renderer does not promise to repair an already combed master. Concretely, preflight renders the candidate center as dual mono and rejects it if short-lag comb correlation rises by more than 0.10 over the cleaner original channel or if a regular notch family is more than 6 dB deeper than the power-envelope reference. A delayed-channel stereo control built with Gate 0's 96-sample offset must be rejected (`crates/fightbox-evidence/src/ears/corpus.rs:84-90`).

At runtime, let `k` be the geometric width control in `[0, 1]`. Let `U` be the orthogonal matrix that reconstructs the original left/right program from `[C, W]`. A rotation `R(k)` moves continuously from a far matrix whose first column is equal in both channels to `U` at `k = 1`. The presented two-channel signal is

```text
z(k) = g(k) R(k) [ C, k W ]^T
g(k) = sqrt((E_C + E_W) / (E_C + k^2 E_W)).
```

At full width, `R(1) = U` and `g(1) = 1`, so the authored stereo program is reconstructed. At zero width, `W` is gone and the remaining center component is presented equally to the two point-HRTF ears. Since `E_C >= E_W`, the compensation is bounded by `sqrt(2)`, or 3.01 dB. The compensation preserves expected source power while side energy collapses; it is not a range gain, and it occurs before the one Steam Audio distance/air/occlusion chain. True-peak and summed-output gates must cover the bounded far-center increase.

The matrix contains scalars only. It contributes no frequency-dependent phase and no group delay. Any cancellation between the authored `C` and `W` at full width is part of the source recording, not a renderer-created path difference; the ingestion mono-compatibility gate prevents that authored relationship from becoming a destructive collapse during `k -> 0`.

### LineSegment and coherent stereo

A mono `LineSegment` has no authored `W`. Treating several points on the line as coherent emitters would create the forbidden transfer function, so the renderer instead forms a quadrature companion `Q_Dq{x}` and presents

```text
z_left  = cos(phi) x_Dq + sin(phi) Q_Dq{x}
z_right = cos(phi) x_Dq - sin(phi) Q_Dq{x}.
```

`x_Dq` is the center signal with exactly the same asserted delay `D_q` as the quadrature output. Over the admitted passband, `Q_Dq` is 90 degrees from `x_Dq`, so each presented channel has the same magnitude spectrum as `x`; only their relative phase and therefore interaural coherence change. `phi = phi_max k`. At `k = 0`, both channels are the same centered point. As `k` rises, the opposite phase rotations reduce coherence around that center without creating a second geometric path.

The same phase pair is available to `StereoImage`. It is most important for dual-mono or highly coherent stereo, where the natural `W` carries little energy even though the descriptor asserts a wide image. For authored stereo it follows the presentation matrix, so it rotates each already presented channel serially rather than adding a new copy of `C`. An asset-static coherence score may reduce the phase rotation for a mix that is already wide, but that mapping is fixed for the asset and cannot respond to momentary program content.

This quadrature stage is the sole non-minimum-phase element introduced by Wave 11. The accepted implementation must expose its exact integer algorithmic delay `D_q`. It is an immutable graph latency: every logical source and every direct, pathing, and reflection stage receives the same fixed delay, while width-enabled direct stems use that latency to realize the quadrature pair. A `Point` receives a pure `D_q` delay, so enabling width support cannot move one source relative to another. This is static per-source/per-stage state, not a signal or control shared between sources. Physical propagation targets remain untouched. The dry arm and quadrature arm must match group delay within one sample over 250 Hz to 12 kHz, the combined phase-rotation magnitude must stay within 0.25 dB of unity over that band, and phase rotation tapers continuously to zero below and above the admitted band. `D_q`, filter order, passband ripple, and the taper are capture metadata.

This is intentionally not a generic Schroeder or random allpass decorrelator. A stable allpass is not minimum phase, and an arbitrary dry/wet sum can notch even when the allpass alone has unit magnitude. The accepted pair is a declared-latency analytic phase rotator whose two arms are asserted quadrature before they are summed. Every other new frequency-selective filter must be minimum phase and publish its maximum admitted-band group delay; zero-delay matrices need no such allowance. If a causal pair cannot meet the magnitude, delay, and transient gates at a useful `phi_max`, this model has failed its prototype gate.

### Geometry and the distance law

`LineSegment.length_m` supplies a full segment length; `StereoImage.width_m` supplies a full stereo-image width (`crates/fightbox-api/src/lib.rs:422-460`). A line's local axis is the source pose's forward vector. A stereo image's local axis is its right vector, `normalize(forward x up)`. Source profiles and runtime motion already carry a full pose, while motion publishes velocity separately (`crates/fightbox-api/src/lib.rs:538-549`, `crates/fightbox-runtime/src/backend.rs:18-40`); the workbench currently derives both orientation and published velocity from its trajectory sample (`tools/fightbox-workbench/src/workbench.rs:570-577`). Width orientation therefore does not come from velocity, although a moving Brown Line fixture will normally align its train axis with its trajectory tangent.

There is one seam to extend rather than guess around. The linked simulation frame currently retains each full pose, but the published render snapshot copies only source position and linear velocity (`crates/fightbox-steam-audio/src/multi_source.rs:284-318`, `crates/fightbox-steam-audio/src/multi_source.rs:416-435`, `crates/fightbox-steam-audio/src/backend_snapshot.rs:28-38`). `MultiSourceDescriptor` likewise seeds only an initial position (`crates/fightbox-steam-audio/src/lib.rs:903-928`). Wave 11 must publish forward/up and seed an initial pose before the first simulation update. Inferring a line axis from velocity would fail for a stationary train, a rotating stereo bed, and the first block.

For listener position `l`, source center `p`, unit extent axis `q`, and half extent `a`, define endpoint directions

```text
u_minus = normalize(p - a q - l)
u_plus  = normalize(p + a q - l)
Omega   = atan2(length(u_minus x u_plus), dot(u_minus, u_plus))
k       = sin(Omega / 2).
```

The listener exactly intersecting an ideal zero-thickness endpoint is an excluded singular configuration; normalization uses a small numeric epsilon and fixtures maintain clearance. Everywhere else the function is continuous. For a broadside recede at perpendicular distance `d`, it reduces to

```text
k(d) = a / sqrt(d^2 + a^2),
```

which is strictly decreasing for `d > 0`, tends to one as the listener approaches the segment center, and tends to zero as distance grows. An end-on segment naturally foreshortens. Orbiting changes angular subtense smoothly rather than sweeping a pair of propagation delays.

`k` controls only the zero-delay side coefficient and quadrature phase angle. Steam Audio's center distance attenuation and air absorption are evaluated once with the primary source/listener geometry, exactly as they are now in the direct parameter block (`crates/fightbox-steam-audio/src/multi_source.rs:1020-1040`). There is no `1/r` term in the width stage. Equal-power compensation corrects the deliberate removal of side program energy, not physical range loss, so the one-gain-chain rule is not double-counted.

The initial `phi_max` candidate is 45 degrees, not a frozen product value. The prototype tests 22.5, 30, and 45 degrees and selects the smallest value that clears the objective near-width threshold and the ABX “mix, not a point” judgment without sounding phasey. The geometric law itself is frozen; only the perceptual mapping's maximum is listening-dependent.

### Exact processing order

For a wide logical source, the direct path is:

```text
decoded asset channels
  -> the one SplAtOneMeter/safety scalar
  -> fixed C/W analysis
  -> one shared propagation-delay control and channel bank
  -> zero-delay, equal-power width presentation
  -> declared common algorithmic delay plus serial quadrature phase pair
  -> one multichannel DirectEffect parameter set
  -> one common-direction point-HRTF transfer per presented ear
  -> source direct stereo stem.
```

The presentation matrix is conceptually first and the phase rotation is serial on each presented channel. An implementation may algebraically fuse their coefficients, but it cannot move the phase stage after a sum with another source and it cannot straddle two different HRTF directions. `IPLBinauralEffect`'s two-channel transfer must be characterized with basis impulses before relying on its undocumented internal channel matrix. The header guarantees only that both input channels are accepted and spatialized from the same position (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1563-1565`, `.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1639-1651`). If that transfer does not implement the required channel-separable common-direction mapping, the fallback is two identically parameterized mono binaural effects, retaining the left ear of one and right ear of the other. This doubles binaural-effect work but does not consume another simulation source.

`Point` stays on the existing point DSP path, apart from the graph's declared pure `D_q` latency. `MultiPoint` remains an occlusion sampling descriptor in Wave 11; it does not acquire a rendering-width meaning without a separate percept and fixture. `LineSegment` and `StereoImage` use the width path above.

## Why an orbit cannot create a renderer-owned comb

The rejected endpoint renderer puts the same correlated waveform through two delays:

```text
Y(omega, t) = X(omega) [a exp(-j omega tau_1(t)) + b exp(-j omega tau_2(t))].
```

Its magnitude contains a cross term controlled by `omega (tau_1 - tau_2)`. During an orbit, that delay difference sweeps continuously, so the notches walk through the spectrum. No amount of distance smoothing changes that topology.

The chosen renderer has, for each ear, the form

```text
Y_e(omega, t) = H_e(omega, direction(t))
                T(omega, trajectory(t))
                A_e(omega, k(t))
                Z_e(omega, k(t)).
```

`T` is the single physical propagation trajectory. `H_e` is the existing point-HRTF transfer for that ear and source center. `Z_e` is a zero-delay mixture of program components. `A_e` is the admitted phase rotator, with unity magnitude within its asserted tolerance and the one declared `D_q` latency. There is no sum of `X exp(-j omega tau_1)` and `X exp(-j omega tau_2)`, and therefore no renderer-owned path-difference term for an orbit to sweep.

This proof is about rendering-induced combing. An input recording can contain a comb, and an unsafe stereo-to-center fold can reveal one. That is why the chosen center is covariance-conditioned and why asset ingestion must run mono-collapse and coherent-comb preflight. HRTFs also contain ordinary direction-dependent pinna notches; the test compares the width renderer to the same moving point-HRTF baseline so that “comb” means added walking periodic cancellation, not removal of normal localization cues.

Time-varying phase-rotator coefficients can still create modulation or clicks if updated badly. Sample ramps and the summed-output smoothness gate cover that implementation failure. They do not weaken the structural statement about path-length combing.

## Occlusion, paths, and reflections

Wave 9 maps extent to Steam Audio's volumetric occlusion radius. In the current adapter, point and multipoint use fixed policy radii while `LineSegment` and `StereoImage` use half their declared size, clamped by policy (`crates/fightbox-steam-audio/src/lib.rs:704-755`). That mapping remains authoritative. Wave 11 does not additionally test visibility at audible “endpoints.”

There is one `IPLSimulationInputs` update and one primary simulation result for the logical source. Direct-effect distance, air absorption, directivity, occlusion, and transmission parameters are copied identically across the two presented source channels. A corner can therefore attenuate or transmit the source as a whole, while the width cue survives or collapses with it. The left and right halves of a mix cannot disagree about whether the source is behind the wall.

Pathing and reflections also retain the primary source's baked path and simulation result. Their input is a canonical center fold, normalized once from `E_C` and `E_W` so that removing the natural width channel does not silently halve indirect energy. It comes from the same calibrated PCM and the same shared propagation head as the direct presentation. Wave 11 does not run a baked path, reflection simulation, or occlusion query per notional endpoint. The current backend already branches direct, pathing, and reflection work from one delayed input (`crates/fightbox-steam-audio/src/multi_source.rs:993-1121`); this decision changes that branch point from one mono sample to a shared-control component bank, not to several physical sources.

This first width renderer does not claim that early reflections resolve the two ends of a train or the left and right edges of a music bed. Indirect sound belongs to the one source and room. Endpoint-resolved early reflections would require a new percept, a new path-consistency design, and a proof that it does not reintroduce correlated path combing.

## Doppler

Every center, natural-width, and quadrature sample follows the same propagation-delay read trajectory. The exact ratio still comes from the source center's published radial velocity and the established `1 / (1 + v/c)` rule. The width presentation and quadrature transform are downstream of that resampling, so they cannot acquire a different pitch trajectory.

For a moving `LineSegment`, this gives true, continuously varying center-of-source Doppler while the endpoint subtense produces an extended image. It is not a moving point: a broadband line must clear the objective width and ABX line-versus-point gates below throughout the close pass. Wave 11 deliberately does not model differential endpoint Doppler or train flex. Such a model would need independent emission content per car; applying differential delay to one correlated mono feed would recreate the prohibited comb.

The quadrature transform's fixed delay does not change Doppler because its time derivative is zero. Its implementation metadata and impulse gate make that distinction explicit: physical propagation delay remains governed by published velocity, while `D_q` is fixed graph latency.

## Source slots and governor cost

The runtime hard cap is eight active sources (`crates/fightbox-runtime/src/render.rs:16`). The linked world stores a fixed source array and validates the selected quality tier's cap before creating one `IPLSource` for each logical descriptor (`crates/fightbox-steam-audio/src/multi_source.rs:105-115`, `crates/fightbox-steam-audio/src/multi_source.rs:1417-1434`, `crates/fightbox-steam-audio/src/multi_source.rs:1686-1727`). Desktop permits eight sources and mobile permits four (`crates/fightbox-steam-audio/src/lib.rs:785-803`).

A `StereoImage` or `LineSegment` consumes exactly one of those slots: 1/8 of the desktop source count or 1/4 of the mobile source count. No quadrature component, channel, endpoint, or width lobe receives an `IPLSource`. Simulation, occlusion, pathing, and reflection counts are unchanged.

The DSP cost is not free. Relative to today's mono direct path, a wide source carries two transport/presentation channels, a quadrature-transform state, a two-channel DirectEffect, and either the verified two-channel binaural apply or, in the conservative fallback, two mono binaural applies. The performance contract is therefore expressed as measured per-wide-source callback cost, not disguised as extra source slots. The implementation gate benchmarks one, four, and eight simultaneous wide sources and records p50, p99, and p999 callback time plus persistent bytes per source.

The governor currently ranks sources by predicted direct audibility and degrades the least audible source before reducing global reflection order (`crates/fightbox-steam-audio/src/governor.rs:390-480`, `crates/fightbox-steam-audio/src/governor.rs:840-864`, `crates/fightbox-steam-audio/src/governor.rs:1004-1034`). Its source states are `Full`, `DirectOnly`, and `Virtualized`; virtualization is reserved for a physically calibrated below-hearing case, otherwise degradation stops at `DirectOnly` (`crates/fightbox-steam-audio/src/governor.rs:67-77`, `crates/fightbox-steam-audio/src/governor.rs:1148-1154`). Width is direct identity, so both `Full` and `DirectOnly` retain it. `Virtualized` silences it with the source. The governor must not abruptly turn a line into a point as a hidden CPU rung.

Desktop and mobile may select different immutable width-filter orders after measurement, but the order is chosen by the quality tier at graph construction, not changed during a walk. If eight wide desktop sources miss the existing callback budget, the permitted responses are a cheaper admitted transform or a lower tier-wide order. Charging several source slots, dropping width per block, or adding an inaudible inter-source sharing scheme is not permitted.

## Evidence available and evidence still required

The `ears` module already exposes whole-capture correlation, coherent-comb correlation, ITD, ILD, IACC, and `width = 1 - IACC` (`crates/fightbox-evidence/src/ears/mod.rs:1-24`, `crates/fightbox-evidence/src/ears/extractors.rs:57-96`, `crates/fightbox-evidence/src/ears/extractors.rs:98-139`). Its IACC search uses a plus or minus 1 ms lag window, and its coherence analysis covers bands from 125 Hz through 12 kHz (`crates/fightbox-evidence/src/ears/extractors.rs:8-11`, `crates/fightbox-evidence/src/ears/extractors.rs:202-235`). Gate 0 includes a coherent 96-sample delayed-copy corruption and a mono-collapse corruption (`crates/fightbox-evidence/src/ears/corpus.rs:10-38`, `crates/fightbox-evidence/src/ears/corpus.rs:84-116`). It already proves at least a 0.10 comb-correlation separation for the coherent-comb corruption and a 0.02 IACC separation for mono collapse (`crates/fightbox-evidence/src/ears/gate0.rs:20-34`, `crates/fightbox-evidence/src/ears/gate0.rs:63-71`, `crates/fightbox-evidence/src/ears/gate0.rs:103-180`).

There is also a moving spectral-notch metric with a 15 dB default threshold, but it presently consumes summed stereo plus a mono reference and collapses the rendered stereo before analysis (`crates/fightbox-evidence/src/metrics.rs:642-709`). The current ears API computes one width result for an entire capture. Contrary to the Wave 11 planning shorthand, there is no existing exported width-versus-distance profile (`crates/fightbox-evidence/src/ears/mod.rs:21-24`, `crates/fightbox-evidence/src/ears/extractors.rs:98-139`). Wave 11 needs a windowed profiler that reports time, source distance, angular subtense, IACC, width, and confidence for each admitted window. It also needs a stereo-reference/per-ear form of the moving-notch detector.

The linked Tom's Diner walk already supplies the appropriate test-harness shape, an approach/corner/recede trajectory and one summed capture, but its loader asserts a mono asset (`crates/fightbox-steam-audio/tests/toms_diner_walk.rs:25-33`, `crates/fightbox-steam-audio/tests/toms_diner_walk.rs:59-162`, `crates/fightbox-steam-audio/tests/toms_diner_walk.rs:427-457`). The Wave 11 fixtures must add a pinned, rights-cleared stereo excerpt and a separate mono Brown Line signal; relabeling the current mono file as stereo evidence is not acceptable.

## Gates

### Percept 7.1: credible width up close

The linked-SDK fixture uses the pinned stereo program as `StereoImage { width_m: 4.0 }`, with the listener 2.5 m broadside from the source. It renders a matched `Point` control with the same PCM, level, pose, direct/path/reflection settings, and HRTF. The new 500 ms, 50 percent-overlap width profile must report a median near-field width of at least 0.20 and at least 0.10 greater than the point control. The summed capture must be finite, non-silent, and within the existing true-peak and continuity limits.

That automation proves a stable binaural spread, not that the result is a credible stereo mix. “The voice remains centered, the accompaniment is broad, and neither sounds hollow, phasey, head-locked, or like frequency bands painted across space” is ABX-only. The acceptance session is twelve randomized trials among point, authored-stereo width, and the coherent-satellite negative control. At least 10 of 12 width-versus-point identifications are required, followed by an explicit pass from md on the quoted quality judgment. The result, HRTF, headphones, `phi_max`, and transform revision are capture metadata.

### Percept 7.2: smooth monotonic collapse with distance

The same broadside source recedes continuously from 2.5 m to 40 m at 1.5 m/s. The profiler aggregates windows into log-distance bins centered at 2.5, 4, 6.3, 10, 16, 25, and 40 m. Median width must have Spearman `rho <= -0.95` with distance, no outward adjacent-bin increase greater than 0.02, a near-to-far decrease of at least 0.10, and a 40 m value within 0.03 of the matched point control.

An analytic unit test samples the exact subtense law over the same interval and requires a finite, continuous, strictly decreasing `k(d)`. The summed-output continuity extractor must find zero click events and no more than 1 dB of additional worst inter-block step relative to the point control. “Collapse sounds like a source becoming smaller rather than an effect being turned down” remains ABX-only and uses the same rendered recede, because IACC alone cannot hear a conspicuous timbral morph.

### Percept 7.3: no comb on approach, orbit, or recede

The canonical walk approaches from 40 m to 2.5 m, completes one 360-degree orbit at 2.5 m while keeping the source in view, then recedes to 40 m. It is rendered once as `Point`, once with the chosen width renderer, once with naive coherent endpoints as a positive corruption, and once with the Gate 0 coherent-comb signal. The endpoint renderer exists only inside the test fixture and can never enter production.

The extended moving-notch detector runs on left ear, right ear, and mono sum against a stereo point reference. Added moving-notch depth must remain below 15 dB in every channel and no worse than the point control plus 3 dB. In 500 ms windows, comb correlation may not exceed the point control by more than 0.10. Summed continuity must report zero clicks. The two known corruptions must fail, proving the trajectory and extractor can hear the prohibited defect rather than merely returning green. A separate ingestion test feeds the coherent delayed-channel stereo corruption to the center analysis and requires the explicit mono-compatibility rejection, so unsupported source combing cannot enter this walk as if it were renderer evidence.

Structural unit tests make the word “never” tractable. Each logical source must expose one propagation control identity; all component-channel delay targets and teleport state observations must be identical; no endpoint position may reach a delay-line constructor; and a basis impulse through the two presentation channels must show relative dry/quadrature group-delay error of at most one sample after subtracting declared `D_q`. A static sweep of every admitted `k` and transform band must keep phase-rotator magnitude within 0.25 dB of unity.

No finite corpus proves absence at every possible position, HRTF, and input. The algebraic no-second-path invariant plus the corruption-sensitive canonical walk is the automated proof available. “No moving hollow, flange, or spectral zipper is audible on the full walk” is explicitly ABX/listening-only, not silently inferred from IACC.

### Percept 2: Brown Line extent with true pitch movement

The Brown Line fixture is a 16 m `LineSegment` with a mono broadband train-like bed and a source pose whose forward axis follows a straight 15 m/s pass. The listener is 3 m from the track at closest approach. A second render replaces the bed with a 1 kHz tone solely for pitch measurement. Published velocity remains the fixture's actual 15 m/s vector; it is not reconstructed from smoothed positions.

For the tone, measured steady pitch must match `1000 / (1 + v_radial / 343)` within 0.1 percent and 0.75 Hz, and residual adjacent-window pitch motion may not exceed 10 cents after subtracting the analytic trajectory. Those limits preserve the existing exact-Doppler unit contract rather than inventing a looser width exception. For the broadband bed, median closest-pass width must be at least 0.15 and at least 0.10 above a matched moving-point control; at 40 m it must be within 0.03 of that control. The approach/orbit/recede comb checks also run on the line renderer.

A single sinusoid is not expected to prove spatial extent, so pitch and width use different inputs. “This reads as a train-length source passing the listener, not a diffuse blob glued to a moving point” is ABX-only. Twelve randomized line-versus-point trials require at least 10 correct identifications plus md's explicit acceptance of the quoted judgment. If the objective width clears but that judgment fails, changing `phi_max` is allowed; adding endpoint copies is not.

## Rejected alternatives

Naive left/right endpoints for stereo are rejected by Percept 7.3. Correlated content reaches each ear through a changing endpoint path difference, so an orbit is a literal swept comb.

Several coherent samples along a line are rejected by Percept 7.3 and by the source budget. More points make the interference denser; they do not turn it into decorrelation. Treating those points as Steam Audio sources would also turn one Brown Line into several of the eight desktop slots.

Decorrelated satellites are rejected as the primary model. A random or allpass-processed copy remains a copy, satellite motion still adds a geometric delay, and a dry/wet allpass sum can notch. Independent noise tails may be useful for a future diffuse-reflection model, but they cannot carry the source's direct stereo program or its exact Doppler identity.

A coherent Ambisonic angular kernel is rejected by Percept 7.3. It avoids explicit source slots, but binaural decode still linearly combines direction-dependent HRTFs for the same waveform. The resulting coloration may move during an orbit, so it lacks the common-transfer proof. Ambisonics remains appropriate for diffuse reflections, not for faking coherent direct extent.

Steam Audio `spatialBlend` is rejected for this feature. The header defines it as a blend between unspatialized input and full point spatialization, not source extent (`.cache/steam-audio/steamaudio-4.8.1/steamaudio/include/phonon.h:1594-1605`). Crossfading correlated dry and HRTF-filtered copies has no no-comb proof, and an unspatialized mono Brown Line is still not a line. It fails Percept 7.3 and Percept 2.

Fading side energy to zero without equal-power center compensation is rejected by the one-gain-chain and smoothness semantics. It makes range change program level according to how stereo the asset happens to be, on top of the physical distance/air chain.

Independent endpoint occlusion, pathing, or Doppler is rejected. It lets the two halves of one source disagree behind a corner and recreates correlated time-varying paths. Endpoint-specific Doppler becomes admissible only when the source supplies genuinely independent per-endpoint emission stems.

A pure moving point, including one with a larger volumetric-occlusion radius, is rejected by Percept 2. Occlusion extent does not make the dry stem sound extended.

## Smallest experiments that settle the open constants

Before production code is divided into lanes, one linked-SDK offline prototype renders four 12-second variants from one deterministic harness: the current point renderer, naive coherent endpoints as the known bad control, the chosen common-transfer renderer, and Steam Audio's two-channel `spatialBlend` path as a measured rejected control. Rendered inputs are dual mono, the pinned Tom's Diner stereo excerpt, and broadband mono train noise. A correlated stereo file with a small channel delay is the ingestion-rejection control and must not reach rendering. Trajectories are stationary near, broadside recede, and approach-orbit-recede.

The first measurement is a basis-impulse characterization of `IPLBinauralEffectApply` with a two-channel input. It records the full 2 by 2 transfer and `peakDelays`. If input channel isolation and the required common-position ear mapping are not exact enough for the common-transfer proof, the prototype immediately uses the paired-mono-effect routing described above.

The second measurement sweeps causal quadrature candidates. A candidate is admissible only if it meets the 0.25 dB magnitude and one-sample matched-group-delay assertions over 250 Hz to 12 kHz, declares a fixed `D_q`, remains click-free while `k` follows the orbit, and makes both Gate 0 corruptions fail the new comb gate. Candidates should include a short FIR analytic pair and a low-order polyphase allpass Hilbert pair. Generic randomized allpass satellites are not candidates.

The third measurement renders `phi_max` values of 22.5, 30, and 45 degrees. Objective gates discard values that do not reach near width or that produce a false comb/pump result. The shortest ABX described above then chooses the least remaining angle. This is the minimum listening test needed because IACC cannot decide whether a low-correlation image sounds like a stereo mix or a phase trick.

A final point-parity experiment renders 72 source directions at far-collapse `k = 0`. After removing declared `D_q`, the width renderer must match the current point renderer's per-ear magnitude within 0.25 dB and peak delay within one sample. One, four, and eight wide-source runs then determine callback p99/p999 and persistent per-source memory. These measurements freeze the tier-specific transform order before implementation spreads across the repository.

## Proposed implementation lanes

The estimates include code, tests, and local documentation, not fixture audio bytes.

1. The stereo runtime seam owns `crates/fightbox-api/src/lib.rs`, `crates/fightbox-runtime/src/backend.rs`, `crates/fightbox-runtime/src/render.rs`, and `crates/fightbox-runtime/src/live.rs`. In roughly 600 to 900 lines over four to six engineer-days, it makes decoded source blocks channel-aware, applies one calibration/safety scalar, and exposes a shared-control multichannel transport plus declared renderer latency. It does not implement width DSP.

2. The backend width lane owns a new `crates/fightbox-steam-audio/src/width_render.rs` plus `crates/fightbox-steam-audio/src/multi_source.rs` and `crates/fightbox-steam-audio/src/lib.rs`. In roughly 900 to 1,300 lines over six to nine engineer-days, it implements static C/W metadata consumption, subtense, equal-power presentation, the admitted quadrature pair, common DirectEffect parameters, same-position HRTF routing, and canonical indirect fold. It owns no asset decoder or evidence extractor.

3. The asset and fixture lane owns `tools/fightbox-cli/src/asset.rs`, `tools/fightbox-cli/src/fixture.rs`, `tools/fightbox-cli/src/schema.rs`, `tools/fightbox-cli/src/phase_b.rs`, and `fixtures/**`. In roughly 500 to 800 lines over three to five engineer-days, it preserves stereo PCM, computes and pins transform/coherence metadata, adds mono-compatibility preflight, acquires the rights-cleared stereo excerpt, and creates the Wave 11 stereo and Brown Line fixtures. It does not touch workbench Rust files.

4. The evidence lane owns `crates/fightbox-evidence/src/ears/**`, `crates/fightbox-evidence/src/metrics.rs`, and new linked tests `crates/fightbox-steam-audio/tests/wave11_width.rs` and `crates/fightbox-steam-audio/tests/wave11_line.rs`. In roughly 600 to 900 lines over four to six engineer-days, it adds windowed width-versus-distance profiling, per-ear/stereo-reference moving-notch analysis, source-transform preflight, pitch tracking, corruption-sensitive controls, and all four gates. It does not change the renderer.

5. The workbench lane owns `tools/fightbox-workbench/src/asset.rs`, `tools/fightbox-workbench/src/fixture.rs`, `tools/fightbox-workbench/src/workbench.rs`, and `tools/fightbox-workbench/src/capture.rs`. In roughly 400 to 650 lines over three to four engineer-days, it streams stereo assets, displays descriptor/width/`k`/declared latency, adds a per-source Point versus Extent A/B control, and writes those states into captures. It consumes fixtures from lane 3 and does not edit them.

6. The governor/performance lane owns `crates/fightbox-steam-audio/src/governor.rs` and dedicated quality-tier/performance tests. In roughly 250 to 450 lines over two to three engineer-days, it records wide-source cost, retains width in `DirectOnly`, selects immutable transform orders per tier, and proves one/four/eight-wide-source budgets. It does not edit `multi_source.rs`.

The rough total is 3,250 to 5,000 lines and 22 to 33 engineer-days before review. Lanes 1, 3, and the extractor half of 4 can start together. Backend lane 2 consumes lane 1's seam and lane 3's pinned metadata. Workbench lane 5 can start against generated stereo samples but becomes a useful listening surface only after lane 2 lands. Governor lane 6 measures the completed backend.

## Immediate workbench hearing path

The workbench currently loads one mono vector into its profile and streams mono source blocks (`tools/fightbox-workbench/src/workbench.rs:327-362`, `tools/fightbox-workbench/src/workbench.rs:494-507`, `tools/fightbox-workbench/src/workbench.rs:1570-1593`). The shortest hearing path is therefore not a hidden backend toggle. It requires lane 3's pinned stereo Tom's Diner asset, lane 1's channel-aware live block, and lane 5's stereo loader.

On the megablock fixture, Tom's Diner should expose `Point` and `StereoImage` as an instantaneous, click-free A/B during a plaza walk. The panel should show physical width, current angular subtense, `k`, `phi`, current quality state, and declared algorithmic latency. It should provide solo, summed scene, capture, and reset-to-fixture controls. The Brown Line fixture needs the same Point/LineSegment A/B plus a trajectory overlay and published radial velocity so md can hear width and true pitch in the same pass.

Every capture used for a decision records the asset hash, source transform metadata, HRTF, quality tier, quadrature revision and order, `D_q`, `phi_max`, fixture hash, and whether it was point or extent. That makes the immediate listening result reproducible rather than a workbench-only impression.

## Explicit nonclaims and stop conditions

Wave 11 renders perceptual angular extent around one source center. It does not resolve a listener pointing to individual train cars, simulate a separate Doppler curve per endpoint, or make indirect reflections endpoint-resolved. It also does not make an arbitrary mono pure tone localize as several stable positions; the Brown Line width percept is evaluated with broadband train material while the tone is reserved for pitch evidence.

The topology is rejected, rather than weakened, if the smallest prototype cannot simultaneously meet all of these conditions: useful near-field IACC separation, corruption-sensitive no-comb gates, no more than 0.25 dB admitted-band magnitude ripple, one-sample matched group delay after declared `D_q`, exact existing Doppler, summed click-free motion, and the stated ABX judgments. The next design investigation in that event is a single-path time-frequency spatial tiler in which each time-frequency cell has exactly one direction, not coherent satellites. That alternative is intentionally deferred because its likely failure mode is audible spectral “rainbow” localization, which only a prototype can settle.
