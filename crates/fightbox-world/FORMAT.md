# `.fightbox` package format version 1

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
