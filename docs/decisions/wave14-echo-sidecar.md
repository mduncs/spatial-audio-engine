# Wave 14 decision: image-source echo sidecar and the NLOS event anomaly

**Status:** design for md's gate; no implementation is authorized by this document.

**Revision under examination:** `fd65f8c`, Steam Audio 4.8.1, 48 kHz / 128 frames.

**Frozen constraints:** `D_q = 0`; the existing calibrated source chain and final safety limiter remain authoritative; the signed impulse filter and knot table remain unchanged; `EXECUTION.md` §κ's degradation order remains unchanged; `fightbox-runtime/src/backend.rs` is not touched; old and explicit-off scenes retain golden output hash `e43a455e6eda686dbea905e16c434474406f55bf7db6356f2d145d24137a27ef`.

## Decision summary for md (one page)

**The NLOS result is primarily a diagnostic harness gap, not a proved event-engine bug.** Wave 14 builds a baked-path session and runs path simulation, but every blast capture sets `pathing=0`. Its “exact zero” proves only that direct is blocked and Steam's realtime reflections are at numerical floor. The blast has its normal reflection send; only the crack is pinned off. The cheap first action is a path-only and all-stage capture of the same impulse. No production send fix is justified before it.

Cadence does not explain this corrected case. Inactive event slots still receive simulation inputs. Activation invalidates the old path instead of invoking W3's three-miss hold, and a probe-covered path resolves on the next 15 Hz pass. The corner blast has about 780 ms of flight time against 67 ms path and 200 ms ordinary reflection periods; its slot remains active for the 3 s program plus 0.75–1.5 s. A very near teleported event deserves a separate race test, but this event has ample convergence time.

**The missing echo field is nevertheless real.** At 167.6 m LOS, Steam supplies an early bed 6–9 dB below direct with only 33–53 ms T60. At 267.6 m around the corner and 452.5 m LOS, reflections sit near -166 dBFS. Twelve configurations yield no discrete return, and Full 2.0 s costs about 9–10 ms per executed simulation tick without changing that. An earlier controlled study found the right 0.589 s return at 100 m and numerical floor at 200 m and 400 m. This is a long-path reflection-model limit, not special treatment of one-shot PCM.

**Recommendation:** add a default-off image-source sidecar for explicitly authored blasts, small-arms shots, and impacts. The crack and steady sources remain off. Runtime reads deterministic facade/corner paths precomputed per static event anchor and listener probe. The audible megablock v1 may generate the same table analytically from its street grid; production tables come from offline mesh plane and edge extraction. Runtime plane extraction and an assumed Steam baked-reflection cure are rejected.

Full renders at most four paths per event and eight globally: up to three strongest specular returns plus a reserved corner-diffraction path in NLOS. These are delay/filter/HRTF taps, not logical or Steam simulation sources. Specular level uses total-path spherical spreading and air loss, with material pressure loss per bounce; direction comes from the final bounce. The corner path is source → edge → listener and starts with provisional 9/15/24 dB loss at 250 Hz/1 kHz/4 kHz. That voicing is for md to ratify, not claimed exact physics. The existing `ArtilleryThunder` shaper runs once at total path distance with its signed knots unchanged.

The sidecar branches from calibrated dry mono before the backend's direct shaper and delay, then applies its own physical path delay, filters, and HRTF. Its bus sums parallel to Steam reflections before the existing monitor and -1 dBTP limiter. It adds no direct-path delay, preserving `D_q=0`. A separate echo gain defaults off and follows the existing reflections master without changing public `StageOutputGains` or `backend.rs`.

Planning cost is below 0.02 ms once per trigger and 0.00 recurring simulation ms/tick. Expected audio cost per 128-frame block is 0.12–0.18 ms for four taps, 0.06–0.09 ms for two, and 0.03–0.05 ms for one; these are estimates awaiting the proof gate. Full keeps 4/8 per-event/global taps, Reduced 2/4, Minimum 1/2, and `DirectOnly`/`Virtualized` turns echoes off inside `EXECUTION.md` §κ's existing reflection-first order. The audible v1 is 3–5 engineer-days, a gate-worthy v1 is 8–12 total, and a generic mesh/moving-source solution is 3–5 engineer-weeks.

**Ratification requested:** accept the harness-gap classification and cheap check; choose offline tables as runtime truth with analytic megablock generation only for v1; approve event-only eligibility, 4/8 tap caps, provisional corner voicing, governor mapping, and the 8–12 day v1 budget. Even if path-only proves that event corner fill already exists, the sidecar is still required for discrete 0.3–1.2 s facade returns.

## α. Ruling on the NLOS anomaly

### Classification

The classification is **harness gap (primary), plus a real long-path reflection-model limitation; not a demonstrated production engine bug, and not cadence physics for the corrected case**.

There are two claims hidden inside “the event is exactly zero around the corner,” and the evidence supports only one of them:

| Claim | What the evidence actually says | Ruling |
|---|---|---|
| Direct + realtime reflections are zero around the corrected corner | Direct peak is `-inf`; reflection energy is roughly `1.0e-12` to `2.8e-12`, with a `-166.268 dBFS` peak, across Reduced, Full 1.5 s, and Full 2.0 s | Proven |
| Baked pathing is zero for that blast | Wave 14 never renders pathing into a capture | Not measured |

The invalid original W0 muzzle, which was inside a building, is not used here. The corrected W0.1 coordinates and output are the evidence of record ([`EXECUTION.md:2473-2490`](../../EXECUTION.md#L2473)).

### The Wave 14 capture mutes pathing

The corrected harness creates a real session from both the scene mesh and `BakedProbeBatch`, configures order-2 pathing with validation and alternates, then explicitly runs direct, pathing, and reflection simulation ([`wave14_echo_truth.rs:218-264`](../../crates/fightbox-steam-audio/src/wave14_echo_truth.rs#L218), [`wave14_echo_truth.rs:364-381`](../../crates/fightbox-steam-audio/src/wave14_echo_truth.rs#L364)). Its session is not missing baked path data.

The capture controls are the defect. `REFLECTIONS_ONLY` sets `pathing: 0.0`, and `DIRECT_ONLY` also sets `pathing: 0.0` ([`wave14_echo_truth.rs:55-64`](../../crates/fightbox-steam-audio/src/wave14_echo_truth.rs#L55)). The harness settles and records the crack and blast under `REFLECTIONS_ONLY`, then records the comparison impulse under `DIRECT_ONLY`; it defines no path-only or unity capture ([`wave14_echo_truth.rs:285-322`](../../crates/fightbox-steam-audio/src/wave14_echo_truth.rs#L285)). Calling `run_pathing()` fills the snapshot, but a zero stage gain guarantees none of it reaches the measured samples.

The `baked: IPL_FALSE` field in `source_inputs` is not evidence to the contrary. It selects Steam's **baked reflections** delivery. Baked pathing is independently supplied through `pathingProbes`, with order, visibility, validation, and alternate-path controls in the same input record ([`multi_source.rs:800-827`](../../crates/fightbox-steam-audio/src/multi_source.rs#L800)). The Wave 14 output also reports both source and listener probe coverage as true.

### The blast send is not disabled

`MultiSourceDescriptor::at` defaults `reflection_send_enabled` to true ([`lib.rs:937-951`](../../crates/fightbox-steam-audio/src/lib.rs#L937)). The workbench's crack descriptor alone calls `.with_reflection_send(false)`; the immediately following blast descriptor is `ArtilleryThunder`, inactive/reusable, transient-priority, and retains the default send ([`workbench.rs:855-869`](../../tools/fightbox-workbench/src/workbench.rs#L855)). The API comment is explicit that disabling this send affects only the source's reflection mixer contribution; direct and baked pathing remain available ([`lib.rs:1020-1028`](../../crates/fightbox-steam-audio/src/lib.rs#L1020)). Wave 14 constructs the same distinction directly: crack off at line 410, blast normal at lines 411–417 ([`wave14_echo_truth.rs:397-418`](../../crates/fightbox-steam-audio/src/wave14_echo_truth.rs#L397)).

Therefore a missing blast reflection-send flag is not the bug. Changing the crack rule would violate M3 and would not fix the blast.

### Event lifetime and cadence do not explain this case

Steam simulation solves propagation from endpoints, not from the duration or stationarity of the PCM. `run_pass` writes source inputs for every configured source slot, without filtering on application `active`, before invoking the SDK ([`multi_source.rs:443-502`](../../crates/fightbox-steam-audio/src/multi_source.rs#L443)). The inactive reusable blast slot can therefore be warmed before a shot. This is especially decisive in the corrected fixture because its muzzle is static rather than chosen at trigger time.

On activation or teleport, the backend immediately invalidates the old path target and publishes zero before the blocking pass ([`multi_source.rs:371-401`](../../crates/fightbox-steam-audio/src/multi_source.rs#L371)). W3's “hold two misses, fade on the third” rule applies only to brief coverage loss during continuous motion. With probes at both endpoints, the next valid path output calls `resolve`; it does not wait through three misses ([`multi_source.rs:565-604`](../../crates/fightbox-steam-audio/src/multi_source.rs#L565)).

The default worker cadence is 60 Hz direct, 15 Hz pathing, and 5 Hz reflections ([`workers.rs:21-29`](../../crates/fightbox-runtime/src/workers.rs#L21)). For the corrected 267.601 m corner case, direct flight time is approximately `267.601 / 343 = 0.780 s`. That admits roughly eleven path periods and three ordinary reflection periods before the first blast sample can arrive, even before crediting prewarming. The workbench retains its reusable slots for `EVENT_PROGRAM_SECONDS + reflection_duration_s`, or 3.75 s at Reduced and 4.5 s at Full ([`ballistic_event.rs:15-20`](../../crates/fightbox-steam-audio/src/ballistic_event.rs#L15), [`workbench.rs:925-938`](../../tools/fightbox-workbench/src/workbench.rs#L925)). The 3 s source program is not expiring before a send converges.

There is a narrower future risk: trigger admission checks only for one 16.7 ms direct tick, and the simulation update is published at the end of the UI update ([`workbench.rs:1348-1369`](../../tools/fightbox-workbench/src/workbench.rs#L1348), [`workbench.rs:1522-1529`](../../tools/fightbox-workbench/src/workbench.rs#L1522)). A newly teleported, very near NLOS event might reach the listener before the first 67 ms path pass or 200 ms reflection pass. That requires a dedicated near-event test. It cannot explain a 780 ms flight with predeclared endpoints.

### Why the steady source looks different

Wave 13 is not an event-versus-steady A/B. It places one continuous point source at `[102.5,102.5,1.5]` and walks the listener about 46–49 m from it across a nearby corner ([`wave13_corner_gate.rs:20-36`](../../crates/fightbox-steam-audio/src/wave13_corner_gate.rs#L20)). At each position it runs four explicit direct/path/reflection pass sets, settles 1.877 s, and then measures direct, path, reflections, and all stages separately ([`wave13_corner_gate.rs:393-499`](../../crates/fightbox-steam-audio/src/wave13_corner_gate.rs#L393)). Its descriptor uses normal sends ([`wave13_corner_gate.rs:533-561`](../../crates/fightbox-steam-audio/src/wave13_corner_gate.rs#L533)). Reduced's measured shadow mean is 4.62 dB below LOS, with independent repeats at 4.647 and 4.584 dB ([`EXECUTION.md:2388-2395`](../../EXECUTION.md#L2388)).

Wave 14 instead examines the realtime reflection stage at 167.6, 267.6, and 452.5 m, and omits path output. Source temporality is not the controlled variable. Range, geometry, source population/governor state, input signal, and measured stages all differ.

The observed range behavior matches the independent reflection-budget report: a controlled 100 m facade returns at 0.589 s and `2.861e-4`, while 200 m and 400 m cases sit at the effect's `1e-9` peak floor despite a 3 s IR and up to 131,072 rays ([`reflection-budget-study.md:165-178`](../diagnostics/reflection-budget-study.md#L165)). That evidence makes long-path reflection energy a much better explanation than “Steam ignores one-shots.”

### Minimal cheap fix and stop rule

The minimal fix belongs to the **diagnostic only**:

1. Add `PATH_ONLY = {0,1,0}` and `ALL = StageOutputGains::UNITY` captures for the identical blast impulse after the same simulation passes.
2. Report path snapshot EQ/SH energy, path-only peak/RMS around the expected blast arrival, and all-stage energy for every listener and rung.
3. Add a continuous-noise control using the same Wave 14 source and listener coordinates. This isolates source temporality without changing geometry.
4. Keep the existing crack reflection null, direct-only, and reflection-only results.

If path-only is nonzero, the production engine already has event corner fill and the alleged application bug closes. If path snapshot energy is nonzero but path-only audio is zero, reopen a render-routing bug. If the snapshot itself is zero despite both endpoints having probes, reopen a path-simulation/configuration bug. None of those engine fixes is justified by the present capture.

This cheap check does not threaten the sidecar decision. Even perfect path fill is a low-frequency/directional transport contribution, not the missing 0.3–1.2 s discrete facade sequence. The sidecar remains necessary for that percept.

## β. Product decision and boundaries

The sidecar's job is narrow: add a few deterministic, physically legible late arrivals to impulse events while Steam continues to own direct sound, baked path transport, and the diffuse reflection bed. It is not a replacement reverb engine. It must not “repair” NLOS by making the occluded direct path audible, and it must not turn every source into a rhythmic multi-tap delay.

The semantic opt-in should be independent of `ImpulseClass`. `ImpulseClass::ArtilleryThunder` answers “how does distance wash this source?”; an echo profile answers “may authored geometry emit discrete returns for this source?” Inferring echo eligibility from the filter class would silently change old scenes and prevent future combinations. The default echo profile is `Off`, structurally absent.

V1 eligibility is:

| Emission | V1 | Reason |
|---|---|---|
| Muzzle blast / artillery blast | On when the fixture opts in | Primary percept target |
| Small-arms one-shot and impact | On when authored | Same discrete-event behavior; useful for scripted exchanges |
| Ballistic crack | Off, pinned | M3 says it owns no reflection send; echoing the tangent-source crack would create false geometry |
| Steady machinery, rotor, bells, music, fire | Off | Existing steady field is accepted; coherent copies risk comb filtering and obvious repetition |
| Repeated/looped “artillery wallpaper” | Off unless each emission has an explicit trigger generation | A loop is not a reliable event clock |

Loud steady sources can be reconsidered only after a later test shows that tap-set interpolation, phase decorrelation, and crossfades do not damage the sources md already accepts. It is outside v1.

## γ. Geometry decision

### Recommendation: precomputed probe/anchor tables at runtime

The runtime source of truth should be a compact echo table baked for each eligible static or scripted source anchor and listener-probe region. The table is generated offline from authored geometry. Each candidate record contains a stable path identifier, path kind, source anchor, listener probe, ordered bounce or edge points, total and excess path length, final arrival direction, material identifiers or signed three-band pressure coefficients, visibility validity, and a deterministic priority key. Package metadata pins the geometry, material map, table schema, baker revision, and content hashes.

The production baker should derive large planar facade patches and diffraction edges from the real mesh offline: cluster coplanar triangles by normal/material, build finite polygons, reject tiny or back-facing patches, then enumerate valid one- and two-bounce images plus corner paths for authored source anchors and probes. Runtime performs no triangle search. For a moving listener, it selects matching stable path IDs from nearby probes and interpolates delay/gain/direction only when all contributors contain that path; otherwise it crossfades at a deterministic boundary. For an impulse v1, freeze the selected tap plan at trigger generation so a single emitted wavefront does not pitch-shift or change topology while it is in flight.

The fastest audible v1 may generate the same table schema from the megablock's analytic rectilinear street grid and known facade rectangles. That is an oracle and an ear-test accelerator, not the permanent geometry authority. It allows md to hear the central idea before the generic plane extractor exists.

### Rejected geometry alternatives

**Runtime mesh-derived planes** are rejected. Clustering and visibility work is control-heavy, topology can flicker under small listener moves, and doing any of it on the audio callback violates the retained architecture. A background runtime job would still make event onset depend on job completion and would complicate determinism.

**Analytic street-grid geometry as the shipping authority** is rejected. It is ideal for the megablock v1 but does not describe imported city meshes, non-axis-aligned facades, courtyards, or material assignments.

**Steam baked reflections as the assumed solution** is rejected pending a small proof bake. The existing study estimates roughly 0.60 GiB per static source at the large 19,881-probe configuration, still requires runtime IR reconstruction/convolution, and may reproduce the same long-distance floor ([`reflection-budget-study.md:225-240`](../diagnostics/reflection-budget-study.md#L225)). It remains a possible future diffuse-field optimization, not the Wave 14 discrete-tap plan.

## δ. Path synthesis

### Candidate budget and selection

At trigger time, select no more than four paths for one event and no more than eight live paths across the graph. Full v1 reserves up to three slots for the strongest valid specular candidates and one for a valid corner path when direct sound is occluded. The reservation prevents a weaker but perceptually necessary corner arrival from losing to several facade images. Reduced and Minimum keep the highest predicted received-pressure candidates while retaining a valid diffraction path in NLOS.

Selection is deterministic: validate segment visibility, reject paths outside the authored 0.3–1.2 s **excess-delay** window for the facade percept, score by predicted band-weighted received pressure, then sort by score, total path length, stable path ID. A separate corner-only arrival may be earlier than 0.3 s if it is the transport that makes an NLOS event audible; it is labeled as diffraction rather than counted as a facade slapback.

### Specular delay, level, and direction

For a specular path, mirror the source through the planar patch sequence, intersect the finite patch polygons, and require every source→bounce→listener segment to be visible in the offline bake. Runtime delay is total path length divided by the frozen speed of sound. In an LOS proof, the expected slapback separation is `(echo_path_length - direct_path_length) / c`; in NLOS, arrival is measured from the emission epoch because no direct arrival exists.

The dry source is calibrated at one metre. The tap's pressure gain is therefore based on the engine's one-metre distance model evaluated at **total image-path length**, multiplied by the pressure reflection coefficient at each bounce. For band `b`, the starting material coefficient is `sqrt(max(0, 1 - absorption_b)) * sqrt(max(0, 1 - scattering))`, multiplied across bounces. Steam-compatible air absorption is evaluated over total path length. This gives spherical spreading once over the image distance and material loss per hop.

Applying `1/r` independently to every segment is rejected because it double-counts geometric spreading for an ideal image source. Cascading the distance shaper or air filter per segment is also rejected. Material interaction belongs per bounce; spherical spreading, air absorption, and the signed distance wash belong to the total traveled distance.

Arrival direction is listener-relative from the final bounce point, not from the original source or the image point. Each tap uses the existing Steam HRTF/binaural primitive with bilinear interpolation. It does **not** create an `IPLSource`, consume one of `MAX_ACTIVE_SOURCES`, enter the simulator, or feed itself back into reflections. Logical virtual sources are rejected because the graph already has a hard source-slot budget and recursive sends would be easy to create.

### NLOS corner approximation

NLOS energy is a separate edge path, never a synthetic direct gain. The offline baker identifies vertical building edges or authored corner portals. A candidate is valid when source→edge and edge→listener are visible while source→listener is blocked. Its path length is the two visible legs, and its arrival direction is from the edge to the listener.

V1 uses a deterministic, bounded three-band knife-edge-inspired loss rather than pretending that a full UTD solver has been implemented. The proposed starting losses are 9 dB at 250 Hz, 15 dB at 1 kHz, and 24 dB at 4 kHz, in addition to total-path spreading and air loss. Interpolation is log-frequency. These values deliberately leave audible low/mid energy and darken the corner. They are a signed creative parameter for md's listening gate; the table generator and tests must predict the configured values exactly. A later validated diffraction model may replace the coefficient generator without changing the runtime table contract.

Rejected alternatives are unoccluding or boosting the direct stage, which destroys the physical shadow; adding an undirected reverb tail, which does not read as energy rolling around a corner; and emitting a full blast from the edge with arbitrary makeup gain, which breaks source calibration and the safety model.

### Frozen impulse shaping

Every echo tap reuses the existing `ImpulseShaper` once, keyed by total echo-path distance. `ArtilleryThunder` retains exactly the current 5 m/18 kHz, 50 m/7.5 kHz, 200 m/2.8 kHz, and 500 m/1.1 kHz signed knots, coefficient construction, clamping, and makeup ([`impulse_shaping.rs:22-42`](../../crates/fightbox-steam-audio/src/impulse_shaping.rs#L22)). An echo beyond 500 m continues to use the frozen 500 m clamp until a separate kilometre-shaping decision is signed. The sidecar neither edits the knot table nor applies the shaper once per leg.

The tap processes the event's full dry stem, not merely a synthesized delta, so the character and calibrated envelope of the source survive. V1 adds no artificial scattering burst. If a later listening pass wants facade roughness, it should be a separately gated short decorrelated cluster around the analytic arrival, not hidden in the core tap.

## ε. Render-chain placement and control

The echo sidecar belongs inside the Steam Audio render graph as a dedicated bus parallel to the existing reflection bus. For an opted-in source it branches from calibrated dry `source_block.input_mono` before the backend's impulse shaper and physical propagation delay. It cannot reuse `mono_work`: that buffer has already been shaped for the **direct** distance and delayed by the **direct** source-listener path before direct, baked path, and Steam reflection stages consume it ([`multi_source.rs:1168-1215`](../../crates/fightbox-steam-audio/src/multi_source.rs#L1168)). Reusing it would apply the wrong distance key and then either double-delay an echo or force error-prone excess-delay bookkeeping.

The echo branch is:

`calibrated dry mono → unchanged total-path impulse shaper → total physical path delay → total-path air/material filters → per-tap HRTF → echo bus`

The direct/path/reflection chain remains byte-for-byte unchanged. The echo bus sums into backend stereo output beside `render_reflection_mix`, before RuntimeGraph's monitor ramp and final limiter. Runtime applies that limiter after backend rendering ([`render.rs:652-733`](../../crates/fightbox-runtime/src/render.rs#L652)); its -1 dBTP ceiling and existing 32-sample lookahead remain untouched ([`safety.rs:8-13`](../../crates/fightbox-runtime/src/safety.rs#L8)). `D_q = 0` means the sidecar inserts no common or dry-path delay. Its delayed arrivals are physical path time, not algorithmic compensation. The already frozen limiter latency does not change.

Control uses a separate block-snapshot `EchoOutputGainControl`, initialized to zero for `Off` profiles and unity only for authored profiles. Its effective gain is `StageOutputGains.reflections × echo_output_gain × governor_echo_gain`. This makes the existing “reflections muted” diagnostic control silence all late-field contributions while still allowing an echo-only capture. It avoids adding a public field to `StageOutputGains`, whose exhaustive literals are already widespread ([`lib.rs:1058-1074`](../../crates/fightbox-steam-audio/src/lib.rs#L1058)).

The application publishes a trigger-generation plan keyed by stable source index, event generation, and emission epoch. The graph resets the eligible delay history on generation change and adopts the complete plan at a block boundary. All rings, filters, and HRTF effects are allocated at session construction for enabled profiles. Nothing allocates, locks, performs geometry work, or waits on the callback.

`fightbox-runtime/src/backend.rs` remains untouched. The plan publication, table loader, render state, and controls are Steam/application-layer facilities around `multi_source.rs`. No new generic runtime trait is warranted until a second backend needs echo plans.

## ζ. Bit neutrality and safety

The sidecar is additive and default-off. Descriptor absence and explicit `EchoProfile::Off` take a structural bypass before plan reads, buffer mutation, effect application, or output accumulation. A legacy session does not allocate echo effect state. This is stronger than multiplying an always-rendered bus by zero and is the required route to preserving the existing output hash at [`multi_source.rs:3736`](../../crates/fightbox-steam-audio/src/multi_source.rs#L3736).

Enabled echoes use the same calibrated, safety-gained source samples as direct sound; they do not invent an independent SPL reference or post-limiter gain. The summed echo bus remains upstream of the final stereo-linked limiter. Telemetry records pre/post limiter peaks and engagements as it does today. A limiter that engages often in the proof scene is a failed level design even if the ceiling contains it; the gate requires finite output, no ceiling violation, and a disclosed engagement rate.

Capture manifests add the echo profile revision, table and geometry hashes, source anchor, trigger generation, selected stable path IDs, predicted delay/level/direction, delivered governor count, and echo-stage gain. Stage captures must distinguish path-only, Steam-reflections-only, echo-only, and all-stage results. Calling both Steam reflections and the sidecar “reflections” without these fields would recreate the Wave 14 ambiguity.

## η. Realtime cost and governor integration

The sidecar performs no ray simulation and has no recurring simulation tick. Table selection, visibility-state lookup, stable sorting, and plan publication happen once per trigger. The v1 target is less than 0.02 ms per trigger on the reference M4. The audio callback cost comes from one shared mono delay ring per enabled event slot, small filters, and one binaural effect per live tap.

Phase B measured direct-plus-binaural apply at roughly 0.029 ms p99 and path apply at 0.022 ms p99 ([`EXECUTION.md:257-266`](../../EXECUTION.md#L257)). Using the larger number as a conservative tap basis gives these planning estimates:

| Delivered sidecar state | Per event / global live taps | Estimated added callback cost per 128-frame block | Recurring simulation cost |
|---|---:|---:|---:|
| Full | 4 / 8 | 0.12–0.18 ms for one event; 0.24–0.36 ms at the global cap | 0.00 ms/tick |
| Reduced | 2 / 4 | 0.06–0.09 ms for one event; 0.12–0.18 ms at cap | 0.00 ms/tick |
| Minimum | 1 / 2 | 0.03–0.05 ms for one event; 0.06–0.09 ms at cap | 0.00 ms/tick |
| Source `DirectOnly` / `Virtualized`, profile Off | 0 | Structural bypass | 0.00 ms/tick |

These are not measurements. They include margin for delay reads and filters but must be replaced by an isolation matrix and a full callback soak. One ring per enabled event slot is sized at construction from the table's maximum **total** path, not merely its excess delay, and all taps use read heads into it. For scale, the corrected 452.5 m listener plus the maximum 1.2 s excess implies about 2.52 s and 0.48 MB of mono float history. The hard ceiling should remain the engine's 2,048 m propagation bound ([`motion_smoothing.rs:17-21`](../../crates/fightbox-steam-audio/src/motion_smoothing.rs#L17)), about 5.97 s and 1.15 MB per slot at 48 kHz; a table above it is rejected. Tap effects and scratch add a smaller fixed amount that the implementation must report.

Tap count belongs to `EXECUTION.md` §κ's first “reflection settings/cadence” degradation family. Full→Reduced→Minimum reduces sidecar taps in lockstep with reflection quality before path validation or alternate-path changes. Within a rung, deterministic predicted received pressure chooses survivors; a valid diffraction tap is reserved in NLOS. If the later low-priority-source step assigns `DirectOnly`, the echo send becomes zero just like the Steam reflection send, while baked path remains audible. Existing transient protection prevents a ballistic slot from being demoted during its three-second event window. `Virtualized` is fully silent.

This adds no new independent degradation order. It also rejects “keep all taps but update them less often”: an emitted one-shot plan is frozen, so cadence reduction would not save callback work. Tap count is the honest cost knob.

## θ. Falsifiable proof contract

Implementation is not accepted because it sounds vaguely more reverberant. It passes only if every item below is reported from release builds with isolated stages.

1. **Close the anomaly measurement first.** The corrected Wave 14 harness records direct-only, path-only, Steam-reflections-only, echo-only, and all-stage captures from the identical calibrated impulse. It reports probe coverage and path EQ/SH. A nonzero path snapshot must produce nonzero path-only audio; otherwise the sidecar lane stops and the engine bug is fixed first.

2. **Analytic single-facade oracle.** A finite planar facade with known material produces the expected valid image path. Arrival is within one sample of `L/c`, each tested band's received level is within 1.0 dB of the table prediction, and binaural ITD/ILD has the predicted side. Occluded or outside-polygon images produce exactly no tap.

3. **Megablock discrete-return gate.** At the corrected firing-street LOS listener, echo-only capture contains at least two table-predicted returns whose excess delays lie in 0.3–1.2 s. Detected delay is within 2 ms and broadband level within 1.5 dB of prediction; each accepted return is at least 6 dB above its local 50 ms floor. The Steam-reflections-only capture remains separately reported, never credited for a sidecar return.

4. **Corner-energy gate.** Direct-only remains exact zero at the corrected around-corner listener. A predicted diffraction arrival appears within 2 ms. Against an unobstructed equal-total-path reference, its broadband received energy is 6–18 dB lower; the 250–500 Hz band is no more than 12 dB lower, and the 2–4 kHz band is at least 6 dB below the low band. This is a product audibility/darkness band, not a claim that the provisional loss table is universally physical. Path-only energy is reported beside it so the two mechanisms cannot be conflated.

5. **Determinism.** Identical table, trigger, pose, and PCM produce bit-identical selected plan bytes and echo-only audio across repeated processes. Path ordering and governor survivor choices do not depend on hash-map order or floating scheduling. Steam's stochastic reflection capture is excluded from this bitwise claim.

6. **Legacy and `D_q` preservation.** Descriptor-absent and explicit-Off fixtures both reproduce `e43a455e6eda686dbea905e16c434474406f55bf7db6356f2d145d24137a27ef`. Direct onset and direct-only samples are unchanged when an enabled sidecar has no valid candidates. Echoes create only their predicted physical late arrivals; no common delay is inserted.

7. **Safety and performance.** Output is finite, post-limiter true peak never exceeds -1 dBTP, and limiter engagements are disclosed for the percept fixture. The isolation matrix measures 1/2/4/8 taps. The full governed callback retains `EXECUTION.md` §κ's `p99 < 1.33 ms`, `p99.9 < 2.13 ms`, and zero soak misses. Delivered tap counts and cost decrease monotonically at Full/Reduced/Minimum/Off.

8. **Failure controls.** Wrong table hash refuses the echo profile rather than falling back to guessed geometry. Removing the facade removes its specular tap. Removing the corner edge removes its diffraction tap. Muting the reflections master silences both Steam reflections and echo; echo-only still works through its dedicated diagnostic control.

The corner band, tap limits, and provisional edge losses are the perceptual clauses md must ratify. The timing, predicted-level, determinism, default-off hash, safety, and callback gates are mechanical.

## ι. Phasing and effort

**Phase 0, diagnostic correction: less than one day.** Add path-only/all-stage Wave 14 evidence and the same-coordinate continuous control. This can reveal that live event corner fill already exists. It does not alter production.

**Audible v1: 3–5 engineer-days.** Generate an analytic megablock table for the corrected muzzle and listening route. Implement one event-only profile, a trigger-generation plan, a shared delay ring, up to three specular taps plus one corner tap, unchanged distance shaping, HRTF rendering, echo solo/gain, and a capture that md can hear. Use fixture-owned facade materials and the provisional corner loss. This phase is intentionally narrow and may be discarded behind the stable table contract.

**Gate-worthy v1: 8–12 engineer-days total, including the audible v1.** Add the versioned table schema and offline generator, stable probe interpolation/freeze rules, all governor rungs, structural-off golden tests, telemetry/manifests, analytic oracle, failure controls, performance matrix, and scripted small-arms/impact eligibility. The output is suitable for md's mechanical and ear gate on the megablock.

**Full solution: 3–5 engineer-weeks after v1 acceptance.** Build and qualify generic mesh plane/edge extraction, multi-anchor bake orchestration, package caching, imported-city validation, moving source anchors, tap-set crossfades, material authoring tools, and only then experiments for loud steady sources or scattered micro-clusters. Steam baked-reflection comparison remains a parallel research item gated by the small long-bin test.

## κ. Ratification record

md should explicitly accept, modify, or reject these decisions before production work starts:

| Decision | Recommendation |
|---|---|
| NLOS anomaly | Harness gap primary; no production event-send/cadence fix yet. Run path-only cheap check first. |
| Runtime geometry | Offline precomputed probe/anchor echo tables. Analytic grid only for audible v1/oracle; production tables come from offline mesh plane/edge extraction. |
| Eligible sources | Explicit impulse-event opt-in. Blast/small arms/impacts yes; crack and steady sources no in v1. |
| Synthesis budget | Four taps/event, eight global; up to three specular plus reserved NLOS diffraction. Dedicated HRTF taps, no logical/Steam source slots. |
| NLOS voicing | Provisional 9/15/24 dB edge loss at 250 Hz/1 kHz/4 kHz and the §θ corner-energy acceptance band. |
| Chain | Dry pre-delay branch, unchanged total-path shaper once, physical delay/filter/HRTF, separate echo bus before the existing limiter; `D_q=0`. |
| Control/API | New Steam-side echo plan and gain control; echo follows reflections master. No `backend.rs` or public `StageOutputGains` shape change. |
| Governor | Full 4/8, Reduced 2/4, Minimum 1/2, DirectOnly/Virtualized Off, inside `EXECUTION.md` §κ's existing first family. |
| Budget | 3–5 days to hear; 8–12 days total for gate-worthy v1; generic/full work deferred. |

## λ. Rejected “quick fixes”

Increasing the realtime IR to 2 s is rejected: the corrected test spends about 9–10 ms per executed Full simulation tick and buys no discrete or NLOS reflection energy. Raising rays or bounces without a new controlled return is likewise unsupported. A large creative gain on the Steam reflection bus is rejected because it raises the early bed and numerical floor together. Re-enabling crack reflections violates the signed event contract and does not affect the blast. Treating baked path as a discrete echo generator confuses two percepts. Adding a generic reverb tail may improve size but cannot meet predicted facade-delay or direction gates. The sidecar should ship only if it produces the small number of explainable arrivals described above.
