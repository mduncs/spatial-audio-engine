# ADR 0003: Correct S3 to a convex exterior building corner

- Status: accepted
- Date: 2026-07-29

## Context

The original S3 fixture put the source in the concave southwest side of two
wall arms extending west and south from `(0, 0)`, with the listener northeast.
A physical path between them had to go around the remote end of one arm.
However, both the floor and uniform-floor probe coverage stopped one metre
before those ends. The claimed route around the shared `(0, 0)` endpoint was
not a valid two-dimensional path through that topology.

The matrix retained in
`docs/diagnostics/phase-a-validated-path.md` and the executable
`legacy_concave_s3_corner_exposes_validation_limit_and_unvalidated_path_moment`
regression establish the failure. The exact legacy fixture produced 317
probes, all-zero validated SH, and five rejected segments among 15 validation
callback segments. Changes to spacing, visibility range, visibility sampling,
endpoint order, and a symmetric probe-volume inset did not create a validated
path.

One asymmetric lattice alignment emitted a nonzero path by passing through the
shared mathematical endpoint. Nearby alignments did not. This floating-point
corner leak is not a physical route and is rejected. Disabling validation, or
leaving validation enabled while disabling alternate lookup, also emitted a
nonzero rejected path. Neither result is acceptance evidence.

## Decision

The canonical `s3-masonry-building-corner` fixture is corrected to a convex
exterior corner:

- double-sided, six-metre-high façades run east from `(0, 0)` to `(10, 0)` and
  north from `(0, 0)` to `(0, 10)`;
- two upward-wound triangles form a floor spanning `[-9, 9]` on both horizontal
  axes, for exactly ten masonry triangles total;
- the source is `(-4, 6, 1.5)` and the listener is `(6, -4, 1.5)`;
- the uniform-floor probe box is `(-8.75, -8.75, 0.5)` through
  `(8.25, 8.25, 2.5)`, with one-metre spacing and 1.5-metre height;
- path validation and alternate lookup remain enabled, pathing order remains
  two, and direct occlusion remains 64 samples;
- the analytic edge is `(0, 0, 1.5)`. The listener-to-edge vector is
  `(-6, 4, 0)`, whose clockwise-from-north azimuth is `303.690068°` with a
  `15°` tolerance.

The quarter-metre lattice offset keeps probes off both façade planes while
covering both sides of the convex exterior edge. The source-to-listener segment
crosses the north-running façade at `(0, 2, 1.5)` and the east-running façade
at `(2, 0, 1.5)`.

The exact candidate was run against the verified Steam Audio 4.8.1 SDK before
acceptance. It produced 324 probes, direct occlusion `0`, SH0 `0.019228581`,
11 validation callback segments with zero rejected, and a decoded arrival
azimuth of `299.36618°`. The delta from the analytic `303.69006°` direction
was `4.32388°`.

## Consequences

This is a controlled-fixture correction. It does not change the Steam Audio
pin, backend architecture, coordinate mapping, bake and reload sequence, stem
contract, or listening requirements.

Semantic validation now fixes the exact façade, floor, endpoint, probe-lattice,
occlusion, and analytic-direction relationships. The linked backend regression
uses an exact executable copy of this fixture rather than its separate
convenience mesh.

The corrected mechanical path result does not itself pass S3. S3 still requires
the captured stems, pathing-on/off metric, provenance, and a completed
provisional headphone listening record.
