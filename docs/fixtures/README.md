# Controlled fixtures

`fixtures/fixture.schema.json` is the strict Draft 2020-12 contract for the current controlled fixtures. JSON uses only owned scalar, array, and object values so a Rust `serde` consumer can deserialize it without vendor handles or inferred defaults.

`s0-free-field/fixture.json` is the 100 m-to-1 m free-field approach. Its source uses `SplAtOneMeter` and binds its asset descriptor via `asset_id` (`s0-calibrated-pink`), so its declared level is tied to the asset's delivered program RMS that the scene-owned source drive scales. It tests spreading ordering and continuity separately from the enabled-versus-disabled air-absorption comparison at the same distance.

`s3-corner/fixture.json` is the convex exterior masonry corner accepted by ADR 0003. Double-sided six-metre façades extend east and north from the shared edge. The initial source/listener segment crosses both façades, while the source and listener remain outside on adjacent sides of the building corner. Two upward-wound horizontal floor triangles cover the declared probe box, source, and listener, which is required for Steam Audio `UNIFORMFLOOR` probe generation. The quarter-metre-offset one-metre lattice keeps probes off both façade planes. The fixture specifies the 1.5 m probe height, path-bake and fresh-process reload requirements, configured `direct`, `path`, `reflections` processing order, required diagnostic stems, and the analytic listener-to-edge azimuth of `303.690068°`.

Run from the repository root:

```sh
jq empty fixtures/fixture.schema.json fixtures/s0-free-field/fixture.json fixtures/s3-corner/fixture.json
python3 fixtures/validate.py
git diff --check
```

Use a Draft 2020-12 JSON Schema implementation as an additional structural check when one is available. `fixtures/validate.py` deliberately has no third-party dependency. It checks the controlled fixture identities and gates, finite numeric values, mesh indices/material references/nondegenerate triangles, and S0's strictly decreasing approach distances. For S3 it enforces the exact ADR 0003 vertices and ten masonry triangles, source and listener outside adjacent façades, line-of-sight intersections with both façades, upward floor and probe coverage, the 18-by-18 offset lattice with no probe on either façade plane, analytic edge/vector/azimuth, 64-sample direct occlusion, validated pathing with alternates, and the existing bake/reload and stem contracts. It also resolves each source's `asset_id` to its descriptor under `fixtures/assets`, validates that descriptor, and checks that its delivered `target_rms_dbfs` is a finite level strictly below 0 dBFS coherent with the source's `SplAtOneMeter` declaration (ADR 0002's one gain chain needs a deliverable program RMS). On success it prints one JSON summary line for automation.

Semantic validation does not generate probes, call `iplPathBakerBake`, serialize or reload a probe batch, render audio, prove finite/nontrivial path output, or establish an audible result. These documents define reproducible inputs and required evidence; S3 still requires the captured stems, pathing-on/off comparison, and a headphone listening record.
