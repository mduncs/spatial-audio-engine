# ADR 0002: Calibrate source PCM with one scene-owned gain chain

- Status: accepted
- Date: 2026-07-29

## Context

Source level needs one deterministic relationship to the PCM that enters propagation. Recording a
generator's normalization gain beside a separate physical source gain leaves two controls that can
both appear to define loudness. It also allows predicted level, meters, safety telemetry, and
delivered samples to disagree.

Asset analysis and scene calibration are different facts. Asset analysis describes decoded,
pre-drive program PCM. Scene calibration defines the digital mapping used to represent a declared
source SPL.

## Decision

The scene owns an affine SPL-to-PCM anchor:

- reference level `Lref = 120 dB SPL`;
- reference source PCM `Pref = -24 dBFS RMS`;
- reference distance `rref = 1 m` exactly.

For a source declared as `SplAtOneMeter(L)`, the target pre-propagation source RMS is:

```text
Ptarget = Pref + (L - Lref)
Gdrive  = Ptarget - Pprogram
```

`Pprogram` is the decoded, pre-drive asset program RMS. The engine scales the decoded PCM exactly
once by `10^(Gdrive / 20)` before the direct, path, and reflection branches. The source meter reads
from that same post-drive chain; there is no independently supplied meter or source-loudness gain.
The inverse mapping is:

```text
L = Lref + (Psource - Pref)
```

Free-field inverse-distance prediction relative to the one-metre declaration uses:

```text
Delta(r) = -20 log10(r / 1 m)
L(r)     = L + Delta(r)
```

Thus a 100 m distance contributes approximately `-40 dB`. A `+6 dB` source-level change produces
`+6 dB` changes in target source RMS, derived drive, and source meter.

`CreativeDb(C)` is the other source-level mode. It sets `Gdrive = C` and
`Ptarget = Pprogram + C`. This is deterministic relative PCM gain only. It has no predicted SPL,
physical reach, or audibility-threshold meaning.

Every `AssetAnalysis` records:

- decoded, pre-drive full-program RMS in dBFS;
- decoded, pre-drive true peak in dBTP;
- caller-owned measurement provenance whose method identifier states the program-RMS window and
  channel aggregation rule and the true-peak method, with analyzer/version identity where
  applicable.

The API validates the supplied analysis but does not claim to perform it. Silence and non-finite
values are rejected, and program RMS must not exceed true peak.

## Rejected anchor

The alternative `85 dB SPL at 1 m = -24 dBFS RMS` was rejected as a monitor-style anchor with
insufficient outdoor source headroom. Under that mapping, a 120 dB source would target
`+11 dBFS RMS`, before accounting for crest factor. It cannot represent the intended outdoor
source range without immediate digital overload or another hidden gain. With the chosen anchor,
85 dB maps to `-59 dBFS RMS`; an asset measured at `-20 dBFS RMS` therefore receives a derived
`-39 dB` drive.

## Separation from peak safety and monitoring

True peak does not alter the RMS/SPL mapping. It predicts post-drive peak as
`TPpost = TPprogram + Gdrive` so the runtime can report headroom and feed downstream safety policy.
Source extent, the monotone safety stage, and the final true-peak limiter remain separate from
source calibration.

Monitor gain and the output-device or headphone transfer function are downstream controls. They
do not change scene source power, propagation decisions, or the source meter. A delivered-ear-SPL
claim requires a measured output transfer and is outside this calibration.

## Consequences and non-claims

`SplAtOneMeter` supports scene-SPL prediction because its declaration, asset measurements, applied
PCM gain, and meters share one chain. Callers cannot add a second arbitrary source-loudness gain.
Explicit creative sends remain separately named mix controls and cannot silently redefine a
physical source.

This decision does not claim delivered-ear SPL, hearing safety, output-device calibration,
real-world audibility for `CreativeDb`, or physical accuracy beyond the propagation model and
evidence attached to a capture.
