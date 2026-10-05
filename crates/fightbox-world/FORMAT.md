# `.fightbox` package formats

## Version 1

A package is a directory containing exactly the three compiler-owned files
`manifest.json`, `mesh.bin`, and `materials.json`. Writers do not include
timestamps. JSON object keys and input provenance rows are sorted, making
repeated compilation byte-stable.

The manifest also records `building_count` and a deterministic `assumptions`
array. Each assumption names the building, the assumed height in metres, and
the policy reason. Older version-1 manifests without these additive fields
load with zero buildings and no assumptions.

`mesh.bin` is entirely little-endian:

1. 8 bytes: ASCII `FBXMESH` followed by NUL
2. `u32`: format version
3. `u32`: vertex count
4. `u32`: triangle count
5. vertex count rows of three `f32`: east, north, up in metres
6. triangle count rows of three `u32` vertex indices
7. triangle count `u32` material IDs

The material IDs index the name-sorted material table. `manifest.json` records
the SHA-256 of the exact `mesh.bin` and `materials.json` bytes, both mesh
counts, the complete material table, tool version, and sorted source input
paths with SHA-256 provenance. The loader verifies the two content hashes,
counts, material-table copy, and all acoustic mesh invariants.

GeoJSON prism exteriors use counter-clockwise footprint rings as viewed from
above. Roof and ground normals point up, bottoms point down, and wall normals
point away from the footprint interior.

## World package version 2

Version 2 is a strict, versioned envelope around the unchanged version-1 core
payloads. Its manifest has `format_version: 2` and
`schema_version: "fightbox.world-manifest.v2"`; the machine-readable contract
is [`world-manifest-v2.schema.json`](world-manifest-v2.schema.json). Unknown
fields in the manifest or any core world-index object are rejected. A
version-1 manifest carrying any version-2 envelope field is also rejected as a
partial hybrid. Existing version-1 manifests remain readable and the
version-1 writer does not emit version-2 fields.

`mesh.bin` and `materials.json` retain their exact version-1 encodings. The v2
world index cross-binds their schema IDs, package-relative paths, byte sizes,
and SHA-256 hashes to the hashes already present in the manifest root. This
makes an envelope-only migration possible without a blanket geometry or
material rebake.

Every v2 package persists:

- a stable lowercase city ID and an authoritative WGS84 latitude, longitude,
  and altitude origin using the declared `wgs84_ecef_enu` local tangent frame;
- a signed east/north grid index and canonical cell ID
  `<city-id>:e<east-index>:n<north-index>`;
- a cell-local-to-city-ENU translation derived from the 485 m grid stride,
  local ENU bounds, sorted neighbors and switch planes, and sorted route IDs;
- the frozen mobile-cell policy: 585 m probe footprint, 485 m stride, 100 m
  pairwise overlap, 50 m ownership guard, 600 m geometry halo and baked-path
  horizon, at most one prepared neighbor, and at most two resident worlds;
- the 48 MiB target and 64 MiB hard raw probe-payload limits. These are package
  capability limits, not definitions of a probe payload format.

Extensions are typed, hash-indexed sidecar references, sorted by capability and
path. A reference declares a versioned capability ID ending in `.vN`, whether
it is optional or required, a normalized relative path, exact raw and stored
content hashes and sizes, and `none` or `zstd` compression. The loader always
checks sidecar containment, stored size, and stored hash. For negotiated
capabilities it also performs bounded decompression, then checks raw size and
raw hash; unknown optional capabilities need not be expanded merely to report
them. The combined raw size of opaque
`fightbox.steam-audio.probe-batch.v1` sidecars may not exceed the frozen 64 MiB
mobile limit. Unsupported optional capabilities are retained and reported;
unsupported required capabilities fail loading. Adding an unrelated optional
capability therefore does not change either core payload. The envelope does not
define or reinterpret the bytes of any probe, echo, or other capability
sidecar.

The v2 writer sorts provenance, assumptions, neighbors, routes, and extension
references and emits no timestamps. Repeating a compile with identical source,
origin, cell identity, metadata, and sidecars is byte-stable. `fightbox city
compile` remains the version-1 command; `fightbox city compile-v2` requires an
explicit city ID and WGS84 origin.
