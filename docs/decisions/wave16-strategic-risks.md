# Wave 16 strategic risks — decisions to make early, before they get expensive

Written 2026-08-03 by the orchestrator, ratification pending md. Context: the city scene is about
to grow; five wave16 lanes are in flight (capacity 8→16 + governor loop, fast-mover WP1/WP2, and
plan lanes for reflection budgets, probe economics, echo geometry authority). Each risk below is a
step we are on course to take whose *ordering or format choice* becomes expensive to reverse later.
Each ends with the early decision that defuses it.

## R1. The backend.rs freeze is accumulating a queue behind it

The freeze has been correct discipline, but three known wants now stack behind it: the capacity
raise (if the 8-slot fixed arrays are genuinely mirrored there), stereo-width v2 / StereoImage
rendering (explicitly blocked on an md unfreeze), and any future limiter-adjacent work. Unfreezing
piecemeal — once per want — multiplies re-ratification cost, because every unfreeze demands the
same heavy gate (golden re-proof, §λ safety re-listen, regression sweep).

**Early decision:** treat backend.rs as ONE planned unfreeze wave with a manifest. Collect every
change that needs entry (cap arrays if capacity lane reports so; stereo v2 seams; nothing else
without listing here), land them in a single gated window with one ear-ratification session, then
re-freeze. Do not grant one-off exceptions between now and that wave.

## R2. One bit-identical golden is becoming a bottleneck contract

The single frozen fingerprint (1-source, unbaked, static, direct-only) is what lets lanes move
fast — but every render-chain wave now carries an "if it trips, STOP" clause, and the fingerprint
proves nothing about baked paths, moving sources, multi-source mixes, or the sidecar. As waves land
(fast-mover changes moving-source output by design; capacity touches array layout), we rely on an
ever-narrower slice of accidental coverage.

**Early decision:** grow a golden FAMILY, not more freezes of convenience: (a) keep the existing
hash forever as-is; (b) after fast-mover lands and md ear-ratifies, pin a moving-source fingerprint;
(c) after capacity lands, pin an 8-source mix fingerprint; (d) document a re-ratification protocol
(md listens, then the new hash is frozen with the same severity). A lane should never negotiate
with a golden — either it's bit-identical or the wave stops for ratification.

## R3. Scene growth vs bake economics — order of operations

Growing the map before corridor/graded probe placement lands means baking the new map uniformly
(unaffordable at scale, per the P² cost curve) or baking it twice (once uniform now, again when
graded placement exists). Both waste days. Same logic for the echo geometry authority: content
built on the analytic 6×6 oracle stops echoing the moment the map isn't the analytic grid.

**Early decision:** sequence scene growth AFTER (1) the probe-economics plan is ratified and its
placement tooling exists, and (2) the echo authority table format exists — even in megablock-parity
form. Growing geometry is the LAST step of the scaling wave, not the first. If md wants new
geometry sooner for creative reasons, declare the interim bake disposable up front.

## R4. Filling 16 slots before per-source reflection budgets exist locks in worse sound

The capacity raise makes 16 sources *admittable*; the 4-source corner-envelope failure (wave13
gate: 15.45 dB shadow mean vs ratified band) is evidence that quality already degrades with source
count under the shared reflection tier. Content authored to 16 shared-tier slots will be mixed and
ear-tuned against the degraded sound, then re-tuning lands when budgets arrive.

**Early decision:** hold scene content at ≤8 sources until the reflection-budget wave lands (or is
consciously waived), even after the cap raise merges. The raise is infrastructure, not an
invitation. Re-run the wave13 corner gate at 4 and 8 sources as part of the budget wave's
acceptance, not later.

## R5. Content authored against baked-Doppler recordings becomes debt when WP1 lands

The A-10 (and the two-street timing design) is built on pre-Dopplered recordings with the engine
adding physical delay on top of a static sky proxy — a deliberate workaround for the anchor bug.
Once WP1/WP2 make engine-computed fast movers viable, every baked-Doppler asset is a fork: keep the
recording (double-Doppler risk if the source ever actually moves) or re-author as dry loop + engine
motion (better, but re-does the scene's load-bearing timing math).

**Early decision:** classify each moving-source asset NOW as either `recording-carries-motion`
(source stays a static proxy forever — the engine must never move it) or `engine-motion candidate`
(plan re-authoring). Record the classification in the asset descriptors (a boolean or enum field)
so no future lane guesses. The A-10 pair is the test case; decide it when WP1's report lands.

## R6. The mono asset chain is calcifying

Every new asset, calibration record, and descriptor added today deepens the mono-only assumption
(decoded_mono, input_mono, mono preflight). Stereo ingestion (city ambience beds are inherently
stereo) is already blocked on the backend unfreeze (R1); the *strategic* risk is separate: content
volume. Fifty mono assets are convertible; two hundred, plus their ear-ratified levels, are a
migration project.

**Early decision:** before the next big content-authoring push, decide whether stereo ingestion is
in or out of the scaling wave. If in, land the descriptor-side schema (stereo wav + mono-compat
preflight fields) BEFORE the content push, even while rendering stays mono, so assets arrive
future-proof. Descriptor schema changes are cheap now, migrations later are not.

## R7. Versioned-format decisions about to be made under deadline pressure

Two formats are being designed right now in plan lanes: the echo tap-table schema and the graded
probe-placement config/manifest. Both will be consumed by verify/world-payload hashing, both will
need extension (moving sources / per-cell taps; new density tiers / route bakes). A v1 schema
frozen by hash discipline but designed without extension points is the classic trap — the wave15
anomaly cache is already at schema v3 after two days.

**Early decision:** both plan docs must show their v2/v3 extension story (unknown-field policy,
version negotiation, what a format break costs at each consumer) before ratification. Reject any
schema whose extension answer is "bump the version and rebake everything" unless the rebake cost
is stated and accepted.

## R8. EXECUTION.md is outgrowing its consumers

~197 KB and growing ~10 KB per window. It is the state of record and every lane is told to read
sections of it; lanes now burn meaningful context on it, and precision suffers when briefs say
"~lines 2562-2570" into a moving file.

**Early decision:** keep EXECUTION.md as the append-only journal, but start a short FRONT-MATTER
index (current freezes, current caps, live lane protocol, pointers by topic) capped at ~2 KB, and
have briefs cite the index, not line numbers. Adopt at the next window append; no rewrite of
history (md converses before any restructure — this is an addition, not a reorganization).

## Sequencing view (what defuses what)

1. Plan-lane ratifications (budgets R4, probes R3, echo R7) — md reads three docs, decides.
2. Fast-mover landing → R5 asset classification decided on the A-10 case.
3. Capacity landing → R1 manifest gets its first real entry (or is proven unnecessary).
4. Budget wave → R4 gate re-run at 4/8 sources.
5. backend.rs unfreeze wave (R1) — one window, one ear session: cap arrays (if needed) + stereo
   descriptor schema (R6) + anything manifested.
6. Probe tooling + echo authority land → THEN grow the city (R3).
