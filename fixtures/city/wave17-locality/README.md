# Wave 17 locality candidate fixture

This deterministic synthetic WGS84 city and global graded-probe policy are the
small, reproducible input for Wave 17 package-plan candidates. They are not
listening evidence and do not substitute for the linked Steam Audio bakes.

The policy contains the frozen 4 m owner-home, 8 m route-core, 16 m transition,
and 32 m residual tiers on one global lattice. `city compile-v2 --probe-policy`
halo-slices each package to the 585 m listener footprint plus the 600 m
geometry/material halo and indexes the exact canonical `fightbox.city-bake.v2`
sidecar.

Build all 23 cell packages, the 21-cell/10.285 km footprint-union route, and the
four-cell seam/1.17 km monolithic-oracle plan outside the repository:

```sh
scripts/wave17-build-locality-candidates.sh \
  /path/to/spatial-audio/evidence/wave17-locality-candidates-v1
```

`candidate-summary.json` records the source, policy, resolved-layout, route, and
four-cell manifest hashes. The initial manifests deliberately report
`bakes_launched: false`. On a linked-SDK host, bake and rebind the four streamed
cells without replacing prior artifacts:

```sh
scripts/wave17-bake-four-cell-candidate.sh \
  /path/to/spatial-audio/evidence/wave17-locality-candidates-v1
```

That command writes `streamed-bake-summary.json` and advances the fixture state
to `streamed_cell_bakes_complete_oracle_pending`. Build the separate desktop
oracle from the same canonical policy and completed cell identities:

```sh
scripts/wave17-bake-monolithic-oracle.sh \
  /path/to/spatial-audio/evidence/wave17-locality-candidates-v1
```

The oracle command uses the fixture's exact 1.17 km bounds, retains whole source
features through the additional 600 m geometry halo, resolves one global probe
sequence without assigning mobile-cell semantics, reserves its 1,750 m pair
estimate, and cross-binds the route, policy, layout, package, SDK and serialized
batch. `city oracle-verify` revalidates those bindings without SDK work.
Completion is oracle-bake evidence only; promotion still requires the streamed
versus oracle seam traversals, aperture evidence, and device qualification.

Newly produced `city bake-v2` and oracle roots also contain the additive strict
`probe-byte-estimate-v2.json` envelope. It is a root evidence envelope, not a
package extension. Route assembly requires and verifies it for every new completed production
bake, includes it in installed-size accounting, and binds its digest and size
into the completed route record. New routes emit `production_eligible: true`;
missing/false means evidence-only and the default Swift production resolver
rejects it. There is no production-route legacy admission flag. The four frozen
pre-envelope bakes remain readable through their retained evidence route with
an explicit evidence-only resolver, and through legacy-oracle verification;
they cannot be reserialized as new production authority. Mobile plans whose projected high exceeds 64 MiB
are rejected by the world route library, CLI, and Swift artifact resolver. The
Swift resolver also requires exact package/bake file closure and verifies the
bound envelope before descriptor construction.

Estimator calibration that exceeds the phone budget must use the separate
`city oracle-bake` lane. Use `--expected-probe-count <n>` to pin the resolved
count before SDK work. If its policy differs from the source route, also require
`--policy-independent-calibration`; the output then carries canonical
`oracle-calibration-only.json` authority with `production_eligible: false` and
both policy hashes. Always follow it with `city oracle-verify`. The retained
exact 10,001-probe diagnostic lives at
`/path/to/spatial-audio/evidence/wave17-probe-byte-estimate-v2-oracle-calibration-20260811T114700Z/oracle-10001-calibration-only`.
It remains desktop/offline evidence: its provisional ±30% estimator band
failed and no coefficient promotion is claimed.

Run the reproducible 0.5 m/15 Hz offline seam harness only against an
estimator-bound production route and the separately retained desktop oracle:

```sh
python3 scripts/wave17-seam-traverse.py \
  --candidate /path/to/spatial-audio/evidence/wave17-locality-production-estimator-v2-20260811T122800Z \
  --oracle-root /path/to/spatial-audio/evidence/wave17-locality-candidates-v1 \
  --output /path/to/spatial-audio/evidence/wave17-seam-$(date -u +%Y%m%dT%H%M%SZ)
```

The command succeeding means the evidence bundle completed, not that the
acoustic gate passed. `route-mechanical.json`, `route-host-consumer.json`, and
`prediction-fallback.json` are mechanical authority; `acoustic-offline.json`
records the separate `offline_level_gate_passed` result. Fresh-process cell
renders never substitute for a live production swap/crossfade, terminal tail,
device, or listening proof.


The canonical exact-frame host run is `/path/to/spatial-audio/evidence/wave17-seam-exact-frame-stitch-20260811T125850Z` (top-level report
SHA-256 `1779a206b655dcbaea44da471c910c31de3f8611b8cb567b4708fe39492f84b9`). Every cell is rendered from global frame zero over
the complete translated 53.333-second route, then only its half-open owned interval is
measured. This preserves exact seek/source frame identity and compares all 801 samples
without asymmetric startup exclusions. The prior segmented `...T1256Z` run restarted
the 4,800-frame deterministic pink asset at each owner run; its reported 9.245 dB
failure compared different source phases and is superseded as a harness defect, not
relabelled acoustic evidence.

The corrected linked-SDK comparison passes the unchanged 1 dB offline gate: maximum
0.0123712422 dB and p95 0.0011120134 dB. A hard stitch of exact-frame owner outputs has
3.726996e-5 maximum PCM error against the oracle; extra discontinuity at the three
switch frames is at most 1.164154e-9. This remains offline artifact-specific evidence.
It does not by itself prove the production callback crossfade, a seekable in-flight
event or retiring tail, iOS/device/AirPods behavior, or human listening quality.


## Calibrated production route and linked callback checkpoint

The implemented fixed-model route superseding the provisional production candidate is
`/path/to/spatial-audio/evidence/wave17-locality-production-fixed-tier-mesh-v2-20260811T134214Z`
(route SHA-256 `14ef017413b52cfbd25661b81aef9d3b8b505e850a339b3721352dea21313f2a`). It was built only with explicit
`--probe-byte-model wave17-fixed-tier-mesh-open-pairs-v2`; the CLI default remains the old revision.

The route's `linked-callback-report/report.json` (SHA-256
`17eaebe1566d9ec2783f898444d1cb5d199396e312264d89ff5d90e63374302f`) is source/executable-bound desktop
linked evidence over all four actual artifacts. It passes the unchanged 1 dB comparison at
`0.7403142505140252 dB` maximum with exactly three bounded swaps and terminal tails. This result does
not substitute for the still-missing admitted soak, iPhone/AirPods/thermal run, or human listening.
